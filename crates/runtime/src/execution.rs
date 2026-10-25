//! Run driver: 1. configuration and the runtime; 2. admission (definitions,
//! versions, the start check, pinned bindings, schemas and the run tree's
//! limits); 3. bounded execution, shutdown/recovery; 4. the interpreter's
//! host: functions, processes, events, emitted events, child runs, secrets
//! and resources. A process stop releases work; explicit cancellation is a
//! terminal storage transaction. Lease loss always stops forward progress.
use crate::{
    FunctionContext, Output, Registry,
    capabilities::{Capabilities, EventSink, ProcessRunner, ResourceResolver, SecretResolver},
    observer::{EventKind, Observer, Observers, RunEvents},
    registry::{Checks, Installed, MissingFunction},
    telemetry::{Metrics, Telemetry},
    testing::{Mode, RecordedCall},
};
use async_trait::async_trait;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use opentelemetry::{Context, global::BoxedTracer, trace::FutureExt};
use ouro_durable::{
    Admission, ChildLink, Clock, Compatibility, Definition, DefinitionTripped, Fence, Lease, RunStore, SavedRun, Swept,
    SystemClock,
};
use ows::{
    durable::{Annotations, CallEntry, CallFailure, Delivery},
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
    /// Maximum unfinished runs of one definition when admitting a schedule
    /// slot, shared across workers and trigger kinds (default 10).
    pub max_concurrent_runs: usize,
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
    /// The host name this runtime's heartbeat rows carry, as the embedding
    /// application knows it (the runtime reads no environment).
    pub host_name: Option<String>,
    /// The largest payload (a task's input or output, a call's arguments or
    /// result, an event) an observer's event carries, as JSON bytes; a
    /// larger one is left out (default 16 KiB; 0 leaves every one out).
    pub event_payload_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_concurrent_runs: 10,
            max_function_concurrency: 16,
            breaker_threshold: 3,
            poll_interval: Duration::from_millis(100),
            heartbeat_interval: Duration::from_millis(ouro_durable::HEARTBEAT_MS),
            retention_ms: ouro_durable::RECORDS_KEPT_MS,
            limits: Limits::default(),
            metadata: json!({}),
            application: Compatibility::default(),
            host_name: None,
            event_payload_bytes: 16 * 1024,
        }
    }
}

