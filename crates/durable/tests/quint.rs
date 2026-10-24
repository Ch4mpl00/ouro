//! Model-based tests driven by the normative contract model (`spec/contract.qnt`):
//! Quint generates random and witness histories, this bridge executes each
//! command against a real store and compares a projection of the store with
//! the model's state after every step. The concurrent batches in
//! `quint/concurrent.rs` take their sequential specification from the same
//! model. The bridge reads the store only through `RunStore`, never backend
//! rows or engine helpers; fault injection and construction are confined to
//! the adapter factory.
//!
//! The bridge is the model's host: it keeps each worker's local state (fence,
//! phase, permit), decides from durable reads what the model's host decides
//! (dispatch, replay, which slot a firing takes) and dispatches to a ghost
//! counter, never a network. Store answers (authority, compare-and-swap,
//! deduplication, overlap, breaker, expiry) come from the store under test.
use anyhow::{Context, ensure};
use ouro_durable::{
    Admission, Arming, ChildLink, CommitFault, Compatibility, DefinitionTripped, FaultInjector, Fence, Fired, Firing,
    IdentityConflict, Overlap, OverlapGuard, Policy, Rearm, Refused, RunSnapshot, RunState, RunStore,
    TransactionalStore, Trigger, memory::MemoryBackend, sqlite::SqliteBackend,
};
use owf::{
    durable::{CallState, Delivery, Record, StepOutcome, TaskResult},
    model::Flow,
    run::{Status, WorkflowError},
};
use quint_connect::{Config, Driver, State, Step};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

// ── 1. the model's units and bindings ──────────────────────────────────────

/// One model tick. The store's lease is four ticks, the model's.
pub const TICK: i64 = 7_500;
/// The model's time policy: mailbox copies and ingress tombstones last five
/// ticks, the skew bound ε is one.
pub fn model_policy() -> Policy {
    Policy { mailbox_ms: 5 * TICK, ingress_ms: 5 * TICK, skew_ms: TICK }
}
/// Run `r`'s owned child is run `r + 1`, so run 2 can be a grandchild.
const RUNS: usize = 3;
const CALLS: usize = 3;
/// The model's jump budget per run tree (`ceiling`); spend keys are `jump<i>`.
const CEILING: u64 = 2;
const SWEEP_KEPT: i64 = 5 * TICK;
const DEADLINE: i64 = 8;
const NAME: &str = "model";
const SCHEDULE: &str = "schedule";
const INGRESS: &str = "ingress";
// Invocation 0 is at most once, 1 and 2 at least once: protection is the
// declared delivery alone. The host's dry-run mock rule marks 0 and 2.
fn protected(i: usize) -> bool {
    i == 0
}
fn mocked(i: usize) -> bool {
    i != 1
}
pub fn run_id(r: usize) -> String {
    format!("r{r}")
}
fn run_index(id: &str) -> Option<usize> {
    id.strip_prefix('r')?.parse().ok().filter(|r| *r < RUNS)
}
/// Worker `w` declares application version `w + 1`.
pub fn worker(w: usize) -> Compatibility {
    Compatibility { version: Some((w + 1).to_string()), accepts: vec![] }
}
fn error(kind: &str, detail: impl Into<String>) -> WorkflowError {
    WorkflowError {
        type_: format!("urn:model:{kind}"),
        status: 500,
        title: None,
        detail: Some(detail.into()),
        instance: None,
    }
}
/// Outcomes 1 success, 2 unhandled failure, 3 timeout, 4 unknown, 5 cancellation.
pub fn status(outcome: i64) -> Status {
    match outcome {
        1 => Status::Completed(json!(42)),
        2 => Status::Faulted(error("unhandled", "model failure")),
        3 => Status::Faulted(error("timeout", "deadline passed")),
        4 => Status::Faulted(error("unknown", "outcome unknown")),
        _ => Status::Cancelled,
    }
}
fn outcome(status: &Status) -> anyhow::Result<i64> {
    Ok(match status {
        Status::Completed(_) => 1,
        Status::Faulted(e) => match e.type_.as_str() {
            "urn:model:unhandled" => 2,
            "urn:model:timeout" => 3,
            "urn:model:unknown" => 4,
            other => anyhow::bail!("unexpected fault {other}"),
        },
        Status::Cancelled => 5,
        other => anyhow::bail!("nonterminal outcome persisted: {other:?}"),
    })
}
pub fn call_record(i: usize, state: CallState) -> Record {
    Record::Call {
        logical: format!("invocation{i}"),
        function: format!("function{i}"),
        arguments: json!(i),
        delivery: if protected(i) { Delivery::AtMostOnce } else { Delivery::AtLeastOnce },
        state,
    }
}
fn task_record(continuation: i64) -> Record {
    Record::Task {
        started_ms: 0,
        result: Some(TaskResult {
            context: json!({}),
            outcome: StepOutcome::Completed { output: json!(continuation), flow: Flow::Continue },
        }),
    }
}
/// The model's event `i` with a payload: its identity is `source` + `id`.
pub fn event(i: usize, payload: i64) -> Value {
    json!({"source": "model", "id": i.to_string(), "type": "event", "data": payload})
}
fn ingress_event(i: usize, payload: i64) -> Value {
    json!({"source": "ingress", "id": i.to_string(), "type": INGRESS, "data": payload})
}
/// The schedule's trigger: due at tick `at` (`None`: until a run ends).
pub fn schedule_trigger(at: Option<i64>, epoch: u64, slot: i64, catchup: i64, rearm: bool) -> Trigger {
    Trigger {
        at_ms: at.map(|t| t * TICK),
        types: vec![],
        state: json!({"epoch": epoch, "slot": slot, "catchup": catchup, "rearm": rearm, "overlap": 0, "maxRuns": 2}),
    }
}
fn overlap_policy(policy: i64) -> anyhow::Result<Overlap> {
    match policy {
        0 => Ok(Overlap::Skip),
        1 => Ok(Overlap::Allow),
        2 => Ok(Overlap::Queue),
        3 => Ok(Overlap::Replace),
        _ => anyhow::bail!("invalid model overlap policy"),
    }
}
fn ticks(ms: Option<i64>) -> i64 {
    ms.map_or(-1, |ms| ms / TICK)
}

// ── 2. the projection both sides are compared in ───────────────────────────

