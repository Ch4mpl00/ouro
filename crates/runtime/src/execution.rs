//! Run driver: 1. configuration and the runtime; 2. admission (definitions,
//! versions, pinned bindings and schemas); 3. bounded execution,
//! shutdown/recovery; 4. the interpreter's host: functions, processes,
//! events, emitted events, child runs, secrets and resources. A process stop
//! releases work; explicit cancellation is a terminal storage transaction.
//! Lease loss always stops forward progress.
use crate::{
    FunctionContext, Registry,
    capabilities::{Capabilities, EventSink, ProcessRunner, ResourceResolver, SecretResolver},
    telemetry::Spans,
    testing::{Mode, RecordedCall},
};
use async_trait::async_trait;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use opentelemetry::{Context, global::BoxedTracer, trace::FutureExt};
use ouro_durable::{Admission, ChildLink, Clock, Definition, Fence, Lease, RunStore, SavedRun, SystemClock};
use owf::{
    durable::CallFailure,
    model::{self, Process, Workflow},
    run::{self, CallInfo, Child, ChildTarget, Effect, Functions, Host, Limits, Status, WorkflowError},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct Config {
    pub max_concurrency: usize,
    pub max_function_concurrency: usize,
    pub breaker_threshold: u32,
    pub poll_interval: Duration,
    pub heartbeat_interval: Duration,
    pub retention_ms: i64,
    pub limits: Limits,
    /// `$runtime.metadata`: implementation-specific facts the host exposes
    /// to every expression (a tenant, feature flags).
    pub metadata: Value,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_function_concurrency: 16,
            breaker_threshold: 3,
            poll_interval: Duration::from_millis(100),
            heartbeat_interval: Duration::from_millis(ouro_durable::HEARTBEAT_MS),
            retention_ms: ouro_durable::RECORDS_KEPT_MS,
            limits: Limits::default(),
            metadata: json!({}),
        }
    }
}

struct ExecutionGuard<'a> {
    executions: &'a Mutex<BTreeMap<(String, u64), CancellationToken>>,
    key: (String, u64),
    token: CancellationToken,
}
impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        self.token.cancel();
        self.executions.lock().unwrap().remove(&self.key);
    }
}