/// How many due runs and open listens one backlog observation reads: the
/// gauges are lower bounds at this size.
const BACKLOG_PAGE: usize = ouro_durable::control::MAX_PAGE;

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
    pub(crate) metrics: Arc<Metrics>,
    /// The built-in OpenTelemetry observer and the host's.
    pub(crate) observers: Arc<Observers>,
    pub(crate) config: Config,
    pub(crate) capabilities: Capabilities,
    permits: Semaphore,
    function_permits: Arc<Semaphore>,
    executions: Arc<Mutex<BTreeMap<(String, u64), CancellationToken>>>,
    /// This runtime's identity in heartbeat rows, and when it was built.
    worker_id: String,
    started_ms: i64,
    /// Per notice key, the sink failures so far and when to try it again.
    /// In memory: another process, or a restart, may try sooner, which
    /// at-least-once allows.
    pub(crate) notice_backoff: Mutex<BTreeMap<String, (u32, i64)>>,
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
            config.max_concurrency > 0 && config.max_function_concurrency > 0 && config.max_concurrent_runs > 0,
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
        let tracer = Arc::new(tracer);
        let metrics = Arc::new(Metrics::new(crate::telemetry::noop_meter()));
        let observers = Arc::new(Observers {
            telemetry: Some(Arc::new(Telemetry::new(tracer.clone(), metrics.clone()))),
            host: Vec::new(),
            payload_bytes: config.event_payload_bytes,
        });
        Ok(Self {
            definition_source: store.clone(),
            store,
            installed: Installed::new(registry),
            tracer,
            metrics,
            observers,
            capabilities: Capabilities::default(),
            permits: Semaphore::new(config.max_concurrency),
            function_permits: Arc::new(Semaphore::new(config.max_function_concurrency)),
            config,
            executions: Arc::new(Mutex::new(BTreeMap::new())),
            worker_id: uuid::Uuid::new_v4().to_string(),
            started_ms: clock.now_ms(),
            notice_backoff: Mutex::new(BTreeMap::new()),
            clock,
        })
    }
    pub fn embedded(path: impl AsRef<std::path::Path>, registry: Registry) -> anyhow::Result<Self> {
        Ok(Self::new(
            Arc::new(ouro_durable::sqlite::open(path)?),
            Arc::new(registry),
            Config::default(),
            Arc::new(SystemClock),
            opentelemetry::global::tracer("ouro-runtime"),
        )?
        .with_meter(opentelemetry::global::meter("ouro-runtime")))
    }
    /// Records the runtime's metrics (`ouro.*`, see the README) with this
    /// meter; without one, none are recorded. The host owns the meter
    /// provider, its exporter and its shutdown. Function handlers get the
    /// same meter in `FunctionContext::meter`.
    pub fn with_meter(mut self, meter: opentelemetry::metrics::Meter) -> Self {
        self.metrics = Arc::new(Metrics::new(meter).enabled());
        let observers = Arc::make_mut(&mut self.observers);
        if observers.telemetry.is_some() {
            observers.telemetry = Some(Arc::new(Telemetry::new(self.tracer.clone(), self.metrics.clone())));
        }
        self
    }
    /// Adds an observer of the runtime's events (`observer`), called after
    /// the built-in OpenTelemetry observer and the observers added before.
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        Arc::make_mut(&mut self.observers).host.push(observer);
        self
    }
    /// Removes the built-in OpenTelemetry observer: no run, task or call
    /// spans, no run, step, call, breaker or budget metrics (the schedule
    /// and backlog metrics, which are no run's events, stay with the
    /// meter). Handlers then run in the context `drive` was called in. With
    /// `with_observer`, a host replaces it.
    pub fn without_telemetry(mut self) -> Self {
        Arc::make_mut(&mut self.observers).telemetry = None;
        self
    }
    /// This runtime's worker id in heartbeat rows, new for every runtime.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }
    /// Upserts this runtime's heartbeat row: its id, host name, application
    /// version, the runs it is executing now and the beat, stale after one
    /// lease (`ouro_durable::LEASE_MS`) without another beat. One row per
    /// runtime, constant cost. `worker` beats on its heartbeat cadence; a
    /// host that drives runs itself (a serverless invocation) beats when it
    /// runs. `Control::workers` lists the rows and marks the stale ones.
    pub async fn heartbeat(&self) -> anyhow::Result<()> {
        let now = self.clock.now_ms();
        self.beat(now, now.saturating_add(ouro_durable::LEASE_MS)).await
    }
    async fn beat(&self, now: i64, expires_ms: i64) -> anyhow::Result<()> {
        let held = u64::try_from(self.executions.lock().unwrap().len())?;
        let worker = ouro_durable::control::Worker {
            id: self.worker_id.clone(),
            host: self.config.host_name.clone(),
            application: self.config.application.version.clone(),
            started_ms: self.started_ms,
            beat_ms: now,
            expires_ms,
            held,
        };
        self.store.beat(&worker).await
    }
    /// The store-wide backlog as gauges (`ouro.backlog.*`, `ouro.waits.*`):
    /// one bounded page of due runs and one of open listens, oldest first,
    /// and the oldest run's row. Read only when the host injected a meter.
    async fn observe_backlog(&self) -> anyhow::Result<()> {
        if !self.metrics.is_enabled() {
            return Ok(());
        }
        let now = self.clock.now_ms();
        let due = self.store.due(now, BACKLOG_PAGE).await?;
        let due_age = match due.first() {
            Some(id) => self.store.get(id).await?.map_or(0, |r| {
                let since = if r.state == ouro_durable::RunState::Running {
                    r.lease_until_ms.unwrap_or(r.wake_at_ms)
                } else {
                    r.wake_at_ms
                };
                now.saturating_sub(since)
            }),
            None => 0,
        };
        let waits = self.store.pending_waits(&ouro_durable::control::Waits::default(), None, BACKLOG_PAGE).await?;
        let wait_age = waits.items.first().map_or(0, |w| now.saturating_sub(w.waiting.since_ms));
        self.metrics.backlog(u64::try_from(due.len())?, due_age, u64::try_from(waits.items.len())?, wait_age);
        Ok(())
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
    pub async fn save(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        self.validate(yaml)?;
        self.store.save_definition(name, yaml).await
    }
    /// Activates a saved version in a new activation epoch, clearing a
    /// tripped breaker; its schedule's triggers replace the definition's
    /// current ones in the same commit. Only activation starts an epoch: a
    /// tripped breaker reports (`definitions`, `notify`) and the host decides
    /// which version, if any, to activate.
    pub async fn activate(&self, name: &str, version: u64) -> anyhow::Result<()> {
        self.activated(name, version, None, None).await
    }
    /// `activate`, attributed: the catalog records `actor` (nonempty) and
    /// `note` with the activation's time, and `Control::definition` shows
    /// them as the version's latest activation.
    pub async fn activate_as(&self, name: &str, version: u64, actor: &str, note: Option<&str>) -> anyhow::Result<()> {
        anyhow::ensure!(!actor.trim().is_empty(), "an attributed activation needs an actor");
        self.activated(name, version, Some(actor), note).await
    }
    async fn activated(&self, name: &str, version: u64, actor: Option<&str>, note: Option<&str>) -> anyhow::Result<()> {
        let definitions = self.store.definitions(name).await?;
        let d = definitions.versions.get(&version).ok_or_else(|| anyhow::anyhow!("unknown definition version"))?;
        self.validate(&d.yaml)?;
        self.activate_armed(name, version, actor, note).await
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
            breaker_threshold: self.config.breaker_threshold,
            max_concurrent_runs: self.config.max_concurrent_runs,
            metrics: self.metrics.clone(),
            observers: self.observers.clone(),
            executions: self.executions.clone(),
        }
    }
    pub(crate) async fn admission(&self, id: &str, name: &str, input: Value) -> anyhow::Result<Admission> {
        self.admitter().active(id, name, input).await
    }
    /// Persist a run before any execution. `drive` executes it immediately;
    /// `worker` also picks it up, including after the embedding process restarts.
    pub async fn start(&self, id: &str, name: &str, input: Value) -> anyhow::Result<SavedRun> {
        if let Some(saved) = self.store.get(id).await? {
            anyhow::ensure!(saved.admission.name == name, "run belongs to another definition");
            return Ok(saved);
        }
        let a = self.admission(id, name, input).await?;
        let now = self.clock.now_ms();
        if self.store.admit(&a, now).await? {
            self.observers.admitted(&a, None, id, now);
        }
        self.store.get(id).await?.ok_or_else(|| anyhow::anyhow!("admitted run disappeared"))
    }
    /// `start`, at a stored version of `name` instead of the active one,
    /// without activating it: the active version, the activation epoch and
    /// the schedule stay as they are. Registry checks and validation apply as
    /// for any start, and the run records that it was pinned
    /// (`Inspection::pinned`); recovery and replay use its own admitted
    /// definition, as every run's.
    ///
    /// While the definition is tripped, a pinned start is refused iff the
    /// stored version's content digest (`ouro_durable::digest_of`) is the
    /// tripped one, whatever its number (`DefinitionTripped`; durable
    /// invariant V2). A version with other content is admitted, so a host can
    /// run its built-in version for a run while the tripped one stays
    /// active. A pinned run counts toward the breaker only when its version
    /// is the active one, as every run does.
    /// Starting an existing id returns that run if it was pinned at the same
    /// version, and fails otherwise.
    pub async fn start_at(&self, id: &str, name: &str, version: u64, input: Value) -> anyhow::Result<SavedRun> {
        if let Some(saved) = self.store.get(id).await? {
            let a = &saved.admission;
            anyhow::ensure!(a.name == name, "run belongs to another definition");
            anyhow::ensure!(a.version == version, "run was admitted at another version");
            return Ok(saved);
        }
        let a = self.admitter().pinned(id, name, version, input).await?;
        if self.store.admit(&a, self.clock.now_ms()).await? {
            self.metrics.started(name);
        }
        self.store.get(id).await?.ok_or_else(|| anyhow::anyhow!("admitted run disappeared"))
    }
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>> {
        self.store.get(id).await
    }
    pub async fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        self.store.list(name, prefix).await
    }
    pub async fn definitions(&self, name: &str) -> anyhow::Result<ouro_durable::Definitions> {
        self.store.definitions(name).await
    }
    pub async fn records(&self, id: &str) -> anyhow::Result<BTreeMap<String, ows::durable::Record>> {
        Ok(self.store.inspect(id).await?.ok_or_else(|| anyhow::anyhow!("unknown run"))?.records)
    }
    /// The run's call journal: every call (and emitted event) it made, with
    /// its delivery and how far it got. The engine does not know whether a
    /// call changed the world; the host decides that from this journal (to
    /// rerun a failed run, hand it over, or reconcile it).
    pub async fn calls(&self, id: &str) -> anyhow::Result<Vec<CallEntry>> {
        Ok(ows::durable::call_journal(&self.records(id).await?))
    }
    /// Delivers an identified CloudEvent (`id`, `source`, `type`; the
    /// `specversion` and `time` are filled in when absent): to `target`'s
    /// mailbox, where a listen takes it until it expires, or to the oldest
    /// run waiting for its type. Delivering it again returns the same
    /// recipient; another payload under its identity is an error. Returns
    /// the recipient, `None` when no run is eligible.
    pub async fn deliver(&self, mut event: Value, target: Option<&str>) -> anyhow::Result<Option<String>> {
        let now = self.clock.now_ms();
        crate::schedule::complete(&mut event, now)?;
        self.store.deliver(&event, target, now).await
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
    pub async fn sweep(&self) -> anyhow::Result<Swept> {
        let swept = self.store.sweep(self.clock.now_ms(), self.config.retention_ms).await?;
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
    /// Execute an attributed operator command. Local cancellations fire tokens
    /// immediately; independent control processes are observed at renewal.
    pub async fn control(
        &self,
        request: &ouro_durable::control::Request,
        command: &ouro_durable::control::Command,
    ) -> Result<ouro_durable::control::Reply, ouro_durable::control::ControlError> {
        use ouro_durable::control::{Command, Reply};
        // A retry of a committed request repeats its reply, not its event.
        let repeated = match command {
            Command::Hold { .. } | Command::Release { .. } => self.store.control_entry(&request.id).await?.is_some(),
            _ => false,
        };
        let now = self.clock.now_ms();
        let reply = self.store.control(request, command, now).await?;
        match (command, &reply) {
            (Command::Cancel { .. }, _) => self.signal_cancelled().await?,
            (Command::Hold { run, note }, Reply::Changed(true)) if !repeated => {
                if let Some(saved) = self.store.get(run).await? {
                    let (actor, reason, note) = (request.actor.clone(), request.reason.clone(), note.clone());
                    self.observers.operated(&saved, now, EventKind::RunHeld { actor, reason, note });
                }
            }
            (Command::Release { run }, Reply::Changed(true)) if !repeated => {
                if let Some(saved) = self.store.get(run).await? {
                    let (actor, reason) = (request.actor.clone(), request.reason.clone());
                    self.observers.operated(&saved, now, EventKind::RunReleased { actor, reason });
                }
            }
            _ => {}
        }
        Ok(reply)
    }
    pub async fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        let changed = self.store.cancel(id, self.clock.now_ms()).await?;
        self.signal_cancelled().await?;
        Ok(changed)
    }
    pub(crate) async fn signal_cancelled(&self) -> anyhow::Result<()> {
        signal_cancelled(&*self.store, &self.executions).await
    }
    pub async fn start_child(
        &self,
        parent: &Fence,
        key: &str,
        id: &str,
        name: &str,
        input: Value,
    ) -> anyhow::Result<SavedRun> {
        let owner = self.store.get(&parent.run_id).await?.ok_or_else(|| anyhow::anyhow!("unknown parent run"))?;
        let a = if let Some(saved) = self.store.get(id).await? {
            anyhow::ensure!(saved.admission.name == name, "child definition changed");
            saved.admission
        } else {
            let declared = model::parse_yaml(&owner.admission.definition)?.budget;
            let lineage = Lineage::of(&owner.admission, &self.config.limits);
            let lineage = Lineage { max_jumps: declared.max_jumps(lineage.max_jumps), ..lineage };
            let child = lineage.child(&self.config.limits).map_err(|e| anyhow::anyhow!("{e}"))?;
            self.admitter().active_in(id, name, input, &child).await?
        };
        let link = ChildLink { parent: parent.run_id.clone(), key: key.into(), reference: key.into() };
        let now = self.clock.now_ms();
        if let Some(f) = self.store.start_child(parent, &a, &link, now).await? {
            let root = owner.root.as_deref().unwrap_or(&owner.admission.id);
            self.observers.admitted(&a, Some(&parent.run_id), root, now);
            self.store.release(&f, self.clock.now_ms()).await?;
        }
        self.store.get(id).await?.ok_or_else(|| anyhow::anyhow!("child disappeared"))
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
        let Some(fence) = self.store.claim_as(id, self.clock.now_ms(), &self.config.application).await? else {
            return Ok(self.store.get(id).await?.and_then(|r| r.outcome));
        };
        let saved = self.store.get(id).await?.ok_or_else(|| anyhow::anyhow!("claimed run disappeared"))?;
        let lease = Arc::new(Lease { store: self.store.clone(), fence, clock: self.clock.clone() });
        let local_cancel = CancellationToken::new();
        let execution_key = (id.to_string(), lease.fence.generation);
        self.executions.lock().unwrap().insert(execution_key.clone(), local_cancel.clone());
        let _execution_guard =
            ExecutionGuard { executions: &self.executions, key: execution_key, token: local_cancel.clone() };
        let events = Arc::new(RunEvents::new(self.observers.clone(), self.clock.clone(), &saved));
        let result = self.execute(&saved, lease.clone(), events.clone(), &local_cancel, stop, mode).await;
        // The drive's end: what this drive committed or found. A terminal
        // state another drive committed (one that took the run over after
        // this one lost its lease) is that drive's to report; a
        // cancellation, which no drive commits, is reported by the drive
        // that finds it.
        match &result {
            Ok(Ending { status: Status::Completed(_) | Status::Faulted(_), foreign: true, .. }) => {
                events.ended(&Status::Interrupted(WorkflowError::runtime("the run ended in another execution")), false)
            }
            Ok(Ending { status, limit_exceeded, .. }) => events.ended(status, *limit_exceeded),
            Err(e) => events.ended(&Status::Interrupted(WorkflowError::runtime(e.to_string())), false),
        }
        result.map(|ending| Some(ending.status))
    }
    async fn execute(
        &self,
        saved: &SavedRun,
        lease: Arc<Lease>,
        events: Arc<RunEvents>,
        local_cancel: &CancellationToken,
        stop: &CancellationToken,
        mode: Mode,
    ) -> anyhow::Result<Ending> {
        // Replay restores the recorded budget decisions, not its private
        // store's: the store saves each one (granted or refused) as it was.
        let replayed = match &mode {
            Mode::Replay { budgets, first, .. } => {
                Some(ReplayJournal { lease: lease.clone(), budgets: budgets.clone(), first: first.clone() })
            }
            _ => None,
        };
        let journal: &dyn ows::durable::Journal = match &replayed {
            Some(replayed) => replayed,
            None => &*lease,
        };
        // The run's records, read once: whether an earlier execution left
        // any decides between started and resumed, and the interpreter
        // starts from the same read.
        let journal = Preloaded::read(journal).await;
        events.began(saved, journal.resumed());
        let root = events.root_context().unwrap_or_else(Context::current);
        // Before the run's next step: every function it can call is still
        // registered (the registry may have changed since admission, by a
        // new release of the host or `set_registry`). A run that cannot
        // proceed fails now, never halfway through.
        let admitter = self.admitter();
        let a = &saved.admission;
        if let Err(e) = admitter.check(&a.name, a.version, &a.definition).await {
            let missing = e.downcast::<MissingFunction>()?;
            return self
                .store
                .finish(
                    &lease.fence,
                    &Status::Faulted(missing.error()),
                    self.clock.now_ms(),
                    self.config.breaker_threshold,
                )
                .await
                .map(Ending::own);
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
                return self
                    .store
                    .finish(
                        &lease.fence,
                        &Status::Interrupted(WorkflowError::validation(e.to_string())),
                        self.clock.now_ms(),
                        self.config.breaker_threshold,
                    )
                    .await
                    .map(Ending::own);
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
                events: events.clone(),
                metrics: self.metrics.clone(),
                definition: (a.name.clone(), a.version),
                function_permits: self.function_permits.clone(),
                lineage,
                mode: mode.clone(),
            }),
            observer: events.clone(),
            clock: Arc::new(InterpreterClock(self.clock.clone())),
            limits,
            // `$runtime`: the descriptor the DSL defines, without the pins;
            // its `schedule` member is `$run.schedule` (`ows::run::Host`).
            runtime: json!({
                "name": runtime["name"],
                "version": runtime["version"],
                "metadata": runtime.get("metadata").cloned().unwrap_or_else(|| json!({})),
                "schedule": runtime.get("schedule").cloned().unwrap_or(Value::Null),
            }),
        };
        // The workflow's deadline counts from admission, whichever process
        // first drives the run.
        let input = saved.admission.input.clone();
        let mut run = Box::pin(run::resume_from(
            &host,
            &wf,
            &saved.admission.id,
            input,
            saved.admitted_ms,
            local_cancel,
            &journal,
        ))
        .with_context(root);
        // The heartbeat is polled together with the run, never instead of
        // it: a store operation the run has started may hold a connection
        // (SQLite's only one) until it is polled to its end. For the same
        // reason the run is dropped, rolling back whatever it has open,
        // before any other store operation here.
        let heartbeat = async {
            let mut ticks = tokio::time::interval(self.config.heartbeat_interval);
            loop {
                ticks.tick().await;
                if let Err(error) = self.store.renew(&lease.fence, self.clock.now_ms()).await {
                    return error;
                }
            }
        };
        let ended = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(None),
            error = heartbeat => Err(Some(error)),
            outcome = &mut run => Ok(outcome),
        };
        let outcome = match ended {
            Ok(outcome) => outcome,
            Err(Some(error)) if error.is::<ouro_durable::control::Held>() => {
                // An operator holds the run. The lease renewal is the
                // fast path of the hold, but a call already dispatched is
                // not retracted: the interpreter goes on, its next fenced
                // step refuses, and the answer of a call in flight is still
                // recorded. It gets one heartbeat for that, well within the
                // lease the last renewal gave it, then the run is dropped
                // like any stopped one and the lease given back.
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => {}
                    _ = tokio::time::sleep(self.config.heartbeat_interval) => {}
                    _ = &mut run => {}
                }
                local_cancel.cancel();
                drop(run);
                return self.yield_held(&lease).await;
            }
            Err(lost) => {
                local_cancel.cancel();
                // Dropping the interpreter closes in-flight spans and
                // handlers. Intent already stored stays outcome-unknown.
                drop(run);
                let Some(error) = lost else {
                    self.store.release(&lease.fence, self.clock.now_ms()).await?;
                    return Ok(Ending::own(Status::Interrupted(WorkflowError::runtime("worker stopped"))));
                };
                if error.is::<ouro_durable::control::Stopped>() {
                    self.store.release(&lease.fence, self.clock.now_ms()).await?;
                }
                if let Some(status) = self.store.get(&saved.admission.id).await?.and_then(|r| r.outcome) {
                    return Ok(Ending { status, foreign: true, limit_exceeded: false });
                }
                return Ok(Ending::own(Status::Interrupted(WorkflowError::runtime(error.to_string()))));
            }
        };
        // Explicit cancel may already have committed and fenced us.
        if let Some(status) = self.store.get(&saved.admission.id).await?.and_then(|r| r.outcome) {
            return Ok(Ending { status, foreign: true, limit_exceeded: false });
        }
        if self.store.stopped().await? {
            self.store.release(&lease.fence, self.clock.now_ms()).await?;
            return Ok(Ending::own(Status::Interrupted(WorkflowError::runtime("runtime emergency stop is set"))));
        }
        match self.store.finish(&lease.fence, &outcome.status, self.clock.now_ms(), self.config.breaker_threshold).await
        {
            Err(e) if e.is::<ouro_durable::control::Held>() => self.yield_held(&lease).await,
            Err(e) if e.is::<ouro_durable::control::Stopped>() => {
                self.store.release(&lease.fence, self.clock.now_ms()).await?;
                Ok(Ending::own(Status::Interrupted(WorkflowError::runtime(e.to_string()))))
            }
            result => result.map(|status| Ending { status, foreign: false, limit_exceeded: outcome.limit_exceeded }),
        }
    }
    /// Gives a held run's lease back: it is Ready again, still held, and
    /// continues from its evidence when released, without spending its
    /// interruption budget. A cancel that ended the run meanwhile is its
    /// outcome; a lease that ended while the run settled is no loss, since
    /// nobody can claim a held run.
    async fn yield_held(&self, lease: &Lease) -> anyhow::Result<Ending> {
        let id = &lease.fence.run_id;
        if let Err(error) = self.store.release(&lease.fence, self.clock.now_ms()).await {
            let saved = self.store.get(id).await?;
            if let Some(status) = saved.as_ref().and_then(|r| r.outcome.clone()) {
                return Ok(Ending { status, foreign: true, limit_exceeded: false });
            }
            let ours = saved.as_ref().is_some_and(|r| {
                r.fence.as_ref() == Some(&lease.fence) && r.lease_until_ms.is_some_and(|at| at > self.clock.now_ms())
            });
            if ours {
                return Err(error);
            }
        }
        Ok(Ending::own(Status::Interrupted(WorkflowError::runtime(format!("run {id} is held by an operator")))))
    }
    pub async fn recover_once(&self, stop: &CancellationToken) -> anyhow::Result<usize> {
        let ids = self.store.due_as(self.clock.now_ms(), self.config.max_concurrency, &self.config.application).await?;
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
        // At most one tick's store work (sweep, due slots, due runs) and one
        // hand-off of notices at a time. Every store operation is a future
        // polled with the runs, never awaited in a branch while they wait: a
        // run suspended inside one may hold a connection (SQLite's only one)
        // that the branch would need. A slow event sink never stalls the
        // runs' heartbeats either.
        let mut ticking: FuturesUnordered<BoxFuture<'_, anyhow::Result<Vec<String>>>> = FuturesUnordered::new();
        let mut notifying: FuturesUnordered<BoxFuture<'_, anyhow::Result<usize>>> = FuturesUnordered::new();
        // The heartbeat row and the backlog gauges, on the lease renewal
        // cadence, one beat at a time.
        let mut beat = tokio::time::interval(self.config.heartbeat_interval);
        let mut beating: FuturesUnordered<BoxFuture<'_, anyhow::Result<()>>> = FuturesUnordered::new();
        let mut ids = std::collections::BTreeSet::new();
        let mut failure = None;
        let shutdown = stop.child_token();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                Some((id, result)) = running.next(), if !running.is_empty() => {
                    ids.remove(&id);
                    if let Err(e) = result {
                        failure = Some(e);
                        break;
                    }
                }
                Some(result) = notifying.next(), if !notifying.is_empty() => {
                    if let Err(e) = result {
                        failure = Some(e);
                        break;
                    }
                }
                Some(result) = beating.next(), if !beating.is_empty() => {
                    if let Err(e) = result {
                        failure = Some(e);
                        break;
                    }
                }
                _ = beat.tick() => {
                    if beating.is_empty() {
                        beating.push(Box::pin(async move {
                            self.heartbeat().await?;
                            self.observe_backlog().await
                        }));
                    }
                }
                Some(result) = ticking.next(), if !ticking.is_empty() => {
                    let due = match result {
                        Ok(due) => due,
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    };
                    for id in due {
                        if ids.len() >= self.config.max_concurrency {
                            break;
                        }
                        if ids.insert(id.clone()) {
                            let token = shutdown.clone();
                            running.push(Box::pin(async move {
                                let result = self.drive(&id, &token).await;
                                (id, result)
                            }));
                        }
                    }
                }
                _ = tick.tick() => {
                    if ticking.is_empty() {
                        let swept = sweep.elapsed() >= Duration::from_secs(3600);
                        if swept {
                            sweep = tokio::time::Instant::now();
                        }
                        ticking.push(Box::pin(async move {
                            if swept {
                                self.sweep().await?;
                            }
                            // Due schedule slots: a range of the trigger index ending now.
                            self.fire_due().await?;
                            self.store.due_as(self.clock.now_ms(), self.config.max_concurrency, &self.config.application).await
                        }));
                    }
                    // Breaker trips and other notices, handed on to the host.
                    if notifying.is_empty() {
                        let token = shutdown.clone();
                        notifying.push(Box::pin(async move { self.notify(&token).await }));
                    }
                }
            }
        }
        shutdown.cancel();
        // Drained together, for the same reason they ran together.
        let (runs, ticks, notices, beats) = futures::join!(
            async {
                let mut failure = None;
                while let Some((_, result)) = running.next().await {
                    failure = result.err().or(failure);
                }
                failure
            },
            async {
                let mut failure = None;
                while let Some(result) = ticking.next().await {
                    failure = result.err().or(failure);
                }
                failure
            },
            async {
                let mut failure = None;
                while let Some(result) = notifying.next().await {
                    failure = result.err().or(failure);
                }
                failure
            },
            async {
                let mut failure = None;
                while let Some(result) = beating.next().await {
                    failure = result.err().or(failure);
                }
                failure
            },
        );
        if let Some(e) = beats.or(notices).or(ticks).or(runs).or(failure) {
            return Err(e);
        }
        // A worker that stopped cleanly is stale at once, holding nothing.
        let now = self.clock.now_ms();
        self.beat(now, now).await
    }
}
/// How a drive ended: the status it returns, whether another execution
/// committed it (a takeover after this drive lost its lease, an operator's
/// or parent's cancellation), and whether the run ended through its jump
/// budget.
struct Ending {
    status: Status,
    foreign: bool,
    limit_exceeded: bool,
}
impl Ending {
    fn own(status: Status) -> Self {
        Ending { status, foreign: false, limit_exceeded: false }
    }
}