#[derive(Debug, Default, PartialEq, Eq)]
struct RunView {
    admitted: bool,
    input: i64,
    version: i64,
    epoch: i64,
    app: i64,
    mode: i64,
    outcome: i64,
    ended: i64,
    deadline: i64,
    owner: i64,
    generation: i64,
    lease: i64,
    records: Vec<i64>,
    values: Vec<i64>,
    frontier: BTreeSet<i64>,
    continuation: i64,
    subscription: bool,
    parent: i64,
    /// Budget decisions by key: 0 none, 1 granted, 2 refused; and the tree's
    /// count after each (`Record::Budget`).
    spends: Vec<i64>,
    spent: Vec<i64>,
}
impl RunView {
    fn absent() -> Self {
        Self {
            ended: -1,
            deadline: -1,
            owner: -1,
            lease: -1,
            records: vec![0; CALLS],
            values: vec![0; CALLS],
            parent: -1,
            spends: vec![0; CALLS],
            spent: vec![0; CALLS],
            ..Self::default()
        }
    }
}
/// A copy per recipient, in arrival order. Disposition 0 pending, 1 taken
/// (`key`), 2 expired: a pending copy past its time counts as expired, which
/// the store records lazily (at a take or a sweep) and the model at `expire`.
/// A broadcast copy never expires (`expires` -1).
#[derive(Debug, PartialEq, Eq)]
struct CopyView {
    recipient: i64,
    id: i64,
    payload: i64,
    expires: i64,
    disposition: i64,
    key: i64,
}
#[derive(Debug, PartialEq, Eq)]
struct TriggerView {
    overlap: i64,
    max_runs: i64,
    epoch: i64,
    due: i64,
    slot: i64,
    catchup: i64,
    rearm: bool,
}
#[derive(Debug, PartialEq, Eq)]
struct QueueView {
    run: i64,
    slot: i64,
    epoch: i64,
    input: i64,
    refused: bool,
}
#[derive(Debug, PartialEq, Eq)]
struct CatalogView {
    active: i64,
    epoch: i64,
    faults: i64,
    tripped: bool,
    trigger: Option<TriggerView>,
    /// The epochs whose trip has a notice, oldest first.
    notices: Vec<i64>,
}
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "Machine")]
pub struct View {
    now: i64,
    runs: Vec<RunView>,
    copies: Vec<CopyView>,
    catalog: CatalogView,
    queued: Option<QueueView>,
    admissions: Vec<i64>,
    fired: BTreeSet<(i64, i64)>,
    ingress: Vec<Option<i64>>,
}

// The model's state as Quint writes it (unused fields are ignored).
#[derive(Deserialize)]
struct Machine {
    db: Durable,
    now: i64,
}
#[derive(Deserialize)]
struct Durable {
    runs: Vec<ModelRun>,
    copies: Vec<ModelCopy>,
    catalog: ModelCatalog,
    admissions: Vec<i64>,
    fired: BTreeSet<(i64, i64)>,
    ingress: BTreeMap<i64, i64>,
    #[serde(rename = "ingressPayload")]
    ingress_payload: BTreeMap<i64, i64>,
}
#[derive(Deserialize)]
struct ModelRun {
    admitted: bool,
    input: i64,
    version: i64,
    epoch: i64,
    app: i64,
    mode: i64,
    outcome: i64,
    ended: i64,
    deadline: i64,
    owner: i64,
    generation: i64,
    lease: i64,
    records: Vec<i64>,
    values: Vec<i64>,
    frontier: BTreeSet<i64>,
    continuation: i64,
    subscription: bool,
    parent: i64,
    retained: bool,
    spends: Vec<i64>,
    spent: Vec<i64>,
}
#[derive(Deserialize)]
struct ModelCopy {
    id: i64,
    recipient: i64,
    payload: i64,
    expires: i64,
    disposition: i64,
    key: i64,
}
#[derive(Deserialize)]
struct ModelCatalog {
    overlap: i64,
    #[serde(rename = "maxRuns")]
    max_runs: i64,
    queued: i64,
    #[serde(rename = "queueSlot")]
    queue_slot: i64,
    #[serde(rename = "queueInput")]
    queue_input: i64,
    #[serde(rename = "queueRefused")]
    queue_refused: bool,
    active: i64,
    epoch: i64,
    faults: i64,
    tripped: bool,
    #[serde(rename = "triggerEpoch")]
    trigger_epoch: i64,
    due: i64,
    slot: i64,
    catchup: i64,
    rearm: bool,
    armed: bool,
    notices: Vec<i64>,
}
/// The model's `never`: a broadcast copy's expiry.
const NEVER: i64 = 1_000_000_000;
fn disposition(disposition: i64, expires: i64, now: i64) -> i64 {
    if disposition == 0 && expires >= 0 && expires <= now { 2 } else { disposition }
}
impl From<Machine> for View {
    fn from(m: Machine) -> Self {
        let now = m.now;
        let d = m.db;
        // Swept evidence is gone from the store; the model keeps it, marked.
        let retained = |r: i64| usize::try_from(r).ok().and_then(|r| d.runs.get(r)).is_none_or(|x| x.retained);
        let runs = d
            .runs
            .iter()
            .map(|x| {
                if !x.admitted {
                    return RunView::absent();
                }
                let kept = x.retained;
                RunView {
                    admitted: true,
                    input: x.input,
                    version: x.version,
                    epoch: x.epoch,
                    app: x.app,
                    mode: x.mode,
                    outcome: x.outcome,
                    ended: x.ended,
                    deadline: x.deadline,
                    owner: x.owner,
                    generation: x.generation,
                    lease: x.lease,
                    records: if kept { x.records.clone() } else { vec![0; CALLS] },
                    values: if kept { x.values.clone() } else { vec![0; CALLS] },
                    frontier: if kept { x.frontier.clone() } else { BTreeSet::new() },
                    continuation: if kept { x.continuation } else { 0 },
                    subscription: x.subscription,
                    parent: x.parent,
                    spends: if kept { x.spends.clone() } else { vec![0; CALLS] },
                    spent: if kept { x.spent.clone() } else { vec![0; CALLS] },
                }
            })
            .collect();
        let mut copies: Vec<CopyView> = d
            .copies
            .iter()
            .filter(|c| retained(c.recipient))
            .map(|c| {
                let expires = if c.expires >= NEVER { -1 } else { c.expires };
                CopyView {
                    recipient: c.recipient,
                    id: c.id,
                    payload: c.payload,
                    expires,
                    disposition: disposition(c.disposition, expires, now),
                    key: c.key,
                }
            })
            .collect();
        copies.sort_by_key(|c| c.recipient);
        let c = d.catalog;
        let ingress = (0..CALLS as i64)
            .map(|i| d.ingress.get(&i).filter(|t| **t > now).and_then(|_| d.ingress_payload.get(&i).copied()))
            .collect();
        View {
            now,
            runs,
            copies,
            catalog: CatalogView {
                active: c.active,
                epoch: c.epoch,
                faults: c.faults,
                tripped: c.tripped,
                trigger: c.armed.then_some(TriggerView {
                    overlap: c.overlap,
                    max_runs: c.max_runs,
                    epoch: c.trigger_epoch,
                    due: c.due,
                    slot: c.slot,
                    catchup: c.catchup,
                    rearm: c.rearm,
                }),
                notices: c.notices,
            },
            queued: (c.queued >= 0).then_some(QueueView {
                run: c.queued,
                slot: c.queue_slot,
                epoch: c.epoch,
                input: c.queue_input,
                refused: c.queue_refused,
            }),
            admissions: d.admissions,
            fired: d.fired,
            ingress,
        }
    }
}