pub struct Runtime {
    pub(crate) store: Arc<dyn RunStore>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) definition_source: Arc<dyn RunStore>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) tracer: Arc<BoxedTracer>,
    pub(crate) config: Config,
    pub(crate) capabilities: Capabilities,
    permits: Semaphore,
    function_permits: Arc<Semaphore>,
    executions: Mutex<BTreeMap<(String, u64), CancellationToken>>,
}
impl Runtime {
    /// Dependencies are built once by the embedding host. No exporter or
    /// network client is installed by the runtime.
    pub fn new(
        store: Arc<dyn RunStore>,
        registry: Arc<Registry>,
        config: Config,
        clock: Arc<dyn Clock>,
        tracer: BoxedTracer,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.max_concurrency > 0 && config.max_function_concurrency > 0,
            "concurrency must be positive"
        );
        anyhow::ensure!(config.breaker_threshold > 0 && config.retention_ms >= 0, "invalid breaker or retention");
        anyhow::ensure!(
            !config.poll_interval.is_zero()
                && !config.heartbeat_interval.is_zero()
                && config.heartbeat_interval < Duration::from_millis(u64::try_from(ouro_durable::LEASE_MS)?),
            "invalid polling/heartbeat interval"
        );
        Ok(Self {
            definition_source: store.clone(),
            store,
            registry,
            clock,
            tracer: Arc::new(tracer),
            capabilities: Capabilities::default(),
            permits: Semaphore::new(config.max_concurrency),
            function_permits: Arc::new(Semaphore::new(config.max_function_concurrency)),
            config,
            executions: Mutex::new(BTreeMap::new()),
        })
    }
    pub fn embedded(path: impl AsRef<std::path::Path>, registry: Registry) -> anyhow::Result<Self> {
        Self::new(
            Arc::new(ouro_durable::sqlite::open(path)?),
            Arc::new(registry),
            Config::default(),
            Arc::new(SystemClock),
            opentelemetry::global::tracer("ouro-runtime"),
        )
    }
    /// Runs `run: container | script | shell` with this runner.
    pub fn with_process_runner(mut self, runner: Arc<dyn ProcessRunner>) -> Self {
        self.capabilities.processes = Some(runner);
        self
    }
    /// Resolves `use.secrets` with this store.
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretResolver>) -> Self {
        self.capabilities.secrets = Some(secrets);
        self
    }
    /// Resolves external schemas (`schema.resource`) with this resolver.
    pub fn with_resources(mut self, resources: Arc<dyn ResourceResolver>) -> Self {
        self.capabilities.resources = Some(resources);
        self
    }
    /// Hands every emitted event to this sink as well.
    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.capabilities.sink = Some(sink);
        self
    }
    /// Parses and validates a definition against the registry and this
    /// runtime's capabilities; external schemas must resolve.
    pub fn validate(&self, yaml: &str) -> anyhow::Result<Workflow> {
        self.admitter().validate(yaml)
    }
    pub fn save(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        self.validate(yaml)?;
        self.store.save_definition(name, yaml)
    }
    pub fn activate(&self, name: &str, version: u64) -> anyhow::Result<()> {
        let definitions = self.store.definitions(name)?;
        let d = definitions.versions.get(&version).ok_or_else(|| anyhow::anyhow!("unknown definition version"))?;
        self.validate(&d.yaml)?;
        self.store.activate(name, version)
    }
    pub(crate) fn admitter(&self) -> Admitter {
        Admitter {
            definitions: self.definition_source.clone(),
            registry: self.registry.clone(),
            capabilities: self.capabilities.clone(),
            metadata: self.config.metadata.clone(),
        }
    }
    pub(crate) fn admission(&self, id: &str, name: &str, input: Value) -> anyhow::Result<Admission> {
        self.admitter().active(id, name, input)
    }
    /// Persist a run before any execution. `drive` executes it immediately;
    /// `worker` also picks it up, including after the embedding process restarts.
    pub fn start(&self, id: &str, name: &str, input: Value) -> anyhow::Result<SavedRun> {
        if let Some(saved) = self.store.get(id)? {
            anyhow::ensure!(saved.admission.name == name, "run belongs to another definition");
            return Ok(saved);
        }
        let a = self.admission(id, name, input)?;
        self.store.admit(&a, self.clock.now_ms())?;
        self.store.get(id)?.ok_or_else(|| anyhow::anyhow!("admitted run disappeared"))
    }
    pub fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>> {
        self.store.get(id)
    }
    pub fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        self.store.list(name, prefix)
    }
    pub fn definitions(&self, name: &str) -> anyhow::Result<ouro_durable::Definitions> {
        self.store.definitions(name)
    }
    pub fn records(&self, id: &str) -> anyhow::Result<BTreeMap<String, owf::durable::Record>> {
        Ok(self.store.inspect(id)?.ok_or_else(|| anyhow::anyhow!("unknown run"))?.records)
    }
    pub fn deliver(&self, type_: &str, data: Value) -> anyhow::Result<Option<String>> {
        self.store.deliver(type_, &data, self.clock.now_ms())
    }
    pub fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        let changed = self.store.cancel(id, self.clock.now_ms())?;
        // A parent cancellation also fences/cancels descendants. Local child
        // handlers see it immediately; remote workers see it at renewal.
        let executions = self.executions.lock().unwrap();
        for ((id, _), token) in executions.iter() {
            if self.store.get(id)?.is_some_and(|r| r.ended_ms.is_some()) {
                token.cancel();
            }
        }
        Ok(changed)
    }
    pub fn start_child(
        &self,
        parent: &Fence,
        key: &str,
        id: &str,
        name: &str,
        input: Value,
    ) -> anyhow::Result<SavedRun> {
        let a = if let Some(saved) = self.store.get(id)? {
            anyhow::ensure!(saved.admission.name == name, "child definition changed");
            saved.admission
        } else {
            self.admission(id, name, input)?
        };
        let link = ChildLink { parent: parent.run_id.clone(), key: key.into(), reference: key.into() };
        if let Some(f) = self.store.start_child(parent, &a, &link, self.clock.now_ms())? {
            self.store.release(&f, self.clock.now_ms())?;
        }
        self.store.get(id)?.ok_or_else(|| anyhow::anyhow!("child disappeared"))
    }
    pub async fn drive(&self, id: &str, stop: &CancellationToken) -> anyhow::Result<Option<Status>> {
        self.drive_mode(id, stop, Mode::Live).await
    }
    pub(crate) async fn drive_mode(
        &self,
        id: &str,
        stop: &CancellationToken,
        mode: Mode,
    ) -> anyhow::Result<Option<Status>> {
        let _permit = tokio::select! {biased; _=stop.cancelled()=>return Ok(None),p=self.permits.acquire()=>p?};
        let Some(fence) = self.store.claim(id, self.clock.now_ms())? else {
            return Ok(self.store.get(id)?.and_then(|r| r.outcome));
        };
        let saved = self.store.get(id)?.ok_or_else(|| anyhow::anyhow!("claimed run disappeared"))?;
        let lease = Arc::new(Lease { store: self.store.clone(), fence, clock: self.clock.clone() });
        let local_cancel = CancellationToken::new();
        let execution_key = (id.to_string(), lease.fence.generation);
        self.executions.lock().unwrap().insert(execution_key.clone(), local_cancel.clone());
        let _execution_guard =
            ExecutionGuard { executions: &self.executions, key: execution_key, token: local_cancel.clone() };
        let spans = Arc::new(Spans::new(self.tracer.clone(), &Context::current(), &saved.admission));
        let result = self.execute(&saved, lease.clone(), spans.clone(), &local_cancel, stop, mode).await;
        let label = match &result {
            Ok(Status::Completed(_)) => "completed",
            Ok(Status::Faulted(_)) => "faulted",
            Ok(Status::Cancelled) => "cancelled",
            Ok(Status::Waiting { .. }) => "waiting",
            _ => "interrupted",
        };
        spans.finish(label);
        result.map(Some)
    }
    async fn execute(
        &self,
        saved: &SavedRun,
        lease: Arc<Lease>,
        spans: Arc<Spans>,
        local_cancel: &CancellationToken,
        stop: &CancellationToken,
        mode: Mode,
    ) -> anyhow::Result<Status> {
        let checked = self.registry.validate_with(&saved.admission.definition, &self.capabilities).and_then(|wf| {
            anyhow::ensure!(
                saved.admission.runtime["bindings"] == self.registry.bindings(&wf),
                "function bindings changed since admission"
            );
            Ok(wf)
        });
        let wf = match checked {
            Ok(wf) => wf,
            Err(e) => {
                return self.store.finish(
                    &lease.fence,
                    &Status::Interrupted(WorkflowError::validation(e.to_string())),
                    self.clock.now_ms(),
                    self.config.breaker_threshold,
                );
            }
        };
        let mut limits = self.config.limits.clone();
        if matches!(mode, Mode::Dry { .. }) {
            limits.max_sleep = Some(Duration::ZERO);
            limits.max_listen = Some(Duration::ZERO);
        }
        let runtime = &saved.admission.runtime;
        let host = Host {
            functions: Arc::new(ExecutionFunctions {
                registry: self.registry.clone(),
                admitter: self.admitter(),
                schemas: Pinned(
                    runtime["schemas"].as_object().map(|m| m.clone().into_iter().collect()).unwrap_or_default(),
                ),
                lease: lease.clone(),
                spans: spans.clone(),
                function_permits: self.function_permits.clone(),
                mode,
            }),
            observer: spans.clone(),
            clock: Arc::new(InterpreterClock(self.clock.clone())),
            limits,
            // `$runtime`: the descriptor the DSL defines, without the pins.
            runtime: json!({
                "name": runtime["name"],
                "version": runtime["version"],
                "metadata": runtime.get("metadata").cloned().unwrap_or_else(|| json!({})),
            }),
        };
        let run = run::resume(&host, &wf, &saved.admission.id, saved.admission.input.clone(), local_cancel, &*lease)
            .with_context(spans.root());
        tokio::pin!(run);
        let mut heartbeat = tokio::time::interval(self.config.heartbeat_interval);
        loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=> {
                    local_cancel.cancel();
                    // Dropping the interpreter closes in-flight spans and
                    // handlers. Intent already stored stays outcome-unknown.
                    self.store.release(&lease.fence,self.clock.now_ms())?;
                    return Ok(Status::Interrupted(WorkflowError::runtime("worker stopped")));
                },
                _=heartbeat.tick()=> {
                    if let Err(error)=self.store.renew(&lease.fence,self.clock.now_ms()) {
                        local_cancel.cancel();
                        if let Some(status)=self.store.get(&saved.admission.id)?.and_then(|r|r.outcome) {return Ok(status)}
                        return Ok(Status::Interrupted(WorkflowError::runtime(error.to_string())));
                    }
                },
                outcome=&mut run=> {
                    // Explicit cancel may already have committed and fenced us.
                    if let Some(status)=self.store.get(&saved.admission.id)?.and_then(|r|r.outcome) {return Ok(status)}
                    return self.store.finish(&lease.fence,&outcome.status,self.clock.now_ms(),self.config.breaker_threshold);
                }
            }
        }
    }
    pub async fn recover_once(&self, stop: &CancellationToken) -> anyhow::Result<usize> {
        let ids = self.store.due(self.clock.now_ms(), self.config.max_concurrency)?;
        let count = ids.len();
        let mut pending = FuturesUnordered::new();
        for id in &ids {
            pending.push(self.drive(id, stop));
        }
        while let Some(result) = pending.next().await {
            result?;
        }
        Ok(count)
    }
    /// Run on host startup. All futures belong to this worker and are joined
    /// during shutdown; no detached workflow tasks survive the worker.
    pub async fn worker(&self, stop: CancellationToken) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(self.config.poll_interval);
        let mut sweep = tokio::time::Instant::now();
        let mut running: FuturesUnordered<BoxFuture<'_, (String, anyhow::Result<Option<Status>>)>> =
            FuturesUnordered::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut failure = None;
        let shutdown = stop.child_token();
        loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break,
                Some((id,result))=running.next(),if !running.is_empty()=> {ids.remove(&id);if let Err(e)=result {failure=Some(e);break;}},
                _=tick.tick()=> {
                    if sweep.elapsed()>=Duration::from_secs(3600) {if let Err(e)=self.store.sweep(self.clock.now_ms(),self.config.retention_ms) {failure=Some(e);break;} sweep=tokio::time::Instant::now();}
                    let due=match self.store.due(self.clock.now_ms(),self.config.max_concurrency) {Ok(due)=>due,Err(e)=>{failure=Some(e);break;}};
                    for id in due {
                        if ids.len()>=self.config.max_concurrency {break;}
                        if ids.insert(id.clone()) {let token=shutdown.clone();running.push(Box::pin(async move {let result=self.drive(&id,&token).await;(id,result)}));}
                    }
                }
            }
        }
        shutdown.cancel();
        while let Some((_, result)) = running.next().await {
            if let Err(e) = result {
                failure = Some(e);
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(())
    }
}
// ── 2. admission ───────────────────────────────────────────────────────────