/// A journal whose first load returns the records read before the run
/// started (`Preloaded::read`), so the drive and the interpreter share one
/// read of them.
struct Preloaded<'a> {
    journal: &'a dyn ows::durable::Journal,
    first: Mutex<Option<anyhow::Result<BTreeMap<String, ows::durable::Record>>>>,
    resumed: bool,
}
impl<'a> Preloaded<'a> {
    async fn read(journal: &'a dyn ows::durable::Journal) -> Self {
        let first = journal.load().await;
        // Unreadable records count as an earlier execution's.
        let resumed = first.as_ref().map_or(true, |records| !records.is_empty());
        Preloaded { journal, first: Mutex::new(Some(first)), resumed }
    }
    fn resumed(&self) -> bool {
        self.resumed
    }
}
#[async_trait]
impl ows::durable::Journal for Preloaded<'_> {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, ows::durable::Record>> {
        let first = self.first.lock().unwrap().take();
        match first {
            Some(first) => first,
            None => self.journal.load().await,
        }
    }
    async fn save(&self, key: &str, record: &ows::durable::Record) -> anyhow::Result<()> {
        self.journal.save(key, record).await
    }
    async fn spend(&self, key: &str, limit: u64) -> anyhow::Result<Option<ows::durable::Record>> {
        self.journal.spend(key, limit).await
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
    /// What a refused trigger run counts against (`Admitter::triggered`).
    pub breaker_threshold: u32,
    pub max_concurrent_runs: usize,
    /// The runtime's metrics and observers, for the schedules an event fires.
    pub metrics: Arc<Metrics>,
    pub observers: Arc<Observers>,
    executions: Arc<Mutex<BTreeMap<(String, u64), CancellationToken>>>,
}

