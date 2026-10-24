//! Durable state: 1. public transactional contract; 2. lease journal;
//! 3. shared transition engine; 4. storage adapters and conformance.
//!
//! Operations commit before returning. An error may follow a successful
//! commit: callers must reload, never assume an empty journal. Store clocks
//! are explicit so expiry can be tested without changing persisted rows.
mod backend;
pub mod conformance;
mod engine;
pub mod memory;
pub mod postgres;
pub mod sqlite;

pub use backend::{
    AtomicBackend, CommitFault, Conflict, FaultInjector, IoCounts, Key, Metrics, Rows, Snapshot, Transaction,
    TransactionalStore,
};
use owf::{
    durable::{Journal, Record},
    run::{Events, Status},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

pub const LEASE_MS: i64 = 30_000;
pub const HEARTBEAT_MS: u64 = 10_000;
pub const MAX_INTERRUPTIONS: u32 = 10;
pub const RECORDS_KEPT_MS: i64 = 7 * 24 * 3_600_000;
/// How long an addressed event waits in its run's mailbox for a listen.
pub const MAILBOX_KEPT_MS: i64 = 24 * 3_600_000;
/// How long an ingress identity stays used after a trigger took its event.
pub const INGRESS_KEPT_MS: i64 = 7 * 24 * 3_600_000;
/// The default bound on clock skew between workers (ε).
pub const CLOCK_SKEW_MS: i64 = 1_000;
/// The admission's runtime binding that names the application version a run
/// was admitted under (`Admission::application_version`).
pub const APPLICATION_VERSION: &str = "application_version";

/// A store's time policy. Every time-related guarantee holds under clock
/// skew between workers of at most `skew_ms`: an owner stops acting `skew_ms`
/// before its lease ends, and a lease is taken over only after it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Mailbox lifetime of an addressed event (default one day).
    pub mailbox_ms: i64,
    /// Lifetime of an ingress identity tombstone (default seven days).
    pub ingress_ms: i64,
    /// The configured skew bound ε (default one second).
    pub skew_ms: i64,
}
impl Default for Policy {
    fn default() -> Self {
        Self { mailbox_ms: MAILBOX_KEPT_MS, ingress_ms: INGRESS_KEPT_MS, skew_ms: CLOCK_SKEW_MS }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub definition: String,
    pub input: Value,
    /// Immutable host bindings exposed as `$runtime` during recovery. Its
    /// `application_version` (a string), when present, is the application
    /// version the run was admitted under; only compatible workers resume it.
    pub runtime: Value,
}
impl Admission {
    /// The declared application version; `None`: undeclared, so every worker
    /// is compatible (issue #3's default).
    pub fn application_version(&self) -> Option<&str> {
        self.runtime.get(APPLICATION_VERSION).and_then(Value::as_str)
    }
}

/// The application versions a worker resumes. A run admitted without a
/// declared version is compatible with every worker, and an undeclared worker
/// (the default) resumes every run: strict pinning is opt-in. Otherwise a
/// worker resumes runs of its own version and of the versions it accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compatibility {
    pub version: Option<String>,
    /// Older versions this worker's code still resumes, listed by the host.
    #[serde(default)]
    pub accepts: Vec<String>,
}
impl Compatibility {
    pub fn admits(&self, run: Option<&str>) -> bool {
        match (run, &self.version) {
            (None, _) | (_, None) => true,
            (Some(run), Some(own)) => run == own || self.accepts.iter().any(|a| a == run),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub run_id: String,
    pub owner: String,
    /// Monotonically increases for each execution attempt, including recovery.
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Ready,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildLink {
    pub parent: String,
    pub key: String,
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRun {
    pub admission: Admission,
    pub state: RunState,
    pub fence: Option<Fence>,
    pub generation: u64,
    pub lease_until_ms: Option<i64>,
    pub wake_at_ms: i64,
    pub interruptions: u32,
    pub outcome: Option<Status>,
    pub ended_ms: Option<i64>,
    pub child: Option<ChildLink>,
    /// When the run was admitted: absolute deadlines (the workflow's
    /// `timeout`) count from here, not from a later drive.
    #[serde(default)]
    pub admitted_ms: i64,
    /// The activation epoch of its definition the run was admitted in, when
    /// it ran the active version: only such runs feed that epoch's breaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    /// The root of an owned child's run tree, whose budget it spends
    /// (`RunStore::spend`); `None` for a root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub type_: String,
    pub data: Value,
    pub delivered_at_ms: i64,
    /// The `Record::Event` key that consumed it, or the `Record::Expired`
    /// key (`mailbox#expired[i]`) of its expiry.
    pub consumed_key: Option<String>,
    /// The CloudEvent as accepted (`publish`, `deliver`): its `type` and
    /// `data` equal the fields above. `None` for a child's lifecycle
    /// notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Value>,
    /// Accepted through `deliver`: only a listen for its type takes it,
    /// never a wildcard subscription (nor a lifecycle notification).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered: bool,
    /// A mailbox copy's end: from then on no listen takes it, and it is
    /// consumed by an observable `Record::Expired` (`take_event`, `sweep`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

/// A consistent persisted view, used by adapter conformance and diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run: SavedRun,
    pub records: BTreeMap<String, Record>,
    pub awaits: BTreeMap<String, i64>,
    pub listeners: BTreeMap<String, Vec<String>>,
    pub events: Vec<StoredEvent>,
    /// Open subscriptions to published events, by key: event types, `*` for any.
    #[serde(default)]
    pub subscriptions: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub version: u64,
    pub yaml: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Definitions {
    pub versions: BTreeMap<u64, Definition>,
    pub active: Option<u64>,
    pub previous: Option<u64>,
    pub consecutive_faults: u32,
    /// Every activation and every breaker rollback starts a new epoch. Only
    /// runs admitted in the current epoch count toward its breaker.
    #[serde(default)]
    pub epoch: u64,
    /// The breaker reached its threshold with no predecessor: no new runs
    /// and no schedule until the next activation.
    #[serde(default)]
    pub tripped: bool,
    /// Trigger keys the current activation armed. Activation, rollback and
    /// tripping replace them in their own commit.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// The triggers a breaker rollback arms for the predecessor.
    #[serde(default)]
    pub fallback: BTreeMap<String, Trigger>,
}

/// What an activation arms: the triggers of the activated version, which
/// replace the definition's current ones in the activation's commit, and
/// those a breaker rollback would arm for its predecessor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Arming {
    pub triggers: BTreeMap<String, Trigger>,
    pub fallback: BTreeMap<String, Trigger>,
}

/// One trigger firing: a compare-and-swap on the trigger `key` from
/// `expected` (`None`: absent) to `next` (`None` deletes it), admitting each
/// admission whose id is not stored yet, in one commit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Firing<'a> {
    pub key: &'a str,
    pub expected: Option<&'a Trigger>,
    pub next: Option<&'a Trigger>,
    pub admit: &'a [Admission],
    /// Attached to the admitted runs: at a run's terminal commit, if this
    /// trigger is still exactly the `next` this firing stored (not replaced
    /// by a later activation, so of the same epoch) and not due, it becomes
    /// due `delay_ms` after the end.
    pub rearm: Option<&'a Rearm>,
    /// An identified CloudEvent this firing consumes from ingress. Its
    /// `source` + `id` is used at most once within `Policy::ingress_ms`,
    /// whichever trigger takes it: one event starts at most one run.
    pub ingress: Option<&'a Value>,
    /// Exclusive firing: when the run this trigger key admitted last is
    /// unfinished (running or waiting), store `busy` instead of `next` and
    /// admit nothing.
    pub busy: Option<&'a Trigger>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fired {
    Committed,
    /// The stored trigger differs from `expected`; nothing was written.
    Changed,
    /// The previous run (its id) is unfinished: `busy` was stored instead.
    Busy(String),
    /// The ingress event was already used; nothing was written.
    Ingested,
}

/// The same event identity (`source` + `id`) accepted again with another
/// payload: returned to the caller, never a silent drop or overwrite.
#[derive(Debug)]
pub struct IdentityConflict(pub String);
impl std::fmt::Display for IdentityConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event {} was already accepted with another payload", self.0)
    }
}
impl std::error::Error for IdentityConflict {}

/// What `sweep` removed: evidence records of finished runs, expired mailbox
/// copies (each leaves a `Record::Expired`) and expired ingress tombstones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub records: usize,
    pub expired: usize,
    pub tombstones: usize,
}