/// Builds admissions: the definition version, its input, and the bindings
/// (function schemas and effects, external schemas) the run keeps.
#[derive(Clone)]
pub(crate) struct Admitter {
    pub definitions: Arc<dyn RunStore>,
    pub registry: Arc<Registry>,
    pub capabilities: Capabilities,
    pub metadata: Value,
}

impl Admitter {
    pub fn validate(&self, yaml: &str) -> anyhow::Result<Workflow> {
        let wf = self.registry.validate_with(yaml, &self.capabilities)?;
        self.schemas(&wf)?;
        Ok(wf)
    }
    /// Every external schema, resolved and checked to be a JSON Schema.
    fn schemas(&self, wf: &Workflow) -> anyhow::Result<serde_json::Map<String, Value>> {
        let mut out = serde_json::Map::new();
        for uri in model::schema_resources(wf) {
            let resolver = self
                .capabilities
                .resources
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no resource resolver for {uri}"))?;
            let document = resolver.resolve(&uri).map_err(|e| anyhow::anyhow!("schema {uri}: {e}"))?;
            jsonschema::validator_for(&document)
                .map_err(|e| anyhow::anyhow!("schema {uri} is not a JSON Schema: {e}"))?;
            out.insert(uri, document);
        }
        Ok(out)
    }
    /// A run of `name`'s active version.
    pub fn active(&self, id: &str, name: &str, input: Value) -> anyhow::Result<Admission> {
        let definitions = self.definitions.definitions(name)?;
        let d = definitions
            .active
            .and_then(|v| definitions.versions.get(&v))
            .ok_or_else(|| anyhow::anyhow!("no active definition for {name}"))?;
        self.of(id, d, input)
    }
    pub fn of(&self, id: &str, d: &Definition, input: Value) -> anyhow::Result<Admission> {
        let wf = self.validate(&d.yaml)?;
        Ok(Admission {
            id: id.into(),
            name: d.name.clone(),
            version: d.version,
            definition: d.yaml.clone(),
            input,
            runtime: json!({
                "name": "ouro-runtime",
                "version": env!("CARGO_PKG_VERSION"),
                "metadata": self.metadata,
                "bindings": self.registry.bindings(&wf),
                "schemas": self.schemas(&wf)?,
            }),
        })
    }
    /// `run.workflow`'s target. CHOICE: the catalog entry named `name`
    /// holds the workflow; the version is the active one for `latest`, else
    /// the newest saved version whose document has that semantic version.
    /// Either way the document's namespace must match. `None`: no such one.
    pub fn target(&self, target: &ChildTarget<'_>) -> anyhow::Result<Option<Definition>> {
        let definitions = self.definitions.definitions(target.name)?;
        let document = |d: &Definition| -> Option<(String, String)> {
            let v: Value = serde_yaml_ng::from_str(&d.yaml).ok()?;
            let doc = v.get("document")?;
            Some((doc.get("namespace")?.as_str()?.to_owned(), doc.get("version")?.as_str()?.to_owned()))
        };
        let candidate = if target.version == "latest" {
            definitions.active.and_then(|v| definitions.versions.get(&v))
        } else {
            definitions.versions.values().rev().find(|d| document(d).is_some_and(|(_, v)| v == target.version))
        };
        Ok(candidate.filter(|d| document(d).is_some_and(|(ns, _)| ns == target.namespace)).cloned())
    }
}