impl State<StoreDriver> for View {
    fn from_driver(d: &StoreDriver) -> anyhow::Result<Self> {
        let store = d.store.as_ref();
        let mut runs = Vec::new();
        let mut copies = Vec::new();
        let mut admissions = Vec::new();
        let mut fired = BTreeSet::new();
        let mut snapshots = Vec::new();
        for r in 0..RUNS {
            let snapshot = store.inspect(&run_id(r))?;
            admissions.push(i64::from(snapshot.is_some()));
            let Some(snapshot) = snapshot else {
                runs.push(RunView::absent());
                snapshots.push(None);
                continue;
            };
            let (view, slot) = d.run_view(&snapshot)?;
            fired.extend(slot);
            runs.push(view);
            copies.extend(d.copies(r, &snapshot)?);
            snapshots.push(Some(snapshot));
        }
        d.audit(&snapshots)?;
        let defs = store.definitions(NAME)?;
        let trigger = store.trigger(SCHEDULE)?;
        ensure!(
            defs.triggers == trigger.iter().map(|_| SCHEDULE.to_string()).collect::<Vec<_>>(),
            "the definition's triggers and the stored triggers differ"
        );
        let trigger = trigger
            .map(|t| -> anyhow::Result<TriggerView> {
                let number = |k: &str| t.state[k].as_i64().with_context(|| format!("trigger {k}"));
                Ok(TriggerView {
                    overlap: number("overlap")?,
                    max_runs: number("maxRuns")?,
                    epoch: number("epoch")?,
                    due: ticks(t.at_ms),
                    slot: number("slot")?,
                    catchup: number("catchup")?,
                    rearm: t.state["rearm"].as_bool().context("trigger rearm")?,
                })
            })
            .transpose()?;
        // A trip reports its own epoch and count, and each has one notice.
        if let Some(t) = &defs.tripped {
            ensure!(
                (Some(t.version), t.epoch, t.faults) == (defs.active, defs.epoch, defs.consecutive_faults),
                "the trip disagrees with its definition: {t:?}"
            );
        }
        let notices = store
            .notices(64)?
            .into_iter()
            .map(|(_, e)| {
                ensure!(e["type"] == ouro_durable::TRIPPED && e["subject"] == NAME, "unexpected notice {e}");
                e["data"]["epoch"].as_i64().context("notice epoch")
            })
            .collect::<anyhow::Result<_>>()?;
        let mut ingress = Vec::new();
        for i in 0..CALLS {
            let mut live = None;
            for payload in 0..5 {
                match store.ingested(&ingress_event(i, payload), d.ms()) {
                    Ok(true) => live = Some(payload),
                    Ok(false) => break,
                    Err(e) if e.is::<IdentityConflict>() => {}
                    Err(e) => return Err(e),
                }
            }
            ingress.push(live);
        }
        Ok(View {
            now: d.now,
            runs,
            copies,
            catalog: CatalogView {
                active: defs.active.map_or(0, |v| v as i64),
                epoch: defs.epoch as i64,
                faults: i64::from(defs.consecutive_faults),
                tripped: defs.tripped.is_some(),
                trigger,
                notices,
            },
            queued: store
                .queued(SCHEDULE)?
                .map(|a| -> anyhow::Result<QueueView> {
                    Ok(QueueView {
                        run: i64::try_from(run_index(&a.id).context("queued run id")?)?,
                        slot: a.runtime["slot"][1].as_i64().context("queued slot")?,
                        epoch: a.runtime["slot"][0].as_i64().context("queued epoch")?,
                        input: a.input.as_i64().context("queued input")?,
                        refused: a.runtime["refused"].as_bool().unwrap_or(false),
                    })
                })
                .transpose()?,
            admissions,
            fired,
            ingress,
        })
    }
}

// ── 3. the driver: adapters, workers and the store ─────────────────────────