/// Transactional operations, independent of SQL. `now_ms` is supplied by the
/// host clock and must use a common time basis across workers, within the
/// store's skew bound (`Policy::skew_ms`). Every fenced operation checks
/// ownership and expiry in the same commit as its changes. Synchronous
/// methods match the interpreter's Journal/Events contract.
pub trait RunStore: Send + Sync {
    /// The time policy this store applies.
    fn policy(&self) -> Policy {
        Policy::default()
    }
    fn admit(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<bool>;
    fn start(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<Option<Fence>>;
    /// A claim by an undeclared worker, which resumes every run.
    fn claim(&self, id: &str, now_ms: i64) -> anyhow::Result<Option<Fence>> {
        self.claim_as(id, now_ms, &Compatibility::default())
    }
    /// Grants a due run only to a compatible worker. A run no worker is
    /// compatible with stays due and visible, never failed or lost. Every
    /// later owned mutation is fenced by the claim it granted.
    fn claim_as(&self, id: &str, now_ms: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>>;
    fn renew(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    fn release(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    fn load_records(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<BTreeMap<String, Record>>;
    fn save_record(&self, fence: &Fence, key: &str, record: &Record, now_ms: i64) -> anyhow::Result<()>;
    fn register_wait(&self, fence: &Fence, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    fn unregister_wait(&self, fence: &Fence, key: &str, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    /// Accepts an identified CloudEvent (non-empty `source`, `id`, `type`).
    /// With `target`, into that run's mailbox, whether or not it listens yet;
    /// without, for the oldest eligible registration of its type (one
    /// pending delivered copy per run and type). It waits until a listen
    /// takes it or `Policy::mailbox_ms` passes. Its identity is
    /// deduplicated per recipient run for the run's evidence lifetime:
    /// accepting it again returns the original recipient; the same identity
    /// with another payload (the event without `time`) is an error. Returns
    /// the recipient, `None` when none is eligible (a terminal target).
    fn deliver(&self, event: &Value, target: Option<&str>, now_ms: i64) -> anyhow::Result<Option<String>>;
    /// `types` may hold `*`, which takes any published event (never a
    /// delivered one). The first matching pending copy is taken, in arrival
    /// order; an expired one is consumed by its expiry record instead.
    fn take_event(&self, fence: &Fence, key: &str, types: &[String], now_ms: i64) -> anyhow::Result<Option<Value>>;
    /// Opens a subscription under `key` (idempotent) for published events of
    /// `types` (`*`: every published event). It stays open until
    /// `unsubscribe` or the run's end, across consumptions, so a listen that
    /// takes several events misses none published between two takes.
    fn subscribe(&self, _fence: &Fence, _key: &str, _types: &[String], _now_ms: i64) -> anyhow::Result<()> {
        anyhow::bail!("this store does not support published events")
    }
    fn unsubscribe(&self, _fence: &Fence, _key: &str, _now_ms: i64) -> anyhow::Result<()> {
        anyhow::bail!("this store does not support published events")
    }
    /// Appends a CloudEvent (an object with at least `type`) to every
    /// unfinished run with a subscription to its type or to `*`, or only to
    /// `target`'s mailbox when given, subscribed or not (it then expires
    /// like a delivery). A run that already holds an event with the same
    /// `source` and `id` is skipped, so publishing again is idempotent; the
    /// same identity with another payload is an error. Several pending
    /// events of one type queue in order. Receiving runs wake; they are
    /// returned.
    fn publish(&self, _event: &Value, _target: Option<&str>, _now_ms: i64) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("this store does not support published events")
    }
    /// Whether a trigger already took this identified event from ingress
    /// (an unexpired tombstone with its payload); another payload under the
    /// same identity is an error.
    fn ingested(&self, _event: &Value, _now_ms: i64) -> anyhow::Result<bool> {
        anyhow::bail!("this store does not support triggers")
    }
    /// The trigger stored under `key`.
    fn trigger(&self, _key: &str) -> anyhow::Result<Option<Trigger>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// Up to `limit` triggers due at `now_ms` or earlier, earliest first.
    fn due_triggers(&self, _now_ms: i64, _limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// Up to `limit` triggers reacting to events of `type_` (or to `*`).
    fn triggers_for(&self, _type_: &str, _limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// The claim-once primitive (`Firing`).
    fn fire(&self, _firing: &Firing<'_>, _now_ms: i64) -> anyhow::Result<Fired> {
        anyhow::bail!("this store does not support triggers")
    }
    /// A plain firing: returns whether it committed.
    fn swap_trigger(
        &self,
        key: &str,
        expected: Option<&Trigger>,
        next: Option<&Trigger>,
        admit: &[Admission],
        rearm: Option<&Rearm>,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let firing = Firing { key, expected, next, admit, rearm, ingress: None, busy: None };
        Ok(self.fire(&firing, now_ms)? == Fired::Committed)
    }
    /// Terminal outcomes of runs admitted in the active version's current
    /// epoch feed its breaker in the same commit: a fault (an unhandled
    /// error, a timeout, an unknown outcome) counts, a success resets the
    /// count; cancellation and running out of interruptions do not count.
    /// At `breaker_threshold` it rolls back to the predecessor in a new
    /// epoch, arming the predecessor's triggers, or, with none, trips the
    /// definition and removes its triggers.
    fn finish(&self, fence: &Fence, outcome: &Status, now_ms: i64, breaker_threshold: u32) -> anyhow::Result<Status>;
    /// Parent ownership, child admission, and completion subscriptions commit
    /// together. Replaying a link joins the original immutable child.
    fn start_child(
        &self,
        parent: &Fence,
        admission: &Admission,
        link: &ChildLink,
        now_ms: i64,
    ) -> anyhow::Result<Option<Fence>>;
    fn child_outcome(&self, parent: &str, key: &str, child: &str) -> anyhow::Result<Option<Status>>;
    /// Spends one unit of the budget the run's tree shares (its root's
    /// counter, an owned child's included) under `key`, if fewer than
    /// `limit` are spent, and saves `Record::Budget` at `key` with the
    /// decision and the count after it, in one fenced commit; repeating
    /// returns the saved record. The count survives restarts and ends with
    /// the root; the store never interprets what a unit stands for.
    fn spend(&self, fence: &Fence, key: &str, limit: u64, now_ms: i64) -> anyhow::Result<Record>;
    /// Due runs for an undeclared worker: every application version.
    fn due(&self, now_ms: i64, limit: usize) -> anyhow::Result<Vec<String>> {
        self.due_as(now_ms, limit, &Compatibility::default())
    }
    /// Up to `limit` due runs this worker is compatible with, earliest first.
    fn due_as(&self, now_ms: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>>;
    fn cancel(&self, id: &str, now_ms: i64) -> anyhow::Result<bool>;
    /// Removes finished runs' evidence older than `kept_ms`, expires due
    /// mailbox copies of unfinished runs (each into a `Record::Expired`) and
    /// drops expired ingress tombstones.
    fn sweep(&self, now_ms: i64, kept_ms: i64) -> anyhow::Result<Swept>;
    fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
    fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>>;
    fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>>;
    fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition>;
    /// Activates `version` in a new epoch with no triggers.
    fn activate(&self, name: &str, version: u64) -> anyhow::Result<()> {
        loop {
            let epoch = self.definitions(name)?.epoch;
            if self.activate_with(name, version, epoch, &Arming::default())?.is_some() {
                return Ok(());
            }
        }
    }
    /// Activates `version` in a new epoch, clears the breaker and arms
    /// `arming`, in one commit, provided the definition is still in `epoch`
    /// (`None` otherwise: read it again). Reactivating the active version
    /// also starts a new epoch.
    fn activate_with(
        &self,
        name: &str,
        version: u64,
        epoch: u64,
        arming: &Arming,
    ) -> anyhow::Result<Option<Definitions>>;
    fn definitions(&self, name: &str) -> anyhow::Result<Definitions>;
}

/// A host-defined trigger: a keyed record the store indexes by the time it
/// is next due and by the event types it reacts to (`*`: any). The store
/// never interprets `state`; the host decides what firing means.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub at_ms: Option<i64>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub state: Value,
}

/// Makes a trigger due again `delay_ms` after a run ends (`Firing::rearm`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rearm {
    pub trigger: String,
    pub delay_ms: i64,
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

/// Binds an executor's fence to the interpreter's synchronous seams. Dropping
/// it does not mutate storage; the owner releases on shutdown, and crashes
/// recover after lease expiry. This works with asynchronous database clients.
pub struct Lease {
    pub store: Arc<dyn RunStore>,
    pub fence: Fence,
    pub clock: Arc<dyn Clock>,
}
impl Journal for Lease {
    fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.store.load_records(&self.fence, self.clock.now_ms())
    }
    fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        self.store.save_record(&self.fence, key, record, self.clock.now_ms())
    }
    fn spend(&self, key: &str, limit: u64) -> anyhow::Result<Option<Record>> {
        self.store.spend(&self.fence, key, limit, self.clock.now_ms()).map(Some)
    }
}
impl Events for Lease {
    fn register(&self, run_id: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.register_wait(&self.fence, types, self.clock.now_ms())
    }
    fn unregister(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.unregister_wait(&self.fence, key, types, self.clock.now_ms())
    }
    fn subscribe(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.subscribe(&self.fence, key, types, self.clock.now_ms())
    }
    fn unsubscribe(&self, run_id: &str, key: &str) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.unsubscribe(&self.fence, key, self.clock.now_ms())
    }
    fn take(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.take_event(&self.fence, key, types, self.clock.now_ms())
    }
    fn notification(&self, _: &str) -> futures::future::BoxFuture<'_, ()> {
        // Polling is also correct across independent connections and processes.
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1)))
    }
}