/// A definition this host cannot admit a run of because it no longer
/// validates against the host (its registry, capabilities, ceilings or
/// resolved external schemas): `start` returns it, a trigger records it
/// (`Refused`).
/// It reads as the validation error it wraps.
#[derive(Debug)]
pub struct InvalidDefinition(pub anyhow::Error);
impl InvalidDefinition {
    /// The fault a refused trigger run ends with.
    pub fn error(&self) -> WorkflowError {
        WorkflowError::validation(format!("{:#}", self.0))
    }
}
impl std::fmt::Display for InvalidDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() { write!(f, "{:#}", self.0) } else { write!(f, "{}", self.0) }
    }
}
impl std::error::Error for InvalidDefinition {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.chain().nth(1)
    }
}

/// The host's resource resolver failed to resolve an external schema
/// (`schema.resource`) now: an infrastructure interruption, never a property
/// of the definition. `start` returns it; a trigger neither consumes its
/// slot nor counts it toward the breaker, and tries the slot again after a
/// backoff. (A resolved document that is not a JSON Schema, or no resolver
/// at all, is the definition failing validation on this host:
/// `InvalidDefinition`.)
#[derive(Debug)]
pub struct ResourceUnavailable {
    pub uri: String,
    pub error: String,
}
impl std::fmt::Display for ResourceUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "schema {}: {}", self.uri, self.error)
    }
}
impl std::error::Error for ResourceUnavailable {}