/// A worker's local state, the model's `Local`. Phase 0 idle, 1 authorized
/// to send (a fresh intent), 2 sent, 4 authorized to send again (an unknown
/// at-least-once call).
#[derive(Clone, Default)]
struct Worker {
    fence: Option<Fence>,
    execution: usize,
    phase: i64,
    invocation: usize,
    stopped: bool,
    permit: bool,
}
#[derive(Clone, Copy)]
pub enum Adapter {
    Memory,
    Sqlite,
    Postgres,
}
pub struct TestDatabase {
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
pub struct StoreDriver {
    coverage: Arc<Mutex<Coverage>>,
    pub adapter: Adapter,
    pub directory: tempfile::TempDir,
    pub store: Arc<dyn RunStore>,
    pub policy: Policy,
    overlap: i64,
    max_runs: usize,
    faults: Arc<FaultInjector>,
    _database: Option<TestDatabase>,
    workers: [Worker; 2],
    /// The model's clock, in ticks.
    now: i64,
    sends: [[usize; CALLS]; RUNS],
    /// Fence owners by worker, to name a run's current owner.
    owners: BTreeMap<String, i64>,
}
impl StoreDriver {
    /// A store with the default time policy.
    pub fn new(adapter: Adapter) -> anyhow::Result<Self> {
        Self::with_policy(adapter, Policy::default())
    }
    /// A store with the model's time policy.
    pub fn model(adapter: Adapter) -> anyhow::Result<Self> {
        Self::with_policy(adapter, model_policy())
    }
    fn with_policy(adapter: Adapter, policy: Policy) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let mut database = None;
        let (store, faults): (Arc<dyn RunStore>, _) = match adapter {
            Adapter::Memory => {
                let b = Arc::new(MemoryBackend::default());
                (Arc::new(TransactionalStore::new(b.clone()).with_policy(policy)), b.faults.clone())
            }
            Adapter::Sqlite => {
                let b = Arc::new(SqliteBackend::open(directory.path().join("model.sqlite"))?);
                (Arc::new(TransactionalStore::new(b.clone()).with_policy(policy)), b.faults.clone())
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
                (Arc::new(TransactionalStore::new(b.clone()).with_policy(policy)), b.faults.clone())
            }
        };
        Ok(Self {
            coverage: Arc::new(Mutex::new(Coverage::default())),
            adapter,
            directory,
            store,
            policy,
            overlap: 0,
            max_runs: 2,
            faults,
            _database: database,
            workers: Default::default(),
            now: 0,
            sends: [[0; CALLS]; RUNS],
            owners: BTreeMap::new(),
        })
    }
    /// Another handle to the same database (SQLite: an independent connection).
    pub fn handle(&self) -> anyhow::Result<Arc<dyn RunStore>> {
        Ok(match self.adapter {
            Adapter::Sqlite => {
                let mut backend = SqliteBackend::open(self.directory.path().join("model.sqlite"))?;
                backend.faults = self.faults.clone();
                Arc::new(TransactionalStore::new(Arc::new(backend)).with_policy(self.policy))
            }
            _ => self.store.clone(),
        })
    }
    /// The model's initial catalog: versions 1–3 saved, version 1 (the one
    /// with a schedule) active in epoch 1, its trigger due at tick 2, and the
    /// generic trigger that takes ingress events.
    pub fn initialize(&mut self) -> anyhow::Result<()> {
        for v in 1..=3 {
            self.store.save_definition(NAME, &format!("definition{v}"))?;
        }
        let arming =
            Arming { triggers: BTreeMap::from([(SCHEDULE.to_string(), schedule_trigger(Some(2), 1, 0, 0, false))]) };
        ensure!(self.store.activate_with(NAME, 1, 0, &arming)?.is_some(), "initial activation");
        let ingress = Trigger { at_ms: None, types: vec![INGRESS.into()], state: json!({}) };
        ensure!(self.store.swap_trigger(INGRESS, None, Some(&ingress), &[], None, 0)?, "ingress trigger");
        Ok(())
    }
    pub fn ms(&self) -> i64 {
        self.now * TICK
    }
    pub fn admission(
        &self,
        r: usize,
        input: i64,
        app: i64,
        mode: i64,
        slot: Option<(u64, i64)>,
    ) -> anyhow::Result<Admission> {
        let version = self.store.definitions(NAME)?.active.context("no active definition")?;
        let mut runtime = json!({"bindings": "fixed", "mode": mode});
        if app > 0 {
            runtime[ouro_durable::APPLICATION_VERSION] = json!(app.to_string());
        }
        if let Some((epoch, slot)) = slot {
            runtime["slot"] = json!([epoch, slot]);
        }
        Ok(Admission {
            id: run_id(r),
            name: NAME.into(),
            version,
            definition: format!("definition{version}"),
            input: json!(input),
            runtime,
        })
    }
    /// The fence worker `w` acts on run `r` with, as the model's host sees
    /// it: its own claim of that run, unless it stopped. Whether the lease
    /// still holds is the store's decision.
    fn fence(&self, w: usize, r: usize) -> Option<Fence> {
        let x = &self.workers[w];
        if x.stopped || x.execution != r { None } else { x.fence.clone() }
    }
    fn valid(&self, f: &Fence) -> anyhow::Result<bool> {
        let skew = self.policy.skew_ms;
        Ok(self.store.get(&f.run_id)?.is_some_and(|r| {
            r.fence.as_ref() == Some(f)
                && r.state == RunState::Running
                && r.lease_until_ms.is_some_and(|t| t > self.ms() + skew)
        }))
    }
    /// Runs one store mutation under the step's injected fault. Returns its
    /// answer and whether it committed: an injected failure after the
    /// commit is an error that committed nonetheless.
    fn write<T>(
        &self,
        fault: Option<CommitFault>,
        call: impl FnOnce(&dyn RunStore) -> anyhow::Result<T>,
    ) -> (anyhow::Result<T>, bool) {
        if let Some(fault) = fault {
            self.faults.arm(fault);
        }
        let result = call(self.store.as_ref());
        // Probing an armed fault disarms it; one still armed did not fire.
        let fired = fault.is_some_and(|f| self.faults.check(f).is_ok());
        let committed = result.is_ok() || (fault == Some(CommitFault::AfterCommit) && fired);
        (result, committed)
    }
    /// Stops every worker acting for a run that is now terminal.
    fn stop_terminal(&mut self) -> anyhow::Result<()> {
        for w in 0..2 {
            let r = self.workers[w].execution;
            if self.store.get(&run_id(r))?.is_some_and(|x| x.outcome.is_some()) {
                self.workers[w].stopped = true;
                self.workers[w].permit = false;
            }
        }
        Ok(())
    }
}

// ── 4. executing the model's commands ──────────────────────────────────────

impl Driver for StoreDriver {
    type State = View;
    fn config() -> Config {
        Config { state: &["m"], nondet: &["command"] }
    }
    fn step(&mut self, step: &Step) -> anyhow::Result<()> {
        *self.coverage.lock().unwrap().actions.entry(step.action_taken.clone()).or_default() += 1;
        if step.action_taken == "init" {
            let coverage = self.coverage.clone();
            *self = Self::model(self.adapter)?;
            self.coverage = coverage;
            return self.initialize();
        }
        macro_rules! pick {
            ($name:literal, $ty:ty) => {
                <$ty>::deserialize(step.nondet_picks.get($name).context(concat!("missing choice ", $name))?.clone())?
            };
        }
        let command = Command {
            op: step.action_taken.clone(),
            r: pick!("r", usize),
            w: pick!("w", usize),
            i: pick!("i", usize),
            value: pick!("value", i64),
            fault: pick!("fault", i64),
        };
        self.execute(&command)
    }
}

struct Command {
    op: String,
    r: usize,
    w: usize,
    i: usize,
    value: i64,
    fault: i64,
}

impl StoreDriver {
    /// The model's `execute`: the environment's part of a fault first, then
    /// the operation, then the fault's consequence for the acting worker.
    fn execute(&mut self, c: &Command) -> anyhow::Result<()> {
        match c.fault {
            6 => self.now += 4,
            7 => self.now += 1,
            10 => self.apply("activate", c.r, c.w, c.i, c.value, None)?,
            _ => {}
        }
        let actor = if matches!(c.fault, 5 | 11) { 1 - c.w } else { c.w };
        let op = match c.fault {
            4 if c.value % 2 == 0 => "take",
            4 => "address",
            _ => c.op.as_str(),
        };
        let value = if c.fault == 9 { -1 } else { c.value };
        let injected = match c.fault {
            1 | 8 => Some(CommitFault::BeforeCommit),
            2 => Some(CommitFault::AfterCommit),
            _ => None,
        };
        let before = (self.workers.clone(), self.now, self.sends);
        self.apply(op, c.r, actor, c.i, value, injected)?;
        if c.fault == 3 {
            self.apply(op, c.r, actor, c.i, c.value, None)?;
        }
        self.faults.clear();
        if matches!(c.fault, 1 | 8) {
            // Nothing committed: the step's local effects are lost too.
            (self.workers, self.now, self.sends) = before;
        }
        if matches!(c.fault, 1 | 2 | 8) {
            self.coverage.lock().unwrap().rejected += 1;
            let x = &mut self.workers[actor];
            x.stopped = true;
            x.phase = 0;
            x.permit = false;
        }
        Ok(())
    }

