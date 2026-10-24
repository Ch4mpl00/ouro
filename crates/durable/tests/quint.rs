//! The bridge uses RunStore snapshots, never backend rows or engine helpers.
//! Fault injection and construction are confined to the adapter factory.
use anyhow::{Context, ensure};
use ouro_durable::{
    Admission, ChildLink, CommitFault, FaultInjector, Fence, RunSnapshot, RunState, RunStore, TransactionalStore,
    memory::MemoryBackend, sqlite::SqliteBackend,
};
use owf::{
    durable::{CallState, Delivery, Record, StepOutcome, TaskResult},
    model::Flow,
    run::{Effect, Status, WorkflowError},
};
use quint_connect::{Config, Driver, State, Step, switch};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct EventView {
    kind: usize,
    value: i64,
    time: i64,
    key: i64,
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct RunView {
    state: i64,
    version: u64,
    input: i64,
    generation: u64,
    lease: i64,
    wake: i64,
    interruptions: u32,
    ended: i64,
    parent: i64,
    calls: Vec<i64>,
    tasks: Vec<bool>,
    awaits: Vec<i64>,
    listeners: Vec<i64>,
    held: Vec<bool>,
    events: Vec<EventView>,
    taken: Vec<i64>,
}
impl Default for RunView {
    fn default() -> Self {
        Self {
            state: 0,
            version: 0,
            input: 0,
            generation: 0,
            lease: -1,
            wake: 0,
            interruptions: 0,
            ended: -1,
            parent: -1,
            calls: vec![0; 3],
            tasks: vec![false; 3],
            awaits: vec![-1; 5],
            listeners: vec![-1; 3],
            held: vec![false; 3],
            events: vec![],
            taken: vec![-1; 3],
        }
    }
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct CatalogView {
    count: usize,
    active: u64,
    previous: u64,
    faults: u32,
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct DatabaseView {
    runs: Vec<RunView>,
    catalog: CatalogView,
}
fn run_id(r: usize) -> String {
    format!("r{r}")
}
fn event_type(t: usize) -> String {
    match t {
        0 => "external".into(),
        1 | 2 => format!("done:r{t}"),
        3 | 4 => format!("waiting:r{}", t - 2),
        _ => panic!("invalid model event type"),
    }
}
fn event_kind(t: &str) -> anyhow::Result<usize> {
    (0..5).find(|i| event_type(*i) == t).context("unexpected event type")
}
fn error() -> WorkflowError {
    WorkflowError::runtime("model failure")
}
fn status(n: i64) -> Status {
    match n {
        3 => Status::Waiting { wake_at_ms: 1_000_000_000 },
        4 => Status::Completed(json!(42)),
        5 => Status::Faulted(error()),
        6 => Status::Cancelled,
        7 => Status::Interrupted(error()),
        _ => panic!("invalid model status"),
    }
}
fn state_number(s: &RunState) -> i64 {
    match s {
        RunState::Ready => 1,
        RunState::Running => 2,
        RunState::Waiting => 3,
        RunState::Completed => 4,
        RunState::Failed => 5,
        RunState::Cancelled => 6,
    }
}
fn call_record(i: usize, state: i64) -> Record {
    Record::Call {
        logical: format!("invocation{i}"),
        function: format!("function{i}"),
        arguments: json!(i),
        effect: if i == 0 { Effect::Write } else { Effect::Read },
        delivery: if i == 2 { Delivery::AtLeastOnce } else { Delivery::AtMostOnce },
        state: match state {
            1 => CallState::Pending,
            2 => CallState::Done(Ok(json!(i))),
            3 => CallState::Done(Err(error())),
            4 => CallState::NotExecuted(error()),
            _ => panic!("invalid call state"),
        },
    }
}
fn task_record(i: usize) -> Record {
    Record::Task {
        started_ms: 0,
        result: Some(TaskResult {
            context: json!({}),
            outcome: StepOutcome::Completed { output: json!(i), flow: Flow::Continue },
        }),
    }
}
impl RunView {
    fn project(snapshot: RunSnapshot) -> anyhow::Result<Self> {
        let r = &snapshot.run;
        let mut view = Self {
            state: state_number(&r.state),
            version: r.admission.version,
            input: r.admission.input.as_i64().context("admission input")?,
            generation: r.generation,
            lease: r.lease_until_ms.unwrap_or(-1),
            wake: r.wake_at_ms,
            interruptions: r.interruptions,
            ended: r.ended_ms.unwrap_or(-1),
            parent: r.child.as_ref().map(|l| l.parent[1..].parse()).transpose()?.unwrap_or(-1),
            ..Self::default()
        };
        ensure!(r.admission.name == "model", "definition name changed");
        ensure!(r.admission.definition == format!("definition{}", r.admission.version), "definition changed");
        ensure!(r.admission.runtime == json!({"bindings": "fixed", "format": 1}), "bindings changed");
        ensure!(r.fence.is_some() == (r.state == RunState::Running), "fence/state mismatch");
        if let Some(outcome) = &r.outcome {
            let expected = match outcome {
                Status::Completed(_) => 4,
                Status::Faulted(_) => 5,
                Status::Cancelled => 6,
                _ => anyhow::bail!("nonterminal outcome persisted"),
            };
            ensure!(expected == view.state, "outcome/state mismatch");
        } else {
            ensure!(view.state < 4, "terminal outcome missing");
        }
        for (t, at) in &snapshot.awaits {
            view.awaits[event_kind(t)?] = *at;
        }
        for (key, types) in &snapshot.listeners {
            if let Some(id) = key.strip_prefix("child:r") {
                view.held[id.parse::<usize>()?] = true;
            } else {
                ensure!(types.len() == 1, "unexpected listener type list");
                let k = key.strip_prefix("event").context("listener key")?.parse::<usize>()?;
                view.listeners[k] = i64::try_from(event_kind(&types[0])?)?;
            }
        }
        for (key, record) in &snapshot.records {
            if let Some(i) = key.strip_prefix("call") {
                let i: usize = i.parse()?;
                let Record::Call { state, .. } = record else { anyhow::bail!("not a call") };
                let n = match state {
                    CallState::Pending => 1,
                    CallState::Done(Ok(_)) => 2,
                    CallState::Done(Err(_)) => 3,
                    CallState::NotExecuted(_) => 4,
                };
                ensure!(*record == call_record(i, n), "call binding/result changed");
                view.calls[i] = n;
            } else if let Some(i) = key.strip_prefix("task") {
                let i: usize = i.parse()?;
                ensure!(*record == task_record(i), "task output/context/flow changed");
                view.tasks[i] = true;
            } else {
                ensure!(key.starts_with("event"), "unexpected record");
            }
        }
        for (index, e) in snapshot.events.iter().enumerate() {
            let kind = event_kind(&e.type_)?;
            let value = if kind == 0 {
                e.data.as_i64().context("external payload")?
            } else {
                match e.data["status"].as_str() {
                    Some("completed") => 4,
                    Some("faulted") => 5,
                    Some("cancelled") => 6,
                    Some("waiting") => 3,
                    _ => anyhow::bail!("child payload"),
                }
            };
            let key = if let Some(key) = &e.consumed_key {
                let k: usize = key.strip_prefix("event").context("consumed key")?.parse()?;
                let Record::Event { event } = snapshot.records.get(key).context("event record missing")? else {
                    anyhow::bail!("consumed event has wrong record")
                };
                ensure!(event["type"] == e.type_ && event["data"] == e.data, "event evidence differs");
                let time = chrono::DateTime::parse_from_rfc3339(event["time"].as_str().context("event time")?)?;
                ensure!(time.timestamp_millis() == e.delivered_at_ms, "event time differs");
                ensure!(view.taken[k] == -1, "two events for one invocation");
                view.taken[k] = i64::try_from(index)?;
                i64::try_from(k)?
            } else {
                -1
            };
            view.events.push(EventView { kind, value, time: e.delivered_at_ms, key });
        }
        for key in snapshot.records.keys().filter(|k| k.starts_with("event")) {
            let k: usize = key[5..].parse()?;
            ensure!(view.taken[k] >= 0, "event record without consumed event");
        }
        Ok(view)
    }
}
impl State<StoreDriver> for DatabaseView {
    fn from_driver(d: &StoreDriver) -> anyhow::Result<Self> {
        let runs = (0..3)
            .map(|r| d.store.inspect(&run_id(r))?.map(RunView::project).transpose().map(Option::unwrap_or_default))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let c = d.store.definitions("model")?;
        for (v, definition) in &c.versions {
            ensure!(definition.yaml == format!("definition{v}"), "version changed");
        }
        for worker in &d.workers {
            if let Some(fence) = &worker.fence {
                let loaded = d.store.load_records(fence, d.now);
                if d.valid(fence)? {
                    ensure!(
                        loaded? == d.store.inspect(&fence.run_id)?.context("leased run missing")?.records,
                        "load differs from snapshot"
                    );
                } else {
                    ensure!(loaded.is_err(), "stale load succeeded");
                }
            }
        }
        // Query APIs are checked as well as the persisted projection.
        let expected_due: Vec<_> = {
            let mut due = d
                .store
                .list(Some("model"), "r")?
                .into_iter()
                .filter(|r| match r.state {
                    RunState::Ready | RunState::Waiting => r.wake_at_ms <= d.now,
                    RunState::Running => r.lease_until_ms.is_some_and(|t| t <= d.now),
                    _ => false,
                })
                .collect::<Vec<_>>();
            // Ordered by when each run became eligible: a running run by its
            // lease expiry, not the wake time it was claimed at.
            due.sort_by_key(|r| {
                let eligible = match r.state {
                    RunState::Running => r.lease_until_ms.unwrap_or(r.wake_at_ms),
                    _ => r.wake_at_ms,
                };
                (eligible, r.admission.id.clone())
            });
            due.into_iter().map(|r| r.admission.id).collect()
        };
        ensure!(d.store.due(d.now, 3)? == expected_due, "due projection differs");
        for r in 0..3 {
            if let Some(saved) = d.store.get(&run_id(r))?
                && let Some(link) = &saved.child
            {
                ensure!(
                    d.store.child_outcome(&link.parent, &link.key, &run_id(r))? == saved.outcome,
                    "child outcome differs"
                );
                ensure!(d.store.child_outcome("unowned", &link.key, &run_id(r)).is_err(), "child ownership bypass");
            }
        }
        Ok(Self {
            runs,
            catalog: CatalogView {
                count: c.versions.len(),
                active: c.active.unwrap_or(0),
                previous: c.previous.unwrap_or(0),
                faults: c.consecutive_faults,
            },
        })
    }
}

#[derive(Clone, Default)]
struct Worker {
    fence: Option<Fence>,
    invocation: usize,
    phase: i64,
}
#[derive(Clone, Copy)]
enum Adapter {
    Memory,
    Sqlite,
    Postgres,
}
struct TestDatabase {
    config: tokio_postgres::Config,
    name: String,
}
impl TestDatabase {
    fn execute(&self, sql: &str) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(async {
            let (client, connection) = self.config.connect(tokio_postgres::NoTls).await?;
            let task = tokio::spawn(connection);
            let result = client.batch_execute(sql).await;
            drop(client);
            task.await??;
            Ok(result?)
        })
    }
}
impl Drop for TestDatabase {
    fn drop(&mut self) {
        self.execute(&format!("DROP DATABASE {} WITH (FORCE)", self.name)).expect("drop model test database");
    }
}
#[derive(Debug, Default)]
struct Coverage {
    actions: BTreeMap<String, usize>,
    rejected: usize,
}
struct StoreDriver {
    coverage: Arc<Mutex<Coverage>>,
    adapter: Adapter,
    directory: tempfile::TempDir,
    store: Arc<dyn RunStore>,
    faults: Arc<FaultInjector>,
    _database: Option<TestDatabase>,
    workers: [Worker; 2],
    now: i64,
    next_event: i64,
    sent: [[usize; 3]; 3],
}
impl StoreDriver {
    fn new(adapter: Adapter) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let mut database = None;
        let (store, faults): (Arc<dyn RunStore>, _) = match adapter {
            Adapter::Memory => {
                let b = Arc::new(MemoryBackend::default());
                (Arc::new(TransactionalStore::new(b.clone())), b.faults.clone())
            }
            Adapter::Sqlite => {
                let b = Arc::new(SqliteBackend::open(directory.path().join("model.sqlite"))?);
                (Arc::new(TransactionalStore::new(b.clone())), b.faults.clone())
            }
            Adapter::Postgres => {
                let config: tokio_postgres::Config = std::env::var("TEST_DATABASE_URL")?.parse()?;
                let db = TestDatabase {
                    config: config.clone(),
                    name: format!("ouro_quint_{}", uuid::Uuid::new_v4().simple()),
                };
                db.execute(&format!("CREATE DATABASE {}", db.name))?;
                let mut child = config;
                child.dbname(&db.name);
                let b = Arc::new(ouro_durable::postgres::PostgresBackend::connect_config(child)?);
                database = Some(db);
                (Arc::new(TransactionalStore::new(b.clone())), b.faults.clone())
            }
        };
        Ok(Self {
            coverage: Arc::new(Mutex::new(Coverage::default())),
            adapter,
            directory,
            store,
            faults,
            _database: database,
            workers: Default::default(),
            now: 0,
            next_event: 1,
            sent: [[0; 3]; 3],
        })
    }
    fn admission(&self, r: usize, input: i64) -> anyhow::Result<Admission> {
        let version = self.store.definitions("model")?.active.context("no active definition")?;
        Ok(Admission {
            id: run_id(r),
            name: "model".into(),
            version,
            definition: format!("definition{version}"),
            input: json!(input),
            runtime: json!({"bindings": "fixed", "format": 1}),
        })
    }
    fn fence(&self, w: usize) -> anyhow::Result<Fence> {
        self.workers[w].fence.clone().context("worker has no fence")
    }
    fn valid(&self, f: &Fence) -> anyhow::Result<bool> {
        Ok(self.store.get(&f.run_id)?.is_some_and(|r| {
            r.fence.as_ref() == Some(f)
                && r.state == RunState::Running
                && r.lease_until_ms.is_some_and(|t| t > self.now)
        }))
    }
    fn arm(&self, fault: i64) {
        match fault {
            1 => self.faults.arm(CommitFault::BeforeCommit),
            2 => self.faults.arm(CommitFault::AfterCommit),
            _ => {}
        }
    }
    fn outcome<T>(
        &mut self,
        result: anyhow::Result<T>,
        fault: i64,
        w: Option<usize>,
        valid: bool,
    ) -> anyhow::Result<Option<T>> {
        self.faults.clear();
        if fault != 0 || !valid {
            ensure!(result.is_err(), "operation unexpectedly succeeded");
            self.coverage.lock().unwrap().rejected += 1;
            if fault != 0
                && let Some(w) = w
            {
                self.workers[w].phase = 0;
            }
            Ok(None)
        } else {
            Ok(Some(result?))
        }
    }
    fn prepare(&mut self, w: usize, i: usize, fault: i64) -> anyhow::Result<()> {
        let f = self.fence(w)?;
        let records = self.store.load_records(&f, self.now)?;
        self.workers[w].invocation = i;
        match records.get(&format!("call{i}")) {
            Some(Record::Call { state: CallState::Done(_), .. }) => self.workers[w].phase = 0,
            Some(Record::Call { state: CallState::Pending, delivery: Delivery::AtMostOnce, .. }) => {
                self.workers[w].phase = 3
            }
            _ => {
                self.arm(fault);
                let result = self.store.save_record(&f, &format!("call{i}"), &call_record(i, 1), self.now);
                self.outcome(result, fault, Some(w), true)?;
                self.workers[w].phase = 1;
            }
        }
        if fault != 0 {
            self.workers[w].phase = 0;
        }
        Ok(())
    }
    fn send(&mut self, w: usize) -> anyhow::Result<()> {
        ensure!(self.workers[w].phase == 1, "dispatch without acknowledged intent");
        let f = self.fence(w)?;
        let r: usize = f.run_id[1..].parse()?;
        let i = self.workers[w].invocation;
        self.sent[r][i] += 1;
        ensure!(i == 2 || self.sent[r][i] <= 1, "protected dispatch repeated");
        self.workers[w].phase = 2;
        Ok(())
    }
    fn reopen(&mut self) -> anyhow::Result<()> {
        if matches!(self.adapter, Adapter::Sqlite) {
            let mut backend = SqliteBackend::open(self.directory.path().join("model.sqlite"))?;
            backend.faults = self.faults.clone();
            self.store = Arc::new(TransactionalStore::new(Arc::new(backend)));
        }
        Ok(())
    }
}
impl Driver for StoreDriver {
    type State = DatabaseView;
    fn config() -> Config {
        Config { state: &["s", "db"], nondet: &["command"] }
    }
    fn step(&mut self, original: &Step) -> anyhow::Result<()> {
        // Parameterized witness actions retain the same nondeterministic picks.
        let step = original;
        *self.coverage.lock().unwrap().actions.entry(step.action_taken.clone()).or_default() += 1;
        switch!(step {
            init => {
                let coverage = self.coverage.clone();
                *self = Self::new(self.adapter)?;
                self.coverage = coverage;
            },
            saveVersion => {
                let n = self.store.definitions("model")?.versions.len() + 1;
                self.store.save_definition("model", &format!("definition{n}"))?;
            },
            activate(version: u64) => { self.store.activate("model", version)?; },
            activateWith(version: u64) => { self.store.activate("model", version)?; },
            _ => { self.operation(original)?; },
        })
    }
}
impl StoreDriver {
    fn operation(&mut self, step: &Step) -> anyhow::Result<()> {
        // Both random and singleton-choice actions call these same handlers.
        let name = step.action_taken.strip_suffix("With").unwrap_or(&step.action_taken);
        macro_rules! pick {
            ($name:literal, $ty:ty) => {
                <$ty>::deserialize(step.nondet_picks.get($name).context(concat!("missing choice ", $name))?.clone())?
            };
        }
        match name {
            "admit" => {
                let r = pick!("r", usize);
                let input = pick!("input", i64);
                let fault = pick!("fault", i64);
                let a = self.admission(r, input)?;
                self.arm(fault);
                self.outcome(self.store.admit(&a, self.now), fault, None, true)?;
            }
            "start" | "claim" => {
                let r = pick!("r", usize);
                let w = pick!("w", usize);
                let expected = if name == "start" {
                    self.store.get(&run_id(r))?.is_none()
                } else {
                    self.store.due(self.now, 3)?.contains(&run_id(r))
                };
                let f = if name == "start" {
                    self.store.start(&self.admission(r, 1)?, self.now)?
                } else {
                    self.store.claim(&run_id(r), self.now)?
                };
                ensure!(f.is_some() == expected, "claim/start return differs");
                if let Some(f) = f {
                    self.workers[w] = Worker { fence: Some(f), ..Worker::default() };
                }
            }
            "renew" | "release" => {
                let w = pick!("w", usize);
                let f = self.fence(w)?;
                let valid = self.valid(&f)?;
                let result =
                    if name == "renew" { self.store.renew(&f, self.now) } else { self.store.release(&f, self.now) };
                self.outcome(result, 0, Some(w), valid)?;
                if name == "release" {
                    self.workers[w].phase = 0;
                }
            }
            "tick" => {
                self.now += pick!("delta", i64);
            }
            "crash" => {
                let w = pick!("w", usize);
                self.workers[w].phase = 0;
                self.reopen()?;
            }
            "restart" => {
                let w = pick!("w", usize);
                self.workers[w] = Worker::default();
            }
            "prepare" => {
                self.prepare(pick!("w", usize), pick!("i", usize), pick!("fault", i64))?;
            }
            "send" => {
                self.send(pick!("w", usize))?;
            }
            "response" | "notExecuted" => {
                let w = pick!("w", usize);
                let f = self.fence(w)?;
                let (result, fault) =
                    if name == "response" { (pick!("result", i64), pick!("fault", i64)) } else { (4, 0) };
                let valid = self.valid(&f)?;
                let i = self.workers[w].invocation;
                self.arm(fault);
                self.outcome(
                    self.store.save_record(&f, &format!("call{i}"), &call_record(i, result), self.now),
                    fault,
                    Some(w),
                    valid,
                )?;
                self.workers[w].phase = 0;
            }
            "checkpoint" => {
                let w = pick!("w", usize);
                let i = pick!("i", usize);
                let fault = pick!("fault", i64);
                let f = self.fence(w)?;
                let valid = self.valid(&f)?;
                self.arm(fault);
                self.outcome(
                    self.store.save_record(&f, &format!("task{i}"), &task_record(i), self.now),
                    fault,
                    Some(w),
                    valid,
                )?;
            }
            "conflict" => {
                let w = pick!("w", usize);
                let i = pick!("i", usize);
                let f = self.fence(w)?;
                for change in 0..4 {
                    let mut record = call_record(i, 1);
                    if let Record::Call { logical, function, arguments, delivery, .. } = &mut record {
                        match change {
                            0 => *logical = "different identity".into(),
                            1 => *function = "different function".into(),
                            2 => *arguments = json!("changed"),
                            _ => {
                                *delivery = if *delivery == Delivery::AtMostOnce {
                                    Delivery::AtLeastOnce
                                } else {
                                    Delivery::AtMostOnce
                                }
                            }
                        }
                    }
                    ensure!(
                        self.store.save_record(&f, &format!("call{i}"), &record, self.now).is_err(),
                        "binding conflict accepted"
                    );
                }
                self.store.finish(&f, &status(5), self.now, 2)?;
            }
            "registerWait" | "unregisterWait" | "takeEvent" => {
                let w = pick!("w", usize);
                let t = pick!("t", usize);
                let f = self.fence(w)?;
                let valid = self.valid(&f)?;
                let types = [event_type(t)];
                let (fault, result) = match name {
                    "registerWait" => (0, self.store.register_wait(&f, &types, self.now)),
                    "unregisterWait" => {
                        let k = pick!("k", usize);
                        (0, self.store.unregister_wait(&f, &format!("event{k}"), &types, self.now))
                    }
                    _ => {
                        let k = pick!("k", usize);
                        let fault = pick!("fault", i64);
                        self.arm(fault);
                        (fault, self.store.take_event(&f, &format!("event{k}"), &types, self.now).map(|_| ()))
                    }
                };
                self.outcome(result, fault, Some(w), valid)?;
            }
            "deliver" => {
                let t = pick!("t", usize);
                let fault = pick!("fault", i64);
                self.arm(fault);
                let event = json!({"source":"model","id":self.next_event.to_string(),"type":event_type(t),"data":self.next_event});
                self.outcome(self.store.deliver(&event, None, self.now), fault, None, true)?;
                self.next_event += 1;
            }
            "finish" => {
                let w = pick!("w", usize);
                let n = pick!("status", i64);
                let fault = pick!("fault", i64);
                let f = self.fence(w)?;
                let valid = self.valid(&f)?;
                self.arm(fault);
                self.outcome(self.store.finish(&f, &status(n), self.now, 2), fault, Some(w), valid)?;
                self.workers[w].phase = 0;
            }
            "startChild" => {
                let w = pick!("w", usize);
                let child = pick!("child", usize);
                let fault = pick!("fault", i64);
                let f = self.fence(w)?;
                let a = self.admission(child, 1)?;
                let link = ChildLink {
                    parent: f.run_id.clone(),
                    key: format!("child{child}"),
                    reference: format!("child{child}"),
                };
                self.arm(fault);
                self.outcome(self.store.start_child(&f, &a, &link, self.now), fault, Some(w), true)?;
            }
            "cancel" => {
                let r = pick!("r", usize);
                let fault = pick!("fault", i64);
                self.arm(fault);
                self.outcome(self.store.cancel(&run_id(r), self.now), fault, None, true)?;
            }
            "sweep" => {
                self.store.sweep(self.now, pick!("kept", i64))?;
            }
            _ => anyhow::bail!("unhandled model action {}", step.action_taken),
        }
        Ok(())
    }
}

fn spec_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec").join(name)
}