/// Why this host refuses to admit a run of a definition, as the fault a
/// trigger records: a function it calls is not registered, or it does not
/// validate. Deterministic only: `None` for every other error (storage, a
/// tripped definition, `ResourceUnavailable`), which the caller handles as
/// an interruption.
pub(crate) fn refusal(e: &anyhow::Error) -> Option<WorkflowError> {
    if let Some(missing) = e.downcast_ref::<MissingFunction>() {
        return Some(missing.error());
    }
    e.downcast_ref::<InvalidDefinition>().map(InvalidDefinition::error)
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

/// Local handlers see terminal cancellation immediately; remote workers see
/// it at their next fenced operation or renewal, as for an explicit cancel.
async fn signal_cancelled(
    store: &dyn RunStore,
    executions: &Mutex<BTreeMap<(String, u64), CancellationToken>>,
) -> anyhow::Result<()> {
    let running: Vec<(String, CancellationToken)> =
        executions.lock().unwrap().iter().map(|((id, _), token)| (id.clone(), token.clone())).collect();
    for (id, token) in running {
        if store.get(&id).await?.is_some_and(|r| r.ended_ms.is_some()) {
            token.cancel();
        }
    }
    Ok(())
}

impl Admitter {
    pub(crate) async fn signal_cancelled(&self, store: &dyn RunStore) -> anyhow::Result<()> {
        signal_cancelled(store, &self.executions).await
    }
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
            let document = resolver
                .resolve(&uri)
                .map_err(|e| anyhow::Error::new(ResourceUnavailable { uri: uri.clone(), error: format!("{e:#}") }))?;
            jsonschema::validator_for(&document)
                .map_err(|e| anyhow::anyhow!("schema {uri} is not a JSON Schema: {e}"))?;
            out.insert(uri, document);
        }
        Ok(out)
    }
    /// A root run of `name`'s active version, unless its breaker tripped.
    pub async fn active(&self, id: &str, name: &str, input: Value) -> anyhow::Result<Admission> {
        self.active_in(id, name, input, &self.root()).await
    }
    /// A root run of the stored version `version` of `name`; the store refuses it
    /// iff the definition is tripped on that version's content,
    /// marked as pinned (`Runtime::start_at`).
    pub async fn pinned(&self, id: &str, name: &str, version: u64, input: Value) -> anyhow::Result<Admission> {
        let mut definitions = self.definitions.definitions(name).await?;
        let d = definitions
            .versions
            .remove(&version)
            .ok_or_else(|| anyhow::anyhow!("unknown definition version {version} of {name}"))?;
        let mut admission = self.of(id, &d, input, &self.root()).await?;
        admission.runtime[ouro_durable::PINNED] = json!(true);
        Ok(admission)
    }
    fn root(&self) -> Lineage {
        Lineage { max_jumps: self.limits.max_jumps, depth: 0 }
    }
    /// `active` at a place in a run tree.
    pub async fn active_in(&self, id: &str, name: &str, input: Value, lineage: &Lineage) -> anyhow::Result<Admission> {
        self.of(id, &self.current(name).await?, input, lineage).await
    }
    /// `name`'s active version, unless its breaker tripped.
    async fn current(&self, name: &str) -> anyhow::Result<Definition> {
        let mut definitions = self.definitions.definitions(name).await?;
        if let Some(trip) = &definitions.tripped {
            return Err(trip.refusal(name).into());
        }
        definitions
            .active
            .and_then(|v| definitions.versions.remove(&v))
            .ok_or_else(|| anyhow::anyhow!("no active definition for {name}"))
    }
    /// A run of `d`: checked (`MissingFunction`), validated
    /// (`InvalidDefinition`) and bound.
    pub async fn of(&self, id: &str, d: &Definition, input: Value, lineage: &Lineage) -> anyhow::Result<Admission> {
        self.check(&d.name, d.version, &d.yaml).await?;
        // A resolver that fails now is no property of the definition.
        let invalid =
            |e: anyhow::Error| if e.is::<ResourceUnavailable>() { e } else { anyhow::Error::new(InvalidDefinition(e)) };
        let wf = self.validate(&d.yaml).map_err(invalid)?;
        let schemas = self.schemas(&wf).map_err(invalid)?;
        Ok(self.admission(id, d, input, lineage, Some((self.registry.bindings(&wf), schemas))).await)
    }
    /// The admission of a run of `d`, with its bindings (function schemas
    /// and delivery, external schemas) when it has them: a refused run is
    /// recorded without.
    async fn admission(
        &self,
        id: &str,
        d: &Definition,
        input: Value,
        lineage: &Lineage,
        bound: Option<(Value, serde_json::Map<String, Value>)>,
    ) -> Admission {
        let mut runtime = json!({
            "name": "ouro-runtime",
            "version": env!("CARGO_PKG_VERSION"),
            "metadata": self.metadata,
        });
        if let Some((bindings, schemas)) = bound {
            runtime["bindings"] = bindings;
            runtime["schemas"] = Value::Object(schemas);
        }
        runtime["limits"] = lineage.pin();
        if let Some(version) = &self.application {
            runtime[ouro_durable::APPLICATION_VERSION] = json!(version);
        }
        crate::telemetry::pin_trace(&mut runtime);
        Admission {
            id: id.into(),
            name: d.name.clone(),
            version: d.version,
            definition: d.yaml.clone(),
            input,
            runtime,
        }
    }
    /// A trigger's root run of `name`'s active version: its admission, or,
    /// when this host refuses it (`refusal`: a missing function, an invalid
    /// definition), the `Refused` the trigger records with its slot instead.
    /// A tripped definition, no active version and storage errors remain
    /// errors.
    ///
    /// `schedule` is what the trigger tells the run (`$run.schedule`:
    /// `{ kind, slot, previous }`), kept in the admission's runtime like its
    /// bindings, so a recovered or replayed run sees the same one.
    pub async fn triggered(
        &self,
        id: &str,
        name: &str,
        input: Value,
        schedule: Value,
    ) -> anyhow::Result<Result<Admission, ouro_durable::Refused>> {
        let d = self.current(name).await?;
        match self.of(id, &d, input.clone(), &self.root()).await {
            Ok(mut admission) => {
                admission.runtime["schedule"] = schedule;
                Ok(Ok(admission))
            }
            Err(e) => match refusal(&e) {
                Some(error) => {
                    let mut admission = self.admission(id, &d, input, &self.root(), None).await;
                    admission.runtime["schedule"] = schedule;
                    Ok(Err(ouro_durable::Refused { admission, error, breaker_threshold: self.breaker_threshold }))
                }
                None => Err(e),
            },
        }
    }
    /// The start check: every function the definition can call is
    /// registered, directly or in the child workflows its `run: workflow`
    /// tasks name, transitively, as they resolve now. A missing one is a
    /// `MissingFunction` (inside the error) naming it and the definitions
    /// that lead to it; a child that does not resolve is left to the run,
    /// which faults on it as it always has. Each definition version's own
    /// needs are cached per registry generation; resolving its children
    /// reads their names' catalog entries.
    pub async fn check(&self, name: &str, version: u64, yaml: &str) -> anyhow::Result<()> {
        let mut path = Vec::new();
        self.check_tree((name, version, yaml), &mut path, &mut BTreeSet::new()).await
    }
    fn check_tree<'a>(
        &'a self,
        (name, version, yaml): (&'a str, u64, &'a str),
        path: &'a mut Vec<(String, u64)>,
        seen: &'a mut BTreeSet<(String, u64)>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
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
                if let Some(d) = self.target(&ChildTarget { namespace, name: child, version }).await? {
                    self.check_tree((&d.name, d.version, &d.yaml), path, seen).await?;
                }
            }
            path.pop();
            Ok(())
        })
    }
    /// `run.workflow`'s target. CHOICE: the catalog entry named `name`
    /// holds the workflow; the version is the active one for `latest`, else
    /// the newest saved version whose document has that semantic version.
    /// Either way the document's namespace must match. `None`: no such one.
    pub async fn target(&self, target: &ChildTarget<'_>) -> anyhow::Result<Option<Definition>> {
        let definitions = self.definitions.definitions(target.name).await?;
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
    /// The drive's events, and through them the call spans.
    pub events: Arc<RunEvents>,
    pub metrics: Arc<Metrics>,
    /// The run's definition name and version, for its calls' context.
    pub definition: (String, u64),
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
        // The call's span, which the OpenTelemetry observer opened when the
        // interpreter reported its dispatch.
        let span = self.events.call_context(info.reference).unwrap_or_else(Context::current);
        let (definition, version) = &self.definition;
        let context = FunctionContext {
            cancel: info.cancel.clone(),
            run_id: info.run_id.into(),
            task_path: info.reference.into(),
            attempt: info.attempt,
            idempotency_key: serde_json::to_string(&(info.run_id, info.logical)).expect("string tuple serializes"),
            span,
            input: info.input.clone(),
            function: name.into(),
            definition: definition.clone(),
            version: *version,
            delivery: info.delivery,
            meter: self.metrics.meter.clone(),
        };
        let result = async {
            if let Mode::Replay { .. } = &self.mode {
                return self.replayed(name, &args, info);
            }
            if crate::registry::builtin(name) {
                return self.builtin(name, args, info).await.map(Output::new).map_err(|e| {
                    match e.downcast::<Refused>() {
                        Ok(Refused(error)) => CallFailure::NotExecuted(error),
                        Err(e) => CallFailure::Interrupted(WorkflowError::runtime(e.to_string())),
                    }
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
            self.lease
                .store
                .check_lease(&self.lease.fence, self.lease.clock.now_ms())
                .await
                .map_err(|e| CallFailure::Interrupted(WorkflowError::runtime(e.to_string())))?;
            let output = match &self.mode {
                // The dry run's guarantee: a call the host's rule marks is
                // answered here and never reaches its handler or runner.
                Mode::Dry { mock, mocked } if mock.mocks(name, &args) => {
                    let output = mock.response(name, &args, &context);
                    mocked.lock().unwrap().push(RecordedCall {
                        task_path: info.reference.into(),
                        logical: info.logical.into(),
                        function: name.into(),
                        arguments: args.clone(),
                        delivery: info.delivery,
                        result: output.as_ref().map(|o| o.value.clone()).map_err(Clone::clone),
                        annotations: output.as_ref().map(|o| o.annotations.clone()).unwrap_or_default(),
                    });
                    output
                }
                Mode::Live | Mode::Dry { .. } => match (registered, process, runner) {
                    (Some(f), _, _) => {
                        f.function.handler.call(args, context.clone()).with_context(context.span.clone()).await
                    }
                    (None, Some(kind), Some(runner)) => runner
                        .run(kind, args, context.clone())
                        .with_context(context.span.clone())
                        .await
                        .map(Output::new),
                    _ => Err(WorkflowError::runtime(format!("no runner for {name}"))),
                },
                Mode::Replay { .. } => unreachable!("replay returns before dispatch"),
            }
            .map_err(CallFailure::Response)?;
            if let Some(f) = registered {
                f.output
                    .validate(&output.value)
                    .map_err(|e| CallFailure::Response(WorkflowError::validation(format!("{name} output: {e}"))))?;
            }
            ows::durable::check_annotations(&output.annotations)
                .map_err(|e| CallFailure::Response(WorkflowError::validation(format!("{name} annotations: {e}"))))?;
            Ok(output)
        }
        .await;
        result.map(|output| {
            // The interpreter saves them with the result.
            info.annotations.extend(output.annotations);
            output.value
        })
    }
    /// A recorded call's result. Records hold redacted values, so the
    /// comparison is between redacted forms and the result gets its
    /// secrets back.
    fn replayed(&self, name: &str, args: &Value, info: &CallInfo<'_>) -> Result<Output, CallFailure> {
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
            Ok(v) => {
                let restore = |v: &Value| info.secrets.restore_value(v).map_err(WorkflowError::runtime);
                let annotations = serde_json::to_value(&call.annotations)
                    .map_err(|e| WorkflowError::runtime(e.to_string()))
                    .and_then(|a| restore(&a))
                    .and_then(|a| serde_json::from_value(a).map_err(|e| WorkflowError::runtime(e.to_string())));
                match (restore(v), annotations) {
                    (Ok(value), Ok(annotations)) => Ok(Output { value, annotations }),
                    (Err(e), _) | (_, Err(e)) => Err(CallFailure::NotExecuted(e)),
                }
            }
            Err(e) => {
                // An error answer keeps what it carried, as it was recorded.
                info.annotations.extend(call.annotations.clone());
                Err(CallFailure::Response(e.clone()))
            }
        }
    }
    async fn builtin(&self, name: &str, args: Value, info: &CallInfo<'_>) -> anyhow::Result<Value> {
        let store = &self.lease.store;
        if name == "child_outcome" {
            let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("child_outcome requires id"))?;
            let key = args["key"].as_str().ok_or_else(|| anyhow::anyhow!("child_outcome requires key"))?;
            return Ok(serde_json::to_value(store.child_outcome(info.run_id, key, id).await?)?);
        }
        let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("start_child requires name"))?;
        let id = child_id(info.run_id, info.logical);
        // Replay joins the pinned child even if its active definition changed.
        let a = if let Some(saved) = store.get(&id).await? {
            anyhow::ensure!(saved.admission.name == name, "child definition changed");
            saved.admission
        } else {
            let lineage = self.lineage.child(&self.admitter.limits).map_err(Refused)?;
            self.admitter.active_in(&id, name, args.get("input").cloned().unwrap_or(Value::Null), &lineage).await?
        };
        let now = self.lease.clock.now_ms();
        let link = ChildLink { parent: info.run_id.into(), key: info.logical.into(), reference: info.reference.into() };
        if let Some(fence) = store.start_child(&self.lease.fence, &a, &link, now).await? {
            self.events.observers().admitted(&a, Some(info.run_id), self.events.root(), now);
            store.release(&fence, now).await?;
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
#[async_trait]
impl ows::durable::Journal for ReplayJournal {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, ows::durable::Record>> {
        self.lease.load().await
    }
    async fn save(&self, key: &str, record: &ows::durable::Record) -> anyhow::Result<()> {
        self.lease.save(key, record).await
    }
    async fn spend(&self, key: &str, limit: u64) -> anyhow::Result<Option<ows::durable::Record>> {
        let limit = match self.budgets.get(key) {
            Some(true) => u64::MAX,
            Some(false) => {
                if let Some(first) = self.first.as_deref().filter(|first| *first != key) {
                    self.lease.spend(first, 0).await?;
                }
                0
            }
            None => limit,
        };
        self.lease.spend(key, limit).await
    }
}

/// A child's run id: its parent and the invocation that started it.
fn child_id(run_id: &str, key: &str) -> String {
    format!("child:{}", serde_json::to_string(&(run_id, key)).expect("string tuple serializes"))
}
#[async_trait]
impl run::Events for ExecutionFunctions {
    async fn waiting(&self, id: &str, key: &str, wait: &ows::events::PendingWait) -> anyhow::Result<()> {
        run::Events::waiting(&*self.lease, id, key, wait).await
    }

    async fn register(&self, id: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::register(&*self.lease, id, types).await
    }
    async fn unregister(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::unregister(&*self.lease, id, key, types).await
    }
    async fn subscribe(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        run::Events::subscribe(&*self.lease, id, key, types).await
    }
    async fn unsubscribe(&self, id: &str, key: &str) -> anyhow::Result<()> {
        run::Events::unsubscribe(&*self.lease, id, key).await
    }
    async fn take(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        if let Mode::Replay { events, .. } = &self.mode
            && let Some(event) = events.get(key)
        {
            // The recorded envelope, as it was taken, to this run only.
            let time = event["time"].as_str().ok_or_else(|| anyhow::anyhow!("missing recorded event time"))?;
            let delivered_at = chrono::DateTime::parse_from_rfc3339(time)?.timestamp_millis();
            self.lease.store.publish(event, Some(id), delivered_at).await?;
        }
        run::Events::take(&*self.lease, id, key, types).await
    }
    fn notification(&self, id: &str) -> BoxFuture<'_, ()> {
        run::Events::notification(&*self.lease, id)
    }
}
#[async_trait]
impl run::Children for ExecutionFunctions {
    async fn start(
        &self,
        run_id: &str,
        key: &str,
        target: &ChildTarget<'_>,
        input: &Value,
        owned: bool,
    ) -> Result<Child, CallFailure> {
        let interrupted = |e: anyhow::Error| CallFailure::Interrupted(WorkflowError::runtime(e.to_string()));
        // A tripped definition admits no child: a definite, typed answer the
        // parent may catch, never a retry. Other storage errors interrupt.
        let refused = |e: anyhow::Error| match e.downcast_ref::<DefinitionTripped>() {
            Some(tripped) => CallFailure::NotExecuted(tripped.error()),
            None => interrupted(e),
        };
        let store = &self.lease.store;
        let id = child_id(run_id, key);
        let done = format!("done:{id}");
        // A started child keeps the version it was admitted with.
        let a = match store.get(&id).await.map_err(interrupted)? {
            Some(saved) => saved.admission,
            None => {
                // An independent run starts a tree of its own, one level
                // deeper, so recursion through either kind of child ends.
                let lineage = self.lineage.child(&self.admitter.limits).map_err(CallFailure::NotExecuted)?;
                let lineage =
                    if owned { lineage } else { Lineage { max_jumps: self.admitter.limits.max_jumps, ..lineage } };
                let d = self.admitter.target(target).await.map_err(interrupted)?.ok_or_else(|| {
                    CallFailure::NotExecuted(WorkflowError::standard(
                        "configuration",
                        400,
                        "Configuration Error",
                        format!("no workflow {}/{} version {} to run", target.namespace, target.name, target.version),
                    ))
                })?;
                self.admitter.of(&id, &d, input.clone(), &lineage).await.map_err(|e| {
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
            if store.admit(&a, now).await.map_err(refused)? {
                self.events.observers().admitted(&a, None, &id, now);
            }
            return Ok(Child { id, done });
        }
        let link = ChildLink { parent: run_id.into(), key: key.into(), reference: key.into() };
        if let Some(fence) = store.start_child(&self.lease.fence, &a, &link, now).await.map_err(refused)? {
            self.events.observers().admitted(&a, Some(run_id), self.events.root(), now);
            store.release(&fence, now).await.map_err(interrupted)?;
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
    fn secrets(&self) -> Option<&dyn ows::secrets::Secrets> {
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
                    annotations: Annotations::new(),
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
                self.lease
                    .store
                    .check_lease(&self.lease.fence, now)
                    .await
                    .map_err(|e| CallFailure::Interrupted(WorkflowError::runtime(e.to_string())))?;
                // A storage failure is no answer: the run stops recoverably and
                // recovery publishes the same envelope again (deduplicated).
                crate::schedule::publish(&*self.lease.store, &self.admitter, event, now)
                    .await
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