    fn apply(
        &mut self,
        op: &str,
        r: usize,
        w: usize,
        i: usize,
        value: i64,
        fault: Option<CommitFault>,
    ) -> anyhow::Result<()> {
        let ms = self.ms();
        let injected = fault.is_some();
        // An unfenced operation fails only by an injected fault or a
        // conflicting event identity; anything else is a finding.
        let expect = |result: anyhow::Result<()>| -> anyhow::Result<()> {
            match result {
                Err(e) if !injected && !e.is::<IdentityConflict>() => Err(e),
                _ => Ok(()),
            }
        };
        match op {
            "admit" | "dry" | "replay" => {
                let defs = self.store.definitions(NAME)?;
                if value < 0 || defs.tripped.is_some() || self.store.get(&run_id(r))?.is_some() {
                    return Ok(());
                }
                let mode = match op {
                    "dry" => 1,
                    "replay" => 2,
                    _ => 0,
                };
                let a = self.admission(r, value, i as i64, mode, None)?;
                expect(self.write(fault, |s| s.admit(&a, ms)).0.map(drop))?;
            }
            "claim" => {
                if self.workers.iter().any(|x| x.permit) {
                    return Ok(());
                }
                let before = self.store.get(&run_id(r))?.map(|x| x.generation);
                let (result, committed) = self.write(fault, |s| s.claim_as(&run_id(r), ms, &worker(w)));
                let fence = match result {
                    Ok(fence) => fence,
                    // A claim whose reply was lost: the new owner is read back
                    // only to name it; the worker stops anyway.
                    Err(_) if committed => {
                        self.store.get(&run_id(r))?.filter(|x| Some(x.generation) != before).and_then(|x| x.fence)
                    }
                    Err(e) if !injected => return Err(e),
                    Err(_) => None,
                };
                if let Some(f) = fence {
                    self.owners.insert(f.owner.clone(), w as i64);
                    self.workers[w] = Worker { fence: Some(f), execution: r, permit: true, ..Worker::default() };
                }
            }
            "renew" => {
                if let Some(f) = self.fence(w, r) {
                    let _ = self.write(fault, |s| s.renew(&f, ms));
                }
            }
            "release" => {
                if let Some(f) = self.fence(w, r)
                    && self.write(fault, |s| s.release(&f, ms)).1
                {
                    let x = &mut self.workers[w];
                    x.stopped = true;
                    x.phase = 0;
                    x.permit = false;
                }
            }
            "intent" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let Ok(records) = self.store.load_records(&f, ms) else { return Ok(()) };
                let saved = self.store.get(&run_id(r))?.context("leased run missing")?;
                let mode = saved.admission.runtime["mode"].as_i64().unwrap_or(0);
                let fresh = matches!(
                    records.get(&format!("call{i}")),
                    None | Some(Record::Call { state: CallState::NotExecuted(_), .. })
                );
                let deadline = saved.admitted_ms / TICK + DEADLINE;
                if fresh && mode != 2 && !(mode == 1 && mocked(i)) && self.now < deadline {
                    let record = call_record(i, CallState::Pending);
                    if self.write(fault, |s| s.save_record(&f, &format!("call{i}"), &record, ms)).1 {
                        self.workers[w].phase = 1;
                        self.workers[w].invocation = i;
                    }
                }
            }
            "dispatch" => {
                let x = &mut self.workers[w];
                if !x.stopped && matches!(x.phase, 1 | 4) && x.execution == r && x.invocation == i {
                    x.phase = 2;
                    self.sends[r][i] += 1;
                    ensure!(!protected(i) || self.sends[r][i] <= 1, "an at-most-once call was sent twice");
                }
            }
            "result" | "notExecuted" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let x = &self.workers[w];
                let state = match op {
                    "result" if x.phase == 2 && x.invocation == i && value >= 0 => {
                        let admission = self.store.get(&run_id(r))?.context("leased run missing")?.admission;
                        let v = admission.input.as_i64().context("input")? + i as i64;
                        // Value 1: the function answered with an error.
                        if value == 1 {
                            CallState::Done(Err(error("response", v.to_string())))
                        } else {
                            CallState::Done(Ok(json!(v)))
                        }
                    }
                    "notExecuted" if x.phase == 1 && x.invocation == i => {
                        CallState::NotExecuted(error("rejected", "not sent"))
                    }
                    _ => return Ok(()),
                };
                let record = call_record(i, state);
                if self.write(fault, |s| s.save_record(&f, &format!("call{i}"), &record, ms)).1 {
                    self.workers[w].phase = 0;
                }
            }
            "recover" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let Ok(records) = self.store.load_records(&f, ms) else { return Ok(()) };
                // An unknown at-least-once call may be sent again.
                if let Some(Record::Call { state: CallState::Pending, delivery: Delivery::AtLeastOnce, .. }) =
                    records.get(&format!("call{i}"))
                {
                    self.workers[w].phase = 4;
                    self.workers[w].invocation = i;
                }
            }
            "checkpoint" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let Ok(records) = self.store.load_records(&f, ms) else { return Ok(()) };
                let result = |k: usize| match records.get(&format!("call{k}")) {
                    Some(Record::Call { state: CallState::Done(Ok(v)), .. }) => v.as_i64(),
                    _ => None,
                };
                if result(i).is_none() {
                    return Ok(());
                }
                // The continuation is computed from durable records only.
                let continuation = (0..CALLS)
                    .filter(|k| *k == i || records.contains_key(&format!("task{k}")))
                    .map(|k| result(k).unwrap_or(0))
                    .sum();
                let record = task_record(continuation);
                let _ = self.write(fault, |s| s.save_record(&f, &format!("task{i}"), &record, ms));
            }
            "conflict" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let Ok(records) = self.store.load_records(&f, ms) else { return Ok(()) };
                if !records.contains_key(&format!("call{i}")) {
                    return Ok(());
                }
                // Every changed binding under the invocation's identity is refused.
                for change in 0..4 {
                    let mut record = call_record(i, CallState::Pending);
                    if let Record::Call { logical, function, arguments, delivery, .. } = &mut record {
                        match change {
                            0 => *logical = "different identity".into(),
                            1 => *function = "different function".into(),
                            2 => *arguments = json!("changed"),
                            _ => {
                                *delivery = match delivery {
                                    Delivery::AtMostOnce => Delivery::AtLeastOnce,
                                    Delivery::AtLeastOnce => Delivery::AtMostOnce,
                                }
                            }
                        }
                    }
                    ensure!(
                        self.store.save_record(&f, &format!("call{i}"), &record, ms).is_err(),
                        "binding conflict accepted"
                    );
                }
                if self.write(fault, |s| s.finish(&f, &status(2), ms, 2)).1 {
                    self.stop_terminal()?;
                }
            }
            "subscribe" => {
                if let Some(f) = self.fence(w, r) {
                    let _ = self.write(fault, |s| s.subscribe(&f, "listen", &["event".into()], ms));
                }
            }
            "spend" => {
                if let Some(f) = self.fence(w, r) {
                    let _ = self.write(fault, |s| s.spend(&f, &format!("jump{i}"), CEILING, ms));
                }
            }
            "address" | "publish" => {
                if value >= 0 {
                    let target = (op == "address").then(|| run_id(r));
                    let e = event(i, value);
                    expect(self.write(fault, |s| s.publish(&e, target.as_deref(), ms)).0.map(drop))?;
                }
            }
            "take" => {
                if let Some(f) = self.fence(w, r) {
                    let _ = self.write(fault, |s| s.take_event(&f, &format!("event{i}"), &["event".into()], ms));
                }
            }
            "expire" => {
                expect(self.write(fault, |s| s.sweep(ms, i64::MAX)).0.map(drop))?;
            }
            "sweep" => {
                expect(self.write(fault, |s| s.sweep(ms, SWEEP_KEPT)).0.map(drop))?;
            }
            "child" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                if r + 1 >= RUNS || self.store.get(&run_id(r + 1))?.is_some() {
                    return Ok(());
                }
                let parent = self.store.get(&run_id(r))?.context("leased parent missing")?.admission;
                let app = parent.application_version().map_or(Ok(0), str::parse)?;
                let mode = parent.runtime["mode"].as_i64().unwrap_or(0);
                let a = self.admission(r + 1, parent.input.as_i64().context("input")?, app, mode, None)?;
                let link = ChildLink { parent: run_id(r), key: "child".into(), reference: "child".into() };
                // The child's first lease stays with the parent's host. A
                // tripped definition refuses it, typed, in the same commit.
                let refusal = self.store.definitions(NAME)?.tripped.is_some() && self.valid(&f)?;
                let (result, _) = self.write(fault, |s| s.start_child(&f, &a, &link, ms));
                if refusal {
                    ensure!(
                        result.err().is_some_and(|e| e.is::<DefinitionTripped>()),
                        "a tripped definition admitted a child"
                    );
                }
            }
            "finish" | "timeout" => {
                let Some(f) = self.fence(w, r) else { return Ok(()) };
                let outcome = if op == "timeout" {
                    let saved = self.store.get(&run_id(r))?.context("leased run missing")?;
                    if self.now < saved.admitted_ms / TICK + DEADLINE {
                        return Ok(());
                    }
                    3
                } else if value >= 0 {
                    value + 1
                } else {
                    return Ok(());
                };
                if self.write(fault, |s| s.finish(&f, &status(outcome), ms, 2)).1 {
                    self.stop_terminal()?;
                }
            }
            "cancel" => {
                let (result, committed) = self.write(fault, |s| s.cancel(&run_id(r), ms));
                expect(result.map(drop))?;
                if committed {
                    self.stop_terminal()?;
                }
            }
            "policy" => {
                let Some(t) = self.store.trigger(SCHEDULE)? else { return Ok(()) };
                if value < 0 || t.state["slot"] != json!(0) {
                    return Ok(());
                }
                for r in 0..RUNS {
                    if self.store.get(&run_id(r))?.is_some() {
                        return Ok(());
                    }
                }
                let mut next = t.clone();
                next.state["overlap"] = json!(value % 4);
                next.state["maxRuns"] = json!(i + 1);
                let (result, committed) =
                    self.write(fault, |s| s.swap_trigger(SCHEDULE, Some(&t), Some(&next), &[], None, ms));
                expect(result.map(drop))?;
                if committed {
                    self.overlap = value % 4;
                    self.max_runs = i + 1;
                }
            }
            "activate" => {
                if value < 0 {
                    return Ok(());
                }
                let defs = self.store.definitions(NAME)?;
                let version = i as u64 + 1;
                let (catchup, rearm) = (value % 2, value >= 2);
                // Version 1 has the schedule.
                let mut arming = Arming::default();
                if version == 1 {
                    let mut t = schedule_trigger(Some(self.now + 2), defs.epoch + 1, 0, catchup, rearm);
                    t.state["overlap"] = json!(self.overlap);
                    t.state["maxRuns"] = json!(self.max_runs);
                    arming.triggers.insert(SCHEDULE.into(), t);
                }
                let (result, _) = self.write(fault, |s| s.activate_with(NAME, version, defs.epoch, &arming));
                if !injected {
                    ensure!(result?.is_some(), "the definition changed under a serial activation");
                }
            }
            // `refuse`: the host cannot admit the slot's run, so the firing
            // records it refused (`Firing::refused`) in place of admitting it.
            "fire" | "refuse" => {
                let defs = self.store.definitions(NAME)?;
                let Some(t) = self.store.trigger(SCHEDULE)? else { return Ok(()) };
                let Some(at) = t.at_ms else { return Ok(()) };
                if value < 0 || at > ms || defs.tripped.is_some() || t.state["epoch"] != json!(defs.epoch) {
                    return Ok(());
                }
                let slot = t.state["slot"].as_i64().context("slot")?;
                let catchup = t.state["catchup"].as_i64().context("catchup")?;
                let rearm = t.state["rearm"].as_bool().context("rearm")?;
                let moved = |at: Option<i64>| {
                    let mut next = t.clone();
                    next.at_ms = at.map(|at| at * TICK);
                    next.state["slot"] = json!(slot + 1);
                    next
                };
                let skip = moved(Some(self.now + 2));
                // A missed slot under `catchup: none`, or a slot whose run id
                // is taken, is skipped; the next slot is two ticks on.
                if (catchup == 1 && at < ms) || self.store.get(&run_id(r))?.is_some() {
                    let firing = Firing { key: SCHEDULE, expected: Some(&t), next: Some(&skip), ..Firing::default() };
                    let (result, _) = self.write(fault, |s| s.fire(&firing, ms));
                    if !injected {
                        ensure!(result? == Fired::Committed, "a serial skip did not commit");
                    }
                    return Ok(());
                }
                let mut admission = self.admission(r, value, 0, 0, Some((defs.epoch, slot)))?;
                if op == "refuse" {
                    admission.runtime["refused"] = json!(true);
                }
                let refused = (op == "refuse").then(|| Refused {
                    admission: admission.clone(),
                    // The model's unhandled failure (outcome 2), as `status(2)`.
                    error: error("unhandled", "model failure"),
                    breaker_threshold: 2,
                });
                let admit = if refused.is_some() { vec![] } else { vec![admission] };
                // A refused run never ends later to re-arm `after`.
                let parked = rearm && refused.is_none();
                let next = moved(if parked { None } else { Some(self.now + 2) });
                let again = Rearm { trigger: SCHEDULE.into(), delay_ms: 2 * TICK };
                let firing = Firing {
                    key: SCHEDULE,
                    expected: Some(&t),
                    next: Some(&next),
                    admit: &admit,
                    rearm: parked.then_some(&again),
                    ingress: None,
                    // The store skips the slot, in this commit, when the
                    // schedule's last run is unfinished.
                    busy: Some(&skip),
                    refused: refused.as_ref(),
                    overlap: Some(OverlapGuard {
                        policy: overlap_policy(self.overlap)?,
                        max_concurrent_runs: self.max_runs,
                        ceiling: &skip,
                    }),
                };
                let (result, _) = self.write(fault, |s| s.fire(&firing, ms));
                if !injected {
                    ensure!(
                        matches!(result?, Fired::Committed | Fired::Busy(_) | Fired::Queued | Fired::Ceiling),
                        "a serial firing did not commit"
                    );
                }
                self.stop_terminal()?;
            }
            "ingress" => {
                let e = ingress_event(i, value);
                match self.store.ingested(&e, ms) {
                    Err(e) if e.is::<IdentityConflict>() => return Ok(()),
                    Err(e) => return Err(e),
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                }
                let defs = self.store.definitions(NAME)?;
                if value < 0 || defs.tripped.is_some() || self.store.get(&run_id(r))?.is_some() {
                    return Ok(());
                }
                let t = self.store.trigger(INGRESS)?.context("ingress trigger")?;
                let admit = [self.admission(r, value, 0, 0, None)?];
                let firing = Firing {
                    key: INGRESS,
                    expected: Some(&t),
                    next: Some(&t),
                    admit: &admit,
                    ingress: Some(&e),
                    ..Firing::default()
                };
                let (result, _) = self.write(fault, |s| s.fire(&firing, ms));
                if !injected {
                    ensure!(result? == Fired::Committed, "a fresh ingress identity was refused");
                }
            }
            "crash" => {
                let x = &mut self.workers[w];
                x.stopped = true;
                x.phase = 0;
                x.permit = false;
                if matches!(self.adapter, Adapter::Sqlite) {
                    self.store = self.handle()?;
                }
            }
            "tick" => {
                self.now += 1;
                for w in 0..2 {
                    let lease = self.store.get(&run_id(self.workers[w].execution))?.and_then(|r| r.lease_until_ms);
                    if ticks(lease) <= self.now {
                        self.workers[w].permit = false;
                        self.workers[w].stopped = true;
                    }
                }
            }
            other => anyhow::bail!("unhandled model command {other}"),
        }
        Ok(())
    }
}