// quint-connect finds `quint` through PATH. Set it only on a child test process,
// avoiding process-global environment mutation while Rust tests run in parallel.
fn with_quint(name: &str, run: impl FnOnce() -> anyhow::Result<()>) {
    if std::env::var_os("OURO_QUINT_CHILD").is_some() {
        run().unwrap();
        return;
    }
    // Quint is a Node package; a checkout without `npm ci --prefix spec` skips
    // the bridge instead of failing the Rust suite. OURO_REQUIRE_QUINT=1 (CI)
    // turns the skip into a failure.
    let Ok(bin) = spec_path("node_modules/.bin").canonicalize() else {
        assert!(std::env::var_os("OURO_REQUIRE_QUINT").is_none(), "run npm ci --prefix spec first");
        eprintln!("skipping {name}: Quint is not installed (npm ci --prefix spec)");
        return;
    };
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("OURO_QUINT_CHILD", "1")
        .status()
        .unwrap();
    assert!(result.success(), "Quint child test failed");
}
fn simulation(adapter: Adapter) -> anyhow::Result<()> {
    use quint_connect::runner::{self, RunConfig};
    let driver = StoreDriver::new(adapter)?;
    let coverage = driver.coverage.clone();
    runner::run_test(
        driver,
        runner::Config {
            test_name: "durable protocol".into(),
            gen_config: RunConfig {
                spec: spec_path("protocol.qnt").to_string_lossy().into_owned(),
                main: None,
                init: None,
                step: None,
                max_samples: Some(std::env::var("QUINT_SAMPLES").ok().map(|v| v.parse()).transpose()?.unwrap_or(100)),
                max_steps: Some(150),
                seed: std::env::var("QUINT_SEED").unwrap_or_else(|_| "20261023".into()),
            },
        },
    )?;
    eprintln!("Quint random coverage: {:?}", coverage.lock().unwrap());
    for test in [
        "ambiguousTest",
        "sendCrashTest",
        "listenersTest",
        "childTest",
        "breakerTest",
        "replayTest",
        "beforeCommitTest",
        "staleFenceTest",
        "admissionTest",
        "readRetryTest",
        "cascadeTest",
        "retentionTest",
        "successResetsBreakerTest",
        "interruptionTest",
    ] {
        let mut witness_driver = StoreDriver::new(adapter)?;
        witness_driver.coverage = coverage.clone();
        runner::run_test(
            witness_driver,
            runner::Config {
                test_name: test.into(),
                gen_config: runner::TestConfig {
                    spec: spec_path("witnesses.qnt").to_string_lossy().into_owned(),
                    main: None,
                    test: test.into(),
                    max_samples: Some(1),
                    seed: std::env::var("QUINT_SEED").unwrap_or_else(|_| "20261023".into()),
                },
            },
        )?;
    }
    eprintln!("Quint combined coverage: {:?}", coverage.lock().unwrap());
    ensure!(coverage.lock().unwrap().actions.len() == 25, "suite missed a protocol action");
    Ok(())
}
#[test]
#[ignore = "protocol.qnt predates #4/#3; the contract-model bridge replaces it"]
fn quint_memory() {
    with_quint("quint_memory", || simulation(Adapter::Memory));
}
#[test]
#[ignore = "protocol.qnt predates #4/#3; the contract-model bridge replaces it"]
fn quint_sqlite() {
    with_quint("quint_sqlite", || simulation(Adapter::Sqlite));
}

#[test]
#[ignore = "requires TEST_DATABASE_URL with permission to create disposable databases"]
fn quint_postgres() {
    with_quint("quint_postgres", || simulation(Adapter::Postgres));
}

#[path = "quint/concurrent.rs"]
mod concurrent;

#[path = "quint/gaps.rs"]
mod gaps;
