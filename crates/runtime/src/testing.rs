//! Isolated dry runs and replay. Both use a private in-memory store; reads
//! execute only in dry mode. Replay takes results from durable call records.
use crate::{Effect, FunctionContext, FunctionError, Runtime};
use ouro_durable::{Clock, RunStore};
use owf::{
    durable::{CallState, Delivery, Record},
    run::{Status, WorkflowError},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub trait Mock: Send + Sync {
    fn response(&self, function: &str, arguments: &Value, context: &FunctionContext) -> Result<Value, FunctionError>;
}
impl<F> Mock for F
where
    F: Fn(&str, &Value, &FunctionContext) -> Result<Value, FunctionError> + Send + Sync,
{
    fn response(&self, f: &str, a: &Value, c: &FunctionContext) -> Result<Value, FunctionError> {
        self(f, a, c)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub task_path: String,
    pub logical: String,
    pub function: String,
    pub arguments: Value,
    pub effect: Effect,
    pub delivery: Delivery,
    pub result: Result<Value, WorkflowError>,
}
pub struct DryRun {
    pub status: Status,
    pub writes: Vec<RecordedCall>,
}
pub struct Replay {
    pub status: Status,
    pub calls_replayed: usize,
}
#[derive(Clone)]
pub(crate) enum Mode {
    Live,
    Dry {
        mock: Arc<dyn Mock>,
        writes: Arc<Mutex<Vec<RecordedCall>>>,
    },
    Replay {
        calls: Arc<BTreeMap<String, RecordedCall>>,
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
        self.validate(yaml)?;
        let store = Arc::new(ouro_durable::memory::memory());
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(self.clock.now_ms())));
        let mut config = self.config.clone();
        config.limits.max_sleep = Some(Duration::ZERO);
        config.limits.max_listen = Some(Duration::ZERO);
        let mut runtime = Runtime::new(
            store.clone(),
            self.registry.clone(),
            config,
            clock.clone(),
            opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new())),
        )?;
        runtime.capabilities = self.capabilities.clone();
        let d = runtime.save("dry-run", yaml)?;
        runtime.activate(&d.name, d.version)?;
        runtime.start("dry-run", "dry-run", input)?;
        runtime.definition_source = self.store.clone();
        runtime.tracer = self.tracer.clone();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let status =
            run_isolated(&runtime, "dry-run", Mode::Dry { mock, writes: writes.clone() }, &clock, stop).await?;
        let writes = writes.lock().unwrap().clone();
        Ok(DryRun { status, writes })
    }
    pub async fn replay(&self, id: &str, stop: &CancellationToken) -> anyhow::Result<Replay> {
        self.replay_inner(id, None, stop).await
    }
    /// Exercise a candidate definition using a recorded run's input and
    /// function results. Changed call names, arguments or effects fail replay.
    pub async fn replay_definition(&self, yaml: &str, id: &str, stop: &CancellationToken) -> anyhow::Result<Replay> {
        self.validate(yaml)?;
        self.replay_inner(id, Some(yaml), stop).await
    }
    async fn replay_inner(&self, id: &str, yaml: Option<&str>, stop: &CancellationToken) -> anyhow::Result<Replay> {
        let mut original = self.store.inspect(id)?.ok_or_else(|| anyhow::anyhow!("unknown recorded run"))?;
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
            original.run.admission.runtime["bindings"] = self.registry.bindings(&wf);
        }
        for (key, record) in original.records {
            if let Record::Call { logical, function, arguments, effect, delivery, state: CallState::Done(result) } =
                record
            {
                let reference = key.strip_suffix("#call").ok_or_else(|| anyhow::anyhow!("invalid call record key"))?;
                calls.insert(
                    reference.to_string(),
                    RecordedCall {
                        task_path: reference.into(),
                        logical,
                        function,
                        arguments,
                        effect,
                        delivery,
                        result,
                    },
                );
            }
        }
        anyhow::ensure!(!calls.is_empty() || original.run.outcome.is_some(), "recorded results unavailable");
        let store = Arc::new(ouro_durable::memory::memory());
        let started = self
            .records(id)?
            .values()
            .find_map(|r| if let Record::Execution { started_ms, .. } = r { Some(*started_ms) } else { None })
            .ok_or_else(|| anyhow::anyhow!("recorded execution unavailable (possibly retained out)"))?;
        let clock = Arc::new(TestClock(std::sync::atomic::AtomicI64::new(started)));
        // Preserve admitted identity and bindings. This isolated store never
        // writes to the original run, and no handler is invoked during replay.
        store.admit(&original.run.admission, started)?;
        let mut runtime = Runtime::new(
            store,
            self.registry.clone(),
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
        Ok(Replay { status, calls_replayed })
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
        let root = runtime.get(id)?.ok_or_else(|| anyhow::anyhow!("isolated run disappeared"))?;
        if let Some(status) = root.outcome {
            return Ok(status);
        }
        let due = runtime.store.due(clock.now_ms(), runtime.config.max_concurrency)?;
        if due.is_empty() {
            let next = runtime
                .list(None, "")?
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