// ── 5. reading the store back ──────────────────────────────────────────────

impl StoreDriver {
    fn run_view(&self, snapshot: &RunSnapshot) -> anyhow::Result<(RunView, Option<(i64, i64)>)> {
        let r = &snapshot.run;
        ensure!(r.admission.name == NAME, "definition name changed");
        ensure!(r.admission.definition == format!("definition{}", r.admission.version), "definition changed");
        ensure!(r.admission.runtime["bindings"] == json!("fixed"), "bindings changed");
        ensure!(r.fence.is_some() == (r.state == RunState::Running), "fence/state mismatch");
        ensure!(r.outcome.is_some() == r.ended_ms.is_some(), "terminal outcome without its end");
        let mut view = RunView {
            admitted: true,
            input: r.admission.input.as_i64().context("admission input")?,
            version: r.admission.version as i64,
            epoch: r.epoch.map_or(0, |e| e as i64),
            app: r.admission.application_version().map_or(Ok(0), str::parse)?,
            mode: r.admission.runtime["mode"].as_i64().context("mode")?,
            outcome: r.outcome.as_ref().map(outcome).transpose()?.unwrap_or(0),
            ended: ticks(r.ended_ms),
            deadline: r.admitted_ms / TICK + DEADLINE,
            owner: r.fence.as_ref().and_then(|f| self.owners.get(&f.owner).copied()).unwrap_or(-1),
            generation: r.generation as i64,
            lease: ticks(r.lease_until_ms),
            subscription: !snapshot.subscriptions.is_empty(),
            parent: r.child.as_ref().and_then(|l| run_index(&l.parent)).map_or(-1, |p| p as i64),
            ..RunView::absent()
        };
        for (key, record) in &snapshot.records {
            if let Some(i) = key.strip_prefix("call") {
                let i: usize = i.parse()?;
                let Record::Call { state, .. } = record else { anyhow::bail!("not a call") };
                let (n, v) = match state {
                    CallState::Pending => (1, 0),
                    CallState::Done(Ok(v)) => (2, v.as_i64().context("call value")?),
                    CallState::Done(Err(e)) => (3, e.detail.as_deref().context("error value")?.parse()?),
                    CallState::NotExecuted(_) => (4, 0),
                };
                ensure!(*record == call_record(i, state.clone()), "call binding changed");
                view.records[i] = n;
                view.values[i] = v;
            } else if let Some(i) = key.strip_prefix("task") {
                let Record::Task {
                    result: Some(TaskResult { outcome: StepOutcome::Completed { output, .. }, .. }),
                    ..
                } = record
                else {
                    anyhow::bail!("not a completed task")
                };
                view.frontier.insert(i.parse()?);
                view.continuation = view.continuation.max(output.as_i64().context("continuation")?);
            } else if key == owf::durable::EXCEEDED {
                // The run's first refusal: one of its refused decisions, the
                // one every refusal names.
                let Record::Exceeded { by } = record else { anyhow::bail!("not a first refusal") };
                ensure!(
                    matches!(snapshot.records.get(by), Some(Record::Budget { granted: false, .. })),
                    "the first refusal names no refused decision"
                );
            } else if let Some(i) = key.strip_prefix("jump") {
                let i: usize = i.parse()?;
                let Record::Budget { granted, spent, first } = record else { anyhow::bail!("not a budget decision") };
                let named = snapshot.records.get(owf::durable::EXCEEDED);
                ensure!(
                    *granted == first.is_none()
                        && first.as_ref().is_none_or(|by| named == Some(&Record::Exceeded { by: by.clone() })),
                    "a refusal does not name the run's first"
                );
                view.spends[i] = if *granted { 1 } else { 2 };
                view.spent[i] = i64::try_from(*spent)?;
            } else {
                ensure!(key.starts_with("event") || key.starts_with("mailbox#expired["), "unexpected record {key}");
            }
        }
        let slot = r.admission.runtime.get("slot").map(|s| (s[0].as_i64().unwrap_or(-1), s[1].as_i64().unwrap_or(-1)));
        Ok((view, slot))
    }