// ── 4. the interpreter's host ──────────────────────────────────────────────

struct InterpreterClock(Arc<dyn Clock>);
impl run::Clock for InterpreterClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.0.now_ms()).expect("host clock must return a valid UTC timestamp")
    }
}

/// The external schemas a run was admitted with.
struct Pinned(BTreeMap<String, Value>);
impl run::Resources for Pinned {
    fn schema(&self, uri: &str) -> anyhow::Result<Value> {
        self.0.get(uri).cloned().ok_or_else(|| anyhow::anyhow!("schema {uri} was not resolved at admission"))
    }
}

pub(crate) struct ExecutionFunctions {
    pub registry: Arc<Registry>,
    pub admitter: Admitter,
    schemas: Pinned,
    pub lease: Arc<Lease>,
    pub spans: Arc<Spans>,
    pub function_permits: Arc<Semaphore>,
    pub mode: Mode,
}
impl ExecutionFunctions {
    /// A function call or, with `process`, a process run: replayed, mocked
    /// (a dry run's writes) or dispatched, inside its span.
    async fn invoke_function(
        &self,
        name: &str,
        process: Option<&str>,
        args: Value,
        info: &CallInfo<'_>,
    ) -> Result<Value, CallFailure> {
        let runner = self.admitter.capabilities.processes.as_ref();
        let effect = match process {
            Some(kind) => runner.and_then(|r| r.effect(kind)),
            None => self.effect(name),
        }
        .ok_or_else(|| CallFailure::NotExecuted(WorkflowError::validation(format!("unknown function {name}"))))?;
        let mut span = self.spans.function(
            name,
            info.reference,
            if effect == Effect::Read { "read" } else { "write" },
            info.attempt,
        );
        let context = FunctionContext {
            cancel: info.cancel.clone(),
            run_id: info.run_id.into(),
            task_path: info.reference.into(),
            attempt: info.attempt,
            idempotency_key: serde_json::to_string(&(info.run_id, info.logical)).expect("string tuple serializes"),
            span: span.context.clone(),
            input: info.input.clone(),
        };
        let result = async {
            if let Mode::Replay { .. } = &self.mode {
                return self.replayed(name, &args, effect, info);
            }
            if crate::registry::builtin(name) {
                return self
                    .builtin(name, args, info)
                    .map_err(|e| CallFailure::Interrupted(WorkflowError::runtime(e.to_string())));
            }
            let registered = match process {
                Some(_) => None,
                None => Some(self.registry.get(name).expect("registered function")),
            };
            if let Some(f) = registered {
                f.input
                    .validate(&args)
                    .map_err(|e| CallFailure::NotExecuted(WorkflowError::validation(format!("{name} input: {e}"))))?;
            }
            let _permit = tokio::select! {
                biased;
                _ = info.cancel.cancelled() => return Err(CallFailure::Interrupted(
                    WorkflowError::runtime("call cancelled before dispatch"))),
                permit = self.function_permits.acquire() => permit.map_err(|e|
                    CallFailure::Interrupted(WorkflowError::runtime(e.to_string())))?
            };
            let result = match &self.mode {
                Mode::Dry { mock, writes } if effect == Effect::Write => {
                    let result = mock.response(name, &args, &context);
                    writes.lock().unwrap().push(RecordedCall {
                        task_path: info.reference.into(),
                        logical: info.logical.into(),
                        function: name.into(),
                        arguments: args.clone(),
                        effect,
                        delivery: info.delivery,
                        result: result.clone(),
                    });
                    result
                }
                Mode::Live | Mode::Dry { .. } => match (registered, process, runner) {
                    (Some(f), _, _) => {
                        f.function.handler.call(args, context.clone()).with_context(context.span.clone()).await
                    }
                    (None, Some(kind), Some(runner)) => {
                        runner.run(kind, args, context.clone()).with_context(context.span.clone()).await
                    }
                    _ => Err(WorkflowError::runtime(format!("no runner for {name}"))),
                },
                Mode::Replay { .. } => unreachable!("replay returns before dispatch"),
            }
            .map_err(CallFailure::Response)?;
            if let Some(f) = registered {
                f.output
                    .validate(&result)
                    .map_err(|e| CallFailure::Response(WorkflowError::validation(format!("{name} output: {e}"))))?;
            }
            Ok(result)
        }
        .await;
        span.finish(
            if result.is_ok() { "completed" } else { "faulted" },
            result.as_ref().err().map(|e| info.secrets.text(&format!("{e:?}"))),
        );
        result
    }
    /// A recorded call's result. Records hold redacted values, so the
    /// comparison is between redacted forms and the result gets its
    /// secrets back.
    fn replayed(&self, name: &str, args: &Value, effect: Effect, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        let Mode::Replay { calls, used, .. } = &self.mode else { unreachable!("only replay mode has recorded calls") };
        let call = calls.get(info.reference).ok_or_else(|| {
            CallFailure::NotExecuted(WorkflowError::runtime(format!("missing replay result at {}", info.reference)))
        })?;
        if call.function != name
            || call.arguments != info.secrets.value(args)
            || call.logical != info.logical
            || call.effect != effect
            || call.delivery != info.delivery
        {
            return Err(CallFailure::NotExecuted(WorkflowError::runtime("replay call bindings differ")));
        }
        used.lock().unwrap().insert(info.reference.to_string());
        match &call.result {
            Ok(v) => info.secrets.restore_value(v).map_err(|e| CallFailure::NotExecuted(WorkflowError::runtime(e))),
            Err(e) => Err(CallFailure::Response(e.clone())),
        }
    }
    fn builtin(&self, name: &str, args: Value, info: &CallInfo<'_>) -> anyhow::Result<Value> {
        let store = &self.lease.store;
        if name == "child_outcome" {
            let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("child_outcome requires id"))?;
            let key = args["key"].as_str().ok_or_else(|| anyhow::anyhow!("child_outcome requires key"))?;
            return Ok(serde_json::to_value(store.child_outcome(info.run_id, key, id)?)?);
        }
        let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("start_child requires name"))?;
        let id = child_id(info.run_id, info.logical);
        // Replay joins the pinned child even if its active definition changed.
        let a = if let Some(saved) = store.get(&id)? {
            anyhow::ensure!(saved.admission.name == name, "child definition changed");
            saved.admission
        } else {
            self.admitter.active(&id, name, args.get("input").cloned().unwrap_or(Value::Null))?
        };
        let now = self.lease.clock.now_ms();
        let link = ChildLink { parent: info.run_id.into(), key: info.logical.into(), reference: info.reference.into() };
        if let Some(fence) = store.start_child(&self.lease.fence, &a, &link, now)? {
            store.release(&fence, now)?;
        }
        Ok(json!({"id":id,"key":info.logical,"done":format!("done:{id}"),"waiting":format!("waiting:{id}")}))
    }
}
/// A child's run id: its parent and the invocation that started it.
fn child_id(run_id: &str, key: &str) -> String {
    format!("child:{}", serde_json::to_string(&(run_id, key)).expect("string tuple serializes"))
}
impl run::Events for ExecutionFunctions {
    fn register(&self, id: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::register(&*self.lease, id, types)
    }
    fn unregister(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::unregister(&*self.lease, id, key, types)
    }
    fn subscribe(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::subscribe(&*self.lease, id, key, types)
    }
    fn unsubscribe(&self, id: &str, key: &str) -> anyhow::Result<()> {
        run::Events::unsubscribe(&*self.lease, id, key)
    }
    fn take(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        if let Mode::Replay { events, .. } = &self.mode
            && let Some(event) = events.get(key)
        {
            // The recorded envelope, as it was taken, to this run only.
            let time = event["time"].as_str().ok_or_else(|| anyhow::anyhow!("missing recorded event time"))?;
            let delivered_at = chrono::DateTime::parse_from_rfc3339(time)?.timestamp_millis();
            self.lease.store.publish(event, Some(id), delivered_at)?;
        }
        run::Events::take(&*self.lease, id, key, types)
    }
    fn notification(&self, id: &str) -> BoxFuture<'_, ()> {
        run::Events::notification(&*self.lease, id)
    }
}
impl run::Children for ExecutionFunctions {
    fn start(
        &self,
        run_id: &str,
        key: &str,
        target: &ChildTarget<'_>,
        input: &Value,
        owned: bool,
    ) -> Result<Child, CallFailure> {
        let interrupted = |e: anyhow::Error| CallFailure::Interrupted(WorkflowError::runtime(e.to_string()));
        let store = &self.lease.store;
        let id = child_id(run_id, key);
        let done = format!("done:{id}");
        // A started child keeps the version it was admitted with.
        let a = match store.get(&id).map_err(interrupted)? {
            Some(saved) => saved.admission,
            None => {
                let d = self.admitter.target(target).map_err(interrupted)?.ok_or_else(|| {
                    CallFailure::NotExecuted(WorkflowError::standard(
                        "configuration",
                        400,
                        "Configuration Error",
                        format!("no workflow {}/{} version {} to run", target.namespace, target.name, target.version),
                    ))
                })?;
                self.admitter.of(&id, &d, input.clone()).map_err(|e| {
                    CallFailure::NotExecuted(WorkflowError::standard(
                        "configuration",
                        400,
                        "Configuration Error",
                        e.to_string(),
                    ))
                })?
            }
        };
        let now = self.lease.clock.now_ms();
        if !owned {
            store.admit(&a, now).map_err(interrupted)?;
            return Ok(Child { id, done });
        }
        let link = ChildLink { parent: run_id.into(), key: key.into(), reference: key.into() };
        if let Some(fence) = store.start_child(&self.lease.fence, &a, &link, now).map_err(interrupted)? {
            store.release(&fence, now).map_err(interrupted)?;
        }
        Ok(Child { id, done })
    }
}
#[async_trait]
impl Functions for ExecutionFunctions {
    fn events(&self) -> Option<&dyn run::Events> {
        Some(self)
    }
    fn children(&self) -> Option<&dyn run::Children> {
        Some(self)
    }
    fn secrets(&self) -> Option<&dyn owf::secrets::Secrets> {
        self.admitter.capabilities.secrets.as_deref()
    }
    fn resources(&self) -> Option<&dyn run::Resources> {
        Some(&self.schemas)
    }
    fn emits(&self) -> bool {
        true
    }
    async fn emit(&self, event: &Value, info: &CallInfo<'_>) -> Result<(), CallFailure> {
        match &self.mode {
            Mode::Dry { writes, .. } => {
                writes.lock().unwrap().push(RecordedCall {
                    task_path: info.reference.into(),
                    logical: info.logical.into(),
                    function: "emit".into(),
                    arguments: event.clone(),
                    effect: Effect::Write,
                    delivery: info.delivery,
                    result: Ok(event.clone()),
                });
                Ok(())
            }
            // CHOICE: replay compares the emitted event without the `id`
            // and `time` the runtime generated, and publishes nothing.
            Mode::Replay { calls, used, .. } => {
                let strip = |v: &Value| {
                    let mut v = v.clone();
                    if let Value::Object(m) = &mut v {
                        m.remove("id");
                        m.remove("time");
                    }
                    v
                };
                match calls.get(info.reference) {
                    Some(call)
                        if call.function == "emit" && strip(&call.arguments) == strip(&info.secrets.value(event)) =>
                    {
                        used.lock().unwrap().insert(info.reference.to_string());
                        Ok(())
                    }
                    _ => Err(CallFailure::NotExecuted(WorkflowError::runtime("replay emit differs"))),
                }
            }
            Mode::Live => {
                let now = self.lease.clock.now_ms();
                // A storage failure is no answer: the run stops recoverably and
                // recovery publishes the same envelope again (deduplicated).
                crate::schedule::publish(&*self.lease.store, &self.admitter, event, now)
                    .map_err(|e| CallFailure::Interrupted(WorkflowError::runtime(e.to_string())))?;
                if let Some(sink) = &self.admitter.capabilities.sink {
                    sink.emit(event).await.map_err(CallFailure::Response)?;
                }
                Ok(())
            }
        }
    }
    fn process(&self, kind: &str) -> Option<Effect> {
        self.admitter.capabilities.processes.as_ref().and_then(|p| p.effect(kind))
    }
    async fn run_process(&self, kind: &str, definition: Value, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        self.invoke_function(&Process::function(kind), Some(kind), definition, info).await
    }
    fn effect(&self, name: &str) -> Option<Effect> {
        if crate::registry::builtin(name) {
            Some(Effect::Read)
        } else {
            self.registry.get(name).map(|f| f.function.effect)
        }
    }
    fn at_most_once(&self, name: &str, _: &Value) -> bool {
        // Writes, processes included, are delivered at most once.
        let effect = match name.strip_prefix(&Process::function("")) {
            Some(kind) => self.process(kind),
            None => self.effect(name),
        };
        effect == Some(Effect::Write)
    }
    async fn invoke(&self, n: &str, a: Value, i: &CallInfo<'_>) -> Result<Value, CallFailure> {
        self.invoke_function(n, None, a, i).await
    }
    async fn call(&self, n: &str, a: Value, i: &CallInfo<'_>) -> Result<Value, WorkflowError> {
        self.invoke_function(n, None, a, i).await.map_err(|e| match e {
            CallFailure::NotExecuted(e)
            | CallFailure::Response(e)
            | CallFailure::Unknown(e)
            | CallFailure::Interrupted(e) => e,
        })
    }
}
