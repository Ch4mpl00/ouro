//! Isolated dry runs and replay. Both use a private in-memory store. A dry
//! run dispatches every call except those its host's mock rule marks, which
//! the mock answers; replay dispatches nothing and takes results from
//! durable call records.
use crate::{FunctionContext, FunctionError, Output, Runtime};
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
pub struct DryRun {
    pub status: Status,
    /// The calls the mock answered and the events the run emitted (as
    /// `emit`; a dry run publishes none), in the order they happened.
    pub mocked: Vec<RecordedCall>,
    /// The dry run's call journal, mocked and dispatched calls alike, with
    /// their annotations (`annotation_totals` sums what it would cost).
    pub calls: Vec<CallEntry>,
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
    pub async fn dry_run(
        &self,
        yaml: &str,
        input: Value,
        mock: Arc<dyn Mock>,
        stop: &CancellationToken,
    ) -> anyhow::Result<DryRun> {
        self.dry_run_with(yaml, input, None, mock, stop).await
    }
    /// `dry_run` with host data for its root (`Runtime::start_with`).
    pub async fn dry_run_with(
        &self,
        yaml: &str,
        input: Value,
        host: Option<Value>,
        mock: Arc<dyn Mock>,
        stop: &CancellationToken,
    ) -> anyhow::Result<DryRun> {
        self.validate(yaml)?;
        let store = Arc::new(ouro_durable::memory::memory());
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(self.clock.now_ms())));
        let mut config = self.config.clone();
        config.limits.max_sleep = Some(Duration::ZERO);
        config.limits.max_listen = Some(Duration::ZERO);
        let mut runtime = Runtime::new(
            store.clone(),
            self.registry(),
            config,
            clock.clone(),
            opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new())),
        )?;
        runtime.capabilities = self.capabilities.clone();
        let d = runtime.save("dry-run", yaml).await?;
        runtime.activate(&d.name, d.version).await?;
        let start = crate::Start { version: None, host: host.unwrap_or(Value::Null) };
        runtime.start_with("dry-run", "dry-run", input, start).await?;
        runtime.definition_source = self.store.clone();
        // Traced with the host's tracer, unless the host removed its
        // OpenTelemetry observer; counted nowhere, and no host observer
        // hears it: a dry run is no run of the host.
        runtime.tracer = self.tracer.clone();
        runtime.observers = Arc::new(crate::observer::Observers {
            telemetry: self
                .observers
                .telemetry
                .as_ref()
                .map(|_| Arc::new(crate::telemetry::Telemetry::new(self.tracer.clone(), runtime.metrics.clone()))),
            host: Vec::new(),
            payload_bytes: 0,
        });
        let mocked = Arc::new(Mutex::new(Vec::new()));
        let status =
            run_isolated(&runtime, "dry-run", Mode::Dry { mock, mocked: mocked.clone() }, &clock, stop).await?;
        let mocked = mocked.lock().unwrap().clone();
        let calls = runtime.calls("dry-run").await?;
        Ok(DryRun { status, mocked, calls })
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