    /// The run's copies of model events, with each consumption's evidence.
    fn copies(&self, r: usize, snapshot: &RunSnapshot) -> anyhow::Result<Vec<CopyView>> {
        let mut out = Vec::new();
        let mut taken = BTreeSet::new();
        for e in snapshot.events.iter().filter(|e| e.type_ == "event") {
            let envelope = e.envelope.as_ref().context("a model event without its envelope")?;
            let id: i64 = envelope["id"].as_str().context("event id")?.parse()?;
            let payload = e.data.as_i64().context("event payload")?;
            let expires = ticks(e.expires_at_ms);
            let (disposition, key) = match &e.consumed_key {
                None => (disposition(0, expires, self.now), -1),
                Some(k) if k.starts_with("mailbox#expired[") => {
                    let Some(Record::Expired { event }) = snapshot.records.get(k) else {
                        anyhow::bail!("an expiry without its record")
                    };
                    ensure!(event["id"] == envelope["id"] && event["data"] == e.data, "expiry evidence differs");
                    (2, -1)
                }
                Some(k) => {
                    let n: i64 = k.strip_prefix("event").context("consumed key")?.parse()?;
                    let Some(Record::Event { event }) = snapshot.records.get(k) else {
                        anyhow::bail!("a take without its record")
                    };
                    ensure!(event["id"] == envelope["id"] && event["data"] == e.data, "event evidence differs");
                    ensure!(taken.insert(n), "two copies taken under one key");
                    (1, n)
                }
            };
            out.push(CopyView { recipient: r as i64, id, payload, expires, disposition, key });
        }
        for key in snapshot.records.keys().filter(|k| k.starts_with("event")) {
            ensure!(taken.contains(&key[5..].parse::<i64>()?), "an event record without its taken copy");
        }
        Ok(out)
    }

