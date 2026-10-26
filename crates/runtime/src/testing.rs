//! Isolated dry runs and replay. Both use a private in-memory store. A dry
//! run dispatches every call except those its host's mock rule marks, which
//! the mock answers; replay dispatches nothing and takes results from
//! durable call records.
use crate::{FunctionContext, FunctionError, Observer, Output, Runtime};
use ouro_durable::{Clock, RunStore};
use ows::{
    durable::{Annotations, CallEntry, CallState, CallStatus, Delivery, Record},
    run::{Status, WorkflowError},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// A dry run's host decides what must not happen for real: the engine does
/// not know which functions change the world. Guarantee: a dry run
/// dispatches no call (function or process run, `run:<kind>`) its `mocks`
/// marks; `response` answers it instead. Every other call is dispatched.
pub trait Mock: Send + Sync {
    /// Whether the call is mocked rather than dispatched.
    fn mocks(&self, function: &str, arguments: &Value) -> bool;
    /// The answer to a mocked call, with the annotations a real one would
    /// carry (a model step's token counts): the dry run keeps them.
    fn response(&self, function: &str, arguments: &Value, context: &FunctionContext) -> Result<Output, FunctionError>;
}
/// A `Mock` from the host's rule and the answers to what it marks (a
/// `Value` or an annotated `Output`).
pub fn mock<O: Into<Output> + 'static>(
    rule: impl Fn(&str, &Value) -> bool + Send + Sync + 'static,
    response: impl Fn(&str, &Value, &FunctionContext) -> Result<O, FunctionError> + Send + Sync + 'static,
) -> Arc<dyn Mock> {
    struct Rule<R, F, O>(R, F, std::marker::PhantomData<fn() -> O>);
    impl<R, F, O> Mock for Rule<R, F, O>
    where
        R: Fn(&str, &Value) -> bool + Send + Sync,
        F: Fn(&str, &Value, &FunctionContext) -> Result<O, FunctionError> + Send + Sync,
        O: Into<Output>,
    {
        fn mocks(&self, f: &str, a: &Value) -> bool {
            (self.0)(f, a)
        }
        fn response(&self, f: &str, a: &Value, c: &FunctionContext) -> Result<Output, FunctionError> {
            (self.1)(f, a, c).map(Into::into)
        }
    }
    Arc::new(Rule(rule, response, std::marker::PhantomData))
}
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub task_path: String,
    pub logical: String,
    pub function: String,
    pub arguments: Value,
    pub delivery: Delivery,
    pub result: Result<Value, WorkflowError>,
    /// The annotations the answer carried.
    pub annotations: Annotations,
}
/// What a dry run did. `#[non_exhaustive]`: it grows as dry runs report more.
#[non_exhaustive]
pub struct DryRun {
    pub status: Status,
    /// The calls the mock answered and the events the run emitted (as
    /// `emit`; a dry run publishes none), in the order they happened. A
    /// delivery through run access (`FunctionContext::deliver`) is no
    /// mocked call: it shows when a `listen` takes it, in the receiving
    /// run's records and as `EventConsumed` to the dry run's observer.
    pub mocked: Vec<RecordedCall>,
    /// The root's call journal, mocked and dispatched calls alike, with
    /// their annotations (`annotation_totals` sums what it would cost);
    /// equals `runs[0].calls` (kept for compatibility).
    pub calls: Vec<CallEntry>,
    /// Every run the dry run admitted, in admission order (the root first).
    pub runs: Vec<DryRunRun>,
}
/// A run a dry run admitted (`DryRun::runs`).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct DryRunRun {
    pub id: String,
    /// The parent run and `ChildLink::key` (as `ChildRef::key`, what
    /// `child_outcome` takes); `None` for the root and for `await: false` runs.
    pub parent: Option<String>,
    pub key: Option<String>,
    /// True for a run started by `run: workflow` with `await: false`: a root
    /// of its own, not the dry run's root.
    pub independent: bool,
    pub name: String,
    pub version: u64,
    /// The content digest of its definition (`ouro_durable::digest_of`).
    pub digest: String,
    /// `None`: still live when the dry run ended (it waits, or was stopped).
    /// Owned descendants live when the root ended were cancelled with it.
    pub outcome: Option<Status>,
    pub calls: Vec<CallEntry>,
}
/// How to dry-run (`Runtime::dry_run_opts`). Exhaustive, so a host writes
/// `DryRunOptions { observer, ..Default::default() }`.
#[derive(Default)]
pub struct DryRunOptions {
    /// The root's run id and definition name in the private store;
    /// `dry-run` for both when `None`, whatever the YAML's `document.name`.
    pub id: Option<String>,
    pub name: Option<String>,
    /// Host data for the root (`Runtime::start_with`).
    pub host: Option<Value>,
    /// Hears every event of this dry run's runs, root and children, root
    /// admission included, with payloads as a live observer gets them;
    /// nothing else of the host does.
    pub observer: Option<Arc<dyn Observer>>,
}
pub struct Replay {
    pub status: Status,
    pub calls_replayed: usize,
    /// The replayed run's call journal: the recorded results and annotations.
    pub calls: Vec<CallEntry>,
}
#[derive(Clone)]
pub(crate) enum Mode {
    Live,
    Dry {
        mock: Arc<dyn Mock>,
        mocked: Arc<Mutex<Vec<RecordedCall>>>,
    },
    Replay {
        /// The recorded calls by reference, with how far each got: an
        /// answer, a proven non-execution, or no answer (unknown).
        calls: Arc<BTreeMap<String, (RecordedCall, CallStatus)>>,
        used: Arc<Mutex<BTreeSet<String>>>,
        events: Arc<BTreeMap<String, Value>>,
        /// The recorded jump decisions by key: granted or refused.
        budgets: Arc<BTreeMap<String, bool>>,
        /// The recorded first refusal's key, the one that acted.
        first: Option<String>,
    },
}
/// A test clock advances durable waits instantly during isolated execution.
struct TestClock(std::sync::atomic::AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl TestClock {
    fn advance(&self, time: i64) {
        self.0.fetch_max(time, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Runtime {
    /// `dry_run_opts` with the default options.
    pub async fn dry_run(
        &self,
        yaml: &str,
        input: Value,
        mock: Arc<dyn Mock>,
        stop: &CancellationToken,
    ) -> anyhow::Result<DryRun> {
        self.dry_run_opts(yaml, input, mock, DryRunOptions::default(), stop).await
    }
    /// `dry_run_opts` with host data for its root (`Runtime::start_with`).
    pub async fn dry_run_with(
        &self,
        yaml: &str,
        input: Value,
        host: Option<Value>,
        mock: Arc<dyn Mock>,
        stop: &CancellationToken,
    ) -> anyhow::Result<DryRun> {
        self.dry_run_opts(yaml, input, mock, DryRunOptions { host, ..DryRunOptions::default() }, stop).await
    }
    /// Runs `yaml` and every run it starts in a private in-memory store on
    /// a virtual clock: calls the mock rule marks are answered by `mock`,
    /// every other call is dispatched. Waits, listen timeouts and holds are
    /// cut to nothing; the driver drives each due run in turn until the root
    /// ends, nothing is due, or `stop`. Handlers reach the private store
    /// through their run access (`FunctionContext::run`, `deliver`).
    pub async fn dry_run_opts(
        &self,
        yaml: &str,
        input: Value,
        mock: Arc<dyn Mock>,
        options: DryRunOptions,
        stop: &CancellationToken,
    ) -> anyhow::Result<DryRun> {
        self.validate(yaml)?;
        let id = options.id.unwrap_or_else(|| "dry-run".into());
        let name = options.name.unwrap_or_else(|| "dry-run".into());
        let store = Arc::new(ouro_durable::memory::memory());
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(self.clock.now_ms())));
        let mut config = self.config.clone();
        config.limits.max_sleep = Some(Duration::ZERO);
        config.limits.max_listen = Some(Duration::ZERO);
        config.limits.max_hold = Some(Duration::ZERO);
        let mut runtime = Runtime::new(
            store.clone(),
            self.registry(),
            config,
            clock.clone(),
            opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new())),
        )?;
        runtime.capabilities = self.capabilities.clone();
        // Traced with the host's tracer, unless the host removed its
        // OpenTelemetry observer; counted nowhere, and no observer of the
        // host's hears it: a dry run is no run of the host. Its own observer
        // hears all of it, from the root's admission on.
        runtime.tracer = self.tracer.clone();
        let admissions = Arc::new(Mutex::new(Vec::new()));
        runtime.observers = Arc::new(crate::observer::Observers {
            telemetry: self
                .observers
                .telemetry
                .as_ref()
                .map(|_| Arc::new(crate::telemetry::Telemetry::new(self.tracer.clone(), runtime.metrics.clone()))),
            host: options.observer.into_iter().collect(),
            payload_bytes: self.config.event_payload_bytes,
            admissions: Some(admissions.clone()),
        });
        let d = runtime.save(&name, yaml).await?;
        runtime.activate(&d.name, d.version).await?;
        let start = crate::Start { version: None, host: options.host.unwrap_or(Value::Null) };
        runtime.start_with(&id, &name, input, start).await?;
        // Children's definitions come from the host's catalog.
        runtime.definition_source = self.store.clone();
        let mocked = Arc::new(Mutex::new(Vec::new()));
        let status = run_isolated(&runtime, &id, Mode::Dry { mock, mocked: mocked.clone() }, &clock, stop).await?;
        let mocked = mocked.lock().unwrap().clone();
        let admitted = admissions.lock().unwrap().clone();
        let mut runs = Vec::with_capacity(admitted.len());
        for run_id in admitted {
            let saved = runtime.get(&run_id).await?.ok_or_else(|| anyhow::anyhow!("a dry run's run disappeared"))?;
            let link = saved.child;
            runs.push(DryRunRun {
                independent: link.is_none() && run_id != id,
                parent: link.as_ref().map(|l| l.parent.clone()),
                key: link.map(|l| l.key),
                name: saved.admission.name,
                version: saved.admission.version,
                digest: ouro_durable::digest_of(&saved.admission.definition),
                outcome: saved.outcome,
                calls: runtime.calls(&run_id).await?,
                id: run_id,
            });
        }
        let calls = runs.first().map(|root| root.calls.clone()).unwrap_or_default();
        Ok(DryRun { status, mocked, calls, runs })
    }
    pub async fn replay(&self, id: &str, stop: &CancellationToken) -> anyhow::Result<Replay> {
        self.replay_inner(id, None, stop).await
    }
    /// Exercise a candidate definition using a recorded run's input and
    /// function results. Changed call names, arguments or delivery fail replay.
    pub async fn replay_definition(&self, yaml: &str, id: &str, stop: &CancellationToken) -> anyhow::Result<Replay> {
        self.validate(yaml)?;
        self.replay_inner(id, Some(yaml), stop).await
    }
    async fn replay_inner(&self, id: &str, yaml: Option<&str>, stop: &CancellationToken) -> anyhow::Result<Replay> {
        let mut original = self.store.inspect(id).await?.ok_or_else(|| anyhow::anyhow!("unknown recorded run"))?;
        let mut calls = BTreeMap::new();
        let events: BTreeMap<_, _> = original
            .records
            .iter()
            .filter_map(|(k, r)| if let Record::Event { event } = r { Some((k.clone(), event.clone())) } else { None })
            .collect();
        let budgets: BTreeMap<_, _> = original
            .records
            .iter()
            .filter_map(
                |(k, r)| if let Record::Budget { granted, .. } = r { Some((k.clone(), *granted)) } else { None },
            )
            .collect();
        let first = original.records.values().find_map(|r| match r {
            Record::Exceeded { by } => Some(by.clone()),
            _ => None,
        });
        if let Some(yaml) = yaml {
            let wf = self.validate(yaml)?;
            original.run.admission.definition = yaml.into();
            original.run.admission.runtime["bindings"] = self.registry().bindings(&wf);
        }
        for (key, record) in original.records {
            let Some(status) = CallEntry::of(&key, &record).map(|entry| entry.state) else { continue };
            let Record::Call { logical, function, arguments, delivery, state, annotations } = record else { continue };
            // A call without an answer is replayed as the unknown outcome
            // it was: an at-most-once one takes the branch the recorded run
            // took (`call.unknown`), and nothing is dispatched for it.
            let result = match state {
                CallState::Done(result) => result,
                CallState::NotExecuted(error) => Err(error),
                CallState::Pending => Err(WorkflowError::unknown_outcome("the recorded call has no answer")),
            };
            let reference = key.strip_suffix("#call").ok_or_else(|| anyhow::anyhow!("invalid call record key"))?;
            calls.insert(
                reference.to_string(),
                (
                    RecordedCall {
                        task_path: reference.into(),
                        logical,
                        function,
                        arguments,
                        delivery,
                        result,
                        annotations,
                    },
                    status,
                ),
            );
        }
        anyhow::ensure!(!calls.is_empty() || original.run.outcome.is_some(), "recorded results unavailable");
        let store = Arc::new(ouro_durable::memory::memory());
        let started = self
            .records(id)
            .await?
            .values()
            .find_map(|r| if let Record::Execution { started_ms, .. } = r { Some(*started_ms) } else { None })
            .ok_or_else(|| anyhow::anyhow!("recorded execution unavailable (possibly retained out)"))?;
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(started)));
        // Preserve admitted identity and bindings. This isolated store never
        // writes to the original run, and no handler is invoked during replay.
        store.admit(&original.run.admission, started).await?;
        let mut runtime = Runtime::new(
            store,
            self.registry(),
            self.config.clone(),
            clock.clone(),
            opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new())),
        )?;
        runtime.capabilities = self.capabilities.clone();
        let used = Arc::new(Mutex::new(BTreeSet::new()));
        let count = calls.len();
        let status = run_isolated(
            &runtime,
            id,
            Mode::Replay {
                calls: Arc::new(calls),
                used: used.clone(),
                events: Arc::new(events),
                budgets: Arc::new(budgets),
                first,
            },
            &clock,
            stop,
        )
        .await?;
        let calls_replayed = used.lock().unwrap().len();
        if matches!(status, Status::Completed(_)) {
            anyhow::ensure!(calls_replayed == count, "replay left unused call results");
        }
        let calls = runtime.calls(id).await?;
        Ok(Replay { status, calls_replayed, calls })
    }
}
async fn run_isolated(
    runtime: &Runtime,
    id: &str,
    mode: Mode,
    clock: &TestClock,
    stop: &CancellationToken,
) -> anyhow::Result<Status> {
    for _ in 0..runtime.config.limits.max_tasks {
        if stop.is_cancelled() {
            return Ok(Status::Interrupted(WorkflowError::runtime("isolated run stopped")));
        }
        let root = runtime.get(id).await?.ok_or_else(|| anyhow::anyhow!("isolated run disappeared"))?;
        if let Some(status) = root.outcome {
            return Ok(status);
        }
        let due = runtime.store.due(clock.now_ms(), runtime.config.max_concurrency).await?;
        if due.is_empty() {
            let next = runtime
                .list(None, "")
                .await?
                .into_iter()
                .filter(|r| r.ended_ms.is_none())
                .map(|r| r.wake_at_ms)
                .min()
                .unwrap_or(i64::MAX);
            if next >= 253_402_300_799_000 {
                return Ok(Status::Waiting { wake_at_ms: next });
            }
            clock.advance(next);
        } else {
            for run in due {
                runtime.drive_mode(&run, stop, mode.clone()).await?;
            }
        }
    }
    anyhow::bail!("isolated execution exceeded recovery limit")
}
