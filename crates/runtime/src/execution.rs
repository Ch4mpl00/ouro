//! Run driver: 1. configuration and the runtime; 2. admission (definitions,
//! versions, the start check, pinned bindings, schemas and the run tree's
//! limits); 3. bounded execution, shutdown/recovery; 4. the interpreter's
//! host: functions, processes, events, emitted events, child runs, secrets
//! and resources. A process stop releases work; explicit cancellation is a
//! terminal storage transaction. Lease loss always stops forward progress.
use crate::{
    FunctionContext, Registry,
    capabilities::{Capabilities, EventSink, ProcessRunner, ResourceResolver, SecretResolver},
    registry::{Checks, Installed, MissingFunction},
    telemetry::Spans,
    testing::{Mode, RecordedCall},
};
use async_trait::async_trait;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use opentelemetry::{Context, global::BoxedTracer, trace::FutureExt};
use ouro_durable::{
    Admission, ChildLink, Clock, Compatibility, Definition, Fence, Lease, RunStore, SavedRun, Swept, SystemClock,
};
use owf::{
    durable::{CallEntry, CallFailure, Delivery},
    model::{self, Process, Workflow},
    run::{self, CallInfo, Child, ChildTarget, Functions, Host, Limits, Status, WorkflowError},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// Host ceilings no definition can lift, among them the jump budget of a
    /// run tree (`max_jumps`, default 100) and how deep child runs nest
    /// (`max_depth`, default 10). Only the host sets them; a definition may
    /// only tighten the budget (`document.metadata.limits`).
    pub limits: Limits,
    /// `$runtime.metadata`: implementation-specific facts the host exposes
    /// to every expression (a tenant, feature flags).
    pub metadata: Value,
    /// The application version this runtime declares, recorded in every run
    /// it admits, and the older versions its code still resumes. Undeclared
    /// (the default): every run is compatible, as before versions existed.
    /// Declared: this runtime claims only runs of a compatible version; the
    /// others wait, visibly, for a compatible worker (`Runtime::compatible`).
    pub application: Compatibility,
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
            application: Compatibility::default(),
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
    pub(crate) installed: Installed,
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
        // A worker stops acting the skew bound before its lease ends, so it
        // must renew before then.
        let acting = ouro_durable::LEASE_MS.saturating_sub(store.policy().skew_ms);
        anyhow::ensure!(
            !config.poll_interval.is_zero()
                && !config.heartbeat_interval.is_zero()
                && config.heartbeat_interval < Duration::from_millis(u64::try_from(acting.max(0))?),
            "invalid polling/heartbeat interval"
        );
        Ok(Self {
            definition_source: store.clone(),
            store,
            installed: Installed::new(registry),
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
    /// The registry runs call through now.
    pub fn registry(&self) -> Arc<Registry> {
        self.installed.get().0
    }
    /// Replaces the registry (a host whose functions change while it runs).
    /// Admissions and resumed runs check against the new one from now on;
    /// a run being driven keeps the registry it started its drive with.
    pub fn set_registry(&self, registry: Arc<Registry>) {
        self.installed.replace(registry);
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
    /// Activates a saved version in a new activation epoch, clearing a
    /// tripped breaker; its schedule's triggers replace the definition's
    /// current ones in the same commit. Only an activation does either: a
    /// tripped breaker reports (`definitions`) and the host decides
    /// which version, if any, to activate.
    pub fn activate(&self, name: &str, version: u64) -> anyhow::Result<()> {
        let definitions = self.store.definitions(name)?;
        let d = definitions.versions.get(&version).ok_or_else(|| anyhow::anyhow!("unknown definition version"))?;
        self.validate(&d.yaml)?;
        self.activate_armed(name, version)
    }
    pub(crate) fn admitter(&self) -> Admitter {
        let (registry, generation) = self.installed.get();
        Admitter {
            definitions: self.definition_source.clone(),
            registry,
            generation,
            checks: self.installed.checks.clone(),
            capabilities: self.capabilities.clone(),
            metadata: self.config.metadata.clone(),
            application: self.config.application.version.clone(),
            limits: self.config.limits.clone(),
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
    /// The run's call journal: every call (and emitted event) it made, with
    /// its delivery and how far it got. The engine does not know whether a
    /// call changed the world; the host decides that from this journal (to
    /// rerun a failed run, hand it over, or reconcile it).
    pub fn calls(&self, id: &str) -> anyhow::Result<Vec<CallEntry>> {
        Ok(owf::durable::call_journal(&self.records(id)?))
    }
    /// Delivers an identified CloudEvent (`id`, `source`, `type`; the
    /// `specversion` and `time` are filled in when absent): to `target`'s
    /// mailbox, where a listen takes it until it expires, or to the oldest
    /// run waiting for its type. Delivering it again returns the same
    /// recipient; another payload under its identity is an error. Returns
    /// the recipient, `None` when no run is eligible.
    pub fn deliver(&self, mut event: Value, target: Option<&str>) -> anyhow::Result<Option<String>> {
        let now = self.clock.now_ms();
        crate::schedule::complete(&mut event, now)?;
        self.store.deliver(&event, target, now)
    }
    /// Whether this runtime may resume `run`: its application version is
    /// undeclared, or one this runtime declares or accepts. An unfinished run
    /// no running worker is compatible with waits for one.
    pub fn compatible(&self, run: &SavedRun) -> bool {
        self.config.application.admits(run.admission.application_version())
    }
    /// Removes evidence past retention, expires mailbox events no listen
    /// took in time (each leaves a `Record::Expired`) and old ingress
    /// tombstones; traced as `sweep` with the counts. The worker calls it.
    pub fn sweep(&self) -> anyhow::Result<Swept> {
        let swept = self.store.sweep(self.clock.now_ms(), self.config.retention_ms)?;
        crate::telemetry::mark(
            &self.tracer,
            "sweep",
            vec![
                opentelemetry::KeyValue::new("ouro.sweep.records", i64::try_from(swept.records).unwrap_or(i64::MAX)),
                opentelemetry::KeyValue::new("ouro.sweep.expired", i64::try_from(swept.expired).unwrap_or(i64::MAX)),
                opentelemetry::KeyValue::new(
                    "ouro.sweep.tombstones",
                    i64::try_from(swept.tombstones).unwrap_or(i64::MAX),
                ),
            ],
        );
        Ok(swept)
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
            let owner = self.store.get(&parent.run_id)?.ok_or_else(|| anyhow::anyhow!("unknown parent run"))?;
            let declared = model::parse_yaml(&owner.admission.definition)?.budget;
            let lineage = Lineage::of(&owner.admission, &self.config.limits);
            let lineage = Lineage { max_jumps: declared.max_jumps(lineage.max_jumps), ..lineage };
            let child = lineage.child(&self.config.limits).map_err(|e| anyhow::anyhow!("{e}"))?;
            self.admitter().active_in(id, name, input, &child)?
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
        let Some(fence) = self.store.claim_as(id, self.clock.now_ms(), &self.config.application)? else {
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
        // Before the run's next step: every function it can call is still
        // registered (the registry may have changed since admission, by a
        // new release of the host or `set_registry`). A run that cannot
        // proceed fails now, never halfway through.
        let admitter = self.admitter();
        let a = &saved.admission;
        if let Err(e) = admitter.check(&a.name, a.version, &a.definition) {
            let missing = e.downcast::<MissingFunction>()?;
            return self.store.finish(
                &lease.fence,
                &Status::Faulted(missing.error()),
                self.clock.now_ms(),
                self.config.breaker_threshold,
            );
        }
        let registry = admitter.registry.clone();
        let checked = registry.validate_with(&a.definition, &self.capabilities).and_then(|wf| {
            anyhow::ensure!(
                a.runtime["bindings"] == registry.bindings(&wf),
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
        // The budget the run was admitted with: the host's ceiling then, or
        // its parent's budget. The definition tightens it in the interpreter.
        let lineage = Lineage::of(&saved.admission, &limits);
        limits.max_jumps = lineage.max_jumps;
        let lineage = Lineage { max_jumps: wf.budget.max_jumps(lineage.max_jumps), ..lineage };
        let runtime = &saved.admission.runtime;
        let host = Host {
            functions: Arc::new(ExecutionFunctions {
                registry,
                admitter,
                schemas: Pinned(
                    runtime["schemas"].as_object().map(|m| m.clone().into_iter().collect()).unwrap_or_default(),
                ),
                lease: lease.clone(),
                spans: spans.clone(),
                function_permits: self.function_permits.clone(),
                lineage,
                mode: mode.clone(),
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
        // The workflow's deadline counts from admission, whichever process
        // first drives the run.
        let input = saved.admission.input.clone();
        // Replay restores the recorded budget decisions, not its private
        // store's: the store saves each one (granted or refused) as it was.
        let replayed = match &mode {
            Mode::Replay { budgets, first, .. } => {
                Some(ReplayJournal { lease: lease.clone(), budgets: budgets.clone(), first: first.clone() })
            }
            _ => None,
        };
        let journal: &dyn owf::durable::Journal = match &replayed {
            Some(replayed) => replayed,
            None => &*lease,
        };
        let run = run::resume_from(&host, &wf, &saved.admission.id, input, saved.admitted_ms, local_cancel, journal)
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
                    if outcome.limit_exceeded {
                        spans.limit_exceeded();
                    }
                    // Explicit cancel may already have committed and fenced us.
                    if let Some(status)=self.store.get(&saved.admission.id)?.and_then(|r|r.outcome) {return Ok(status)}
                    return self.store.finish(&lease.fence,&outcome.status,self.clock.now_ms(),self.config.breaker_threshold);
                }
            }
        }
    }
    pub async fn recover_once(&self, stop: &CancellationToken) -> anyhow::Result<usize> {
        let ids = self.store.due_as(self.clock.now_ms(), self.config.max_concurrency, &self.config.application)?;
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
                    if sweep.elapsed()>=Duration::from_secs(3600) {if let Err(e)=self.sweep() {failure=Some(e);break;} sweep=tokio::time::Instant::now();}
                    // Due schedule slots: a range of the trigger index ending now.
                    if let Err(e)=self.fire_due() {failure=Some(e);break;}
                    let due=match self.store.due_as(self.clock.now_ms(),self.config.max_concurrency,&self.config.application) {Ok(due)=>due,Err(e)=>{failure=Some(e);break;}};
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
/// (function schemas and delivery, external schemas) the run keeps.
#[derive(Clone)]
pub(crate) struct Admitter {
    pub definitions: Arc<dyn RunStore>,
    pub registry: Arc<Registry>,
    /// The registry's generation, for the start checks made against it.
    pub generation: u64,
    pub checks: Arc<Checks>,
    pub capabilities: Capabilities,
    pub metadata: Value,
    /// The declared application version, recorded in every admission.
    pub application: Option<String>,
    /// The host's ceilings: a root run is admitted with its jump budget.
    pub limits: Limits,
}

/// Where a run stands in its tree, pinned in its admission's runtime
/// bindings (`limits`): the jump budget it was admitted with (the host's
/// ceiling for a root, its parent's budget for an owned child) and how many
/// ancestors it has.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lineage {
    pub max_jumps: u64,
    pub depth: u32,
}
impl Lineage {
    /// An admission without the pin is a root under today's ceiling.
    fn of(admission: &Admission, limits: &Limits) -> Self {
        let pinned = &admission.runtime["limits"];
        Lineage {
            max_jumps: pinned["max_jumps"].as_u64().unwrap_or(limits.max_jumps),
            depth: pinned["depth"].as_u64().and_then(|d| u32::try_from(d).ok()).unwrap_or(0),
        }
    }
    /// An owned child's lineage, refused past the host's nesting depth.
    fn child(&self, limits: &Limits) -> Result<Lineage, WorkflowError> {
        let depth = self.depth.saturating_add(1);
        if depth > limits.max_depth {
            return Err(WorkflowError::budget(format!(
                "a child run would nest {depth} deep; this host allows {}",
                limits.max_depth
            )));
        }
        Ok(Lineage { depth, ..*self })
    }
    fn pin(&self) -> Value {
        json!({ "max_jumps": self.max_jumps, "depth": self.depth })
    }
}

impl Admitter {
    pub fn validate(&self, yaml: &str) -> anyhow::Result<Workflow> {
        let wf = self.registry.validate_with(yaml, &self.capabilities)?;
        run::validate_limits(&wf, &self.limits)?;
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
    /// A root run of `name`'s active version, unless its breaker tripped.
    pub fn active(&self, id: &str, name: &str, input: Value) -> anyhow::Result<Admission> {
        self.active_in(id, name, input, &self.root())
    }
    fn root(&self) -> Lineage {
        Lineage { max_jumps: self.limits.max_jumps, depth: 0 }
    }
    /// `active` at a place in a run tree.
    pub fn active_in(&self, id: &str, name: &str, input: Value, lineage: &Lineage) -> anyhow::Result<Admission> {
        let definitions = self.definitions.definitions(name)?;
        if let Some(trip) = &definitions.tripped {
            anyhow::bail!(
                "{name} is tripped: version {} faulted {} times in a row; activate a version to resume",
                trip.version,
                trip.faults
            );
        }
        let d = definitions
            .active
            .and_then(|v| definitions.versions.get(&v))
            .ok_or_else(|| anyhow::anyhow!("no active definition for {name}"))?;
        self.of(id, d, input, lineage)
    }
    pub fn of(&self, id: &str, d: &Definition, input: Value, lineage: &Lineage) -> anyhow::Result<Admission> {
        self.check(&d.name, d.version, &d.yaml)?;
        let wf = self.validate(&d.yaml)?;
        let mut runtime = json!({
            "name": "ouro-runtime",
            "version": env!("CARGO_PKG_VERSION"),
            "metadata": self.metadata,
            "bindings": self.registry.bindings(&wf),
            "schemas": self.schemas(&wf)?,
            "limits": lineage.pin(),
        });
        if let Some(version) = &self.application {
            runtime[ouro_durable::APPLICATION_VERSION] = json!(version);
        }
        Ok(Admission {
            id: id.into(),
            name: d.name.clone(),
            version: d.version,
            definition: d.yaml.clone(),
            input,
            runtime,
        })
    }
    /// The start check: every function the definition can call is
    /// registered, directly or in the child workflows its `run: workflow`
    /// tasks name, transitively, as they resolve now. A missing one is a
    /// `MissingFunction` (inside the error) naming it and the definitions
    /// that lead to it; a child that does not resolve is left to the run,
    /// which faults on it as it always has. Each definition version's own
    /// needs are cached per registry generation; resolving its children
    /// reads their names' catalog entries.
    pub fn check(&self, name: &str, version: u64, yaml: &str) -> anyhow::Result<()> {
        let mut path = Vec::new();
        self.check_tree((name, version, yaml), &mut path, &mut BTreeSet::new())
    }
    fn check_tree(
        &self,
        (name, version, yaml): (&str, u64, &str),
        path: &mut Vec<(String, u64)>,
        seen: &mut BTreeSet<(String, u64)>,
    ) -> anyhow::Result<()> {
        // A definition reached twice (a recursive child) was checked already.
        if !seen.insert((name.to_owned(), version)) {
            return Ok(());
        }
        path.push((name.to_owned(), version));
        let needs = self.checks.needs((name, version, yaml), (&self.registry, self.generation), &self.capabilities);
        if let Some(function) = &needs.missing {
            return Err(MissingFunction { function: function.clone(), path: path.clone() }.into());
        }
        for (namespace, child, version) in &needs.children {
            if let Some(d) = self.target(&ChildTarget { namespace, name: child, version })? {
                self.check_tree((&d.name, d.version, &d.yaml), path, seen)?;
            }
        }
        path.pop();
        Ok(())
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
    /// This run's budget (its definition's included) and depth, which its
    /// owned children inherit.
    pub lineage: Lineage,
    pub mode: Mode,
}
impl ExecutionFunctions {
    /// A function call or, with `process`, a process run: replayed, mocked
    /// (in a dry run, what the host's mock rule marks) or dispatched, inside
    /// its span.
    async fn invoke_function(
        &self,
        name: &str,
        process: Option<&str>,
        args: Value,
        info: &CallInfo<'_>,
    ) -> Result<Value, CallFailure> {
        let runner = self.admitter.capabilities.processes.as_ref();
        let known = match process {
            Some(kind) => runner.is_some_and(|r| r.runs(kind)),
            None => self.has(name),
        };
        if !known {
            return Err(CallFailure::NotExecuted(WorkflowError::validation(format!("unknown function {name}"))));
        }
        let mut span = self.spans.function(name, info.reference, info.attempt);
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
                return self.replayed(name, &args, info);
            }
            if crate::registry::builtin(name) {
                return self.builtin(name, args, info).map_err(|e| match e.downcast::<Refused>() {
                    Ok(Refused(error)) => CallFailure::NotExecuted(error),
                    Err(e) => CallFailure::Interrupted(WorkflowError::runtime(e.to_string())),
                });
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
                // The dry run's guarantee: a call the host's rule marks is
                // answered here and never reaches its handler or runner.
                Mode::Dry { mock, mocked } if mock.mocks(name, &args) => {
                    let result = mock.response(name, &args, &context);
                    mocked.lock().unwrap().push(RecordedCall {
                        task_path: info.reference.into(),
                        logical: info.logical.into(),
                        function: name.into(),
                        arguments: args.clone(),
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
    fn replayed(&self, name: &str, args: &Value, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        let Mode::Replay { calls, used, .. } = &self.mode else { unreachable!("only replay mode has recorded calls") };
        let call = calls.get(info.reference).ok_or_else(|| {
            CallFailure::NotExecuted(WorkflowError::runtime(format!("missing replay result at {}", info.reference)))
        })?;
        if call.function != name
            || call.arguments != info.secrets.value(args)
            || call.logical != info.logical
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
            let lineage = self.lineage.child(&self.admitter.limits).map_err(Refused)?;
            self.admitter.active_in(&id, name, args.get("input").cloned().unwrap_or(Value::Null), &lineage)?
        };
        let now = self.lease.clock.now_ms();
        let link = ChildLink { parent: info.run_id.into(), key: info.logical.into(), reference: info.reference.into() };
        if let Some(fence) = store.start_child(&self.lease.fence, &a, &link, now)? {
            store.release(&fence, now)?;
        }
        Ok(json!({"id":id,"key":info.logical,"done":format!("done:{id}"),"waiting":format!("waiting:{id}")}))
    }
}
/// A child the host refused to start (its nesting depth): an answer the
/// workflow can catch, not a stop.
#[derive(Debug)]
struct Refused(WorkflowError);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Refused {}

/// Replay's journal: a recorded budget decision is saved again as it was
/// (a granted one with no limit, a refused one with none left), so the
/// replayed run jumps where the recorded one did whatever its private
/// store counted; an unrecorded one spends the private budget. The recorded
/// first refusal is saved before any other, so the same jump acts on it.
struct ReplayJournal {
    lease: Arc<Lease>,
    budgets: Arc<BTreeMap<String, bool>>,
    first: Option<String>,
}
impl owf::durable::Journal for ReplayJournal {
    fn load(&self) -> anyhow::Result<BTreeMap<String, owf::durable::Record>> {
        self.lease.load()
    }
    fn save(&self, key: &str, record: &owf::durable::Record) -> anyhow::Result<()> {
        self.lease.save(key, record)
    }
    fn spend(&self, key: &str, limit: u64) -> anyhow::Result<Option<owf::durable::Record>> {
        let limit = match self.budgets.get(key) {
            Some(true) => u64::MAX,
            Some(false) => {
                if let Some(first) = self.first.as_deref().filter(|first| *first != key) {
                    self.lease.spend(first, 0)?;
                }
                0
            }
            None => limit,
        };
        self.lease.spend(key, limit)
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
                // An independent run starts a tree of its own, one level
                // deeper, so recursion through either kind of child ends.
                let lineage = self.lineage.child(&self.admitter.limits).map_err(CallFailure::NotExecuted)?;
                let lineage =
                    if owned { lineage } else { Lineage { max_jumps: self.admitter.limits.max_jumps, ..lineage } };
                let d = self.admitter.target(target).map_err(interrupted)?.ok_or_else(|| {
                    CallFailure::NotExecuted(WorkflowError::standard(
                        "configuration",
                        400,
                        "Configuration Error",
                        format!("no workflow {}/{} version {} to run", target.namespace, target.name, target.version),
                    ))
                })?;
                self.admitter.of(&id, &d, input.clone(), &lineage).map_err(|e| {
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
            // An emitted event leaves the dry run only as a record: it is
            // neither published nor handed to the sink.
            Mode::Dry { mocked, .. } => {
                mocked.lock().unwrap().push(RecordedCall {
                    task_path: info.reference.into(),
                    logical: info.logical.into(),
                    function: "emit".into(),
                    arguments: event.clone(),
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
    fn process(&self, kind: &str) -> bool {
        self.admitter.capabilities.processes.as_ref().is_some_and(|p| p.runs(kind))
    }
    async fn run_process(&self, kind: &str, definition: Value, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        self.invoke_function(&Process::function(kind), Some(kind), definition, info).await
    }
    fn has(&self, name: &str) -> bool {
        crate::registry::builtin(name) || self.registry.get(name).is_some()
    }
    fn at_most_once(&self, name: &str, _: &Value) -> bool {
        // The declared delivery; the interpreter applies a task's
        // `metadata.delivery` on top, which can only tighten it.
        let delivery = match name.strip_prefix(&Process::function("")) {
            Some(kind) => self.admitter.capabilities.processes.as_ref().map(|p| p.delivery(kind)),
            None => self.registry.get(name).map(|f| f.function.delivery),
        };
        delivery == Some(Delivery::AtMostOnce)
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