    /// Checks outside the projection: fenced loads, the due queries per
    /// worker version, owned child outcomes and the parent's notification.
    fn audit(&self, snapshots: &[Option<RunSnapshot>]) -> anyhow::Result<()> {
        for x in &self.workers {
            if let Some(fence) = &x.fence {
                let loaded = self.store.load_records(fence, self.ms());
                if self.valid(fence)? {
                    let records = self.store.inspect(&fence.run_id)?.context("leased run missing")?.records;
                    ensure!(loaded? == records, "load differs from snapshot");
                } else {
                    ensure!(loaded.is_err(), "load outside the lease succeeded");
                }
            }
        }
        let mut due: Vec<_> = snapshots
            .iter()
            .flatten()
            .filter_map(|s| {
                let r = &s.run;
                let eligible = match r.state {
                    RunState::Ready | RunState::Waiting => r.wake_at_ms,
                    RunState::Running => r.lease_until_ms?,
                    _ => return None,
                };
                let app = r.admission.application_version().map(str::to_owned);
                (eligible <= self.ms()).then(|| (eligible, r.admission.id.clone(), app))
            })
            .collect();
        due.sort();
        let ids = |w: Option<usize>| -> Vec<String> {
            due.iter()
                .filter(|(_, _, app)| w.is_none_or(|w| worker(w).admits(app.as_deref())))
                .map(|(_, id, _)| id.clone())
                .collect()
        };
        ensure!(self.store.due(self.ms(), 3)? == ids(None), "due projection differs");
        for w in 0..2 {
            ensure!(self.store.due_as(self.ms(), 3, &worker(w))? == ids(Some(w)), "versioned due projection differs");
        }
        for child in snapshots.iter().flatten() {
            let Some(link) = &child.run.child else { continue };
            let parent = run_index(&link.parent)
                .and_then(|p| snapshots[p].as_ref())
                .context("an owned child without its parent")?;
            let id = &child.run.admission.id;
            ensure!(
                self.store.child_outcome(&link.parent, &link.key, id)? == child.run.outcome,
                "child outcome differs"
            );
            ensure!(self.store.child_outcome("unowned", &link.key, id).is_err(), "child ownership bypass");
            // A child that ended while its parent was live left the parent
            // its terminal notification, unless the parent's evidence is swept.
            let swept = parent.run.ended_ms.is_some() && parent.events.is_empty();
            if let Some(end) = child.run.ended_ms
                && parent.run.ended_ms.is_none_or(|p| p > end)
                && !swept
            {
                let notified = parent.events.iter().filter(|e| e.type_ == format!("done:{id}")).count();
                ensure!(notified == 1, "a finished child notified its live parent {notified} times");
            }
            // An owned child never outlives its parent (#14).
            if let Some(end) = parent.run.ended_ms {
                ensure!(
                    child.run.ended_ms.is_some_and(|c| c <= end),
                    "an owned child outlived its parent: {:?}",
                    child.run.outcome
                );
            }
        }
        Ok(())
    }
}

// ── 6. running the model's histories ───────────────────────────────────────

pub fn spec_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec").join(name)
}

// quint-connect finds `quint` through PATH. Set it only on a child test process,
// avoiding process-global environment mutation while Rust tests run in parallel.
pub fn with_quint(name: &str, run: impl FnOnce() -> anyhow::Result<()>) {
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
/// Every command of the contract model, and initialization.
const COMMANDS: usize = 31;
fn simulation(adapter: Adapter) -> anyhow::Result<()> {
    use quint_connect::runner::{self, RunConfig};
    let seed = std::env::var("QUINT_SEED").unwrap_or_else(|_| "20261023".into());
    let driver = StoreDriver::model(adapter)?;
    let coverage = driver.coverage.clone();
    runner::run_test(
        driver,
        runner::Config {
            test_name: "durable contract".into(),
            gen_config: RunConfig {
                spec: spec_path("contract.qnt").to_string_lossy().into_owned(),
                main: None,
                init: None,
                step: None,
                max_samples: Some(std::env::var("QUINT_SAMPLES").ok().map(|v| v.parse()).transpose()?.unwrap_or(100)),
                max_steps: Some(150),
                seed: seed.clone(),
            },
        },
    )?;
    eprintln!("Quint random coverage: {:?}", coverage.lock().unwrap());
    let mut witnesses = StoreDriver::model(adapter)?;
    witnesses.coverage = coverage.clone();
    runner::run_test(
        witnesses,
        runner::Config {
            test_name: "contract witnesses".into(),
            gen_config: runner::TestConfig {
                spec: spec_path("contract_witnesses.qnt").to_string_lossy().into_owned(),
                main: None,
                test: "bridgeSuite".into(),
                max_samples: Some(1),
                seed,
            },
        },
    )?;
    let coverage = coverage.lock().unwrap();
    eprintln!("Quint combined coverage: {coverage:?}");
    ensure!(coverage.actions.len() == COMMANDS, "the suite missed a contract command");
    Ok(())
}
#[test]
fn quint_memory() {
    with_quint("quint_memory", || simulation(Adapter::Memory));
}
#[test]
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
