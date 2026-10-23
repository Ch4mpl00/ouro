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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub definition: String,
    pub input: Value,
    /// Immutable host bindings exposed as `$runtime` during recovery.
    pub runtime: Value,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub type_: String,
    pub data: Value,
    pub delivered_at_ms: i64,
    pub consumed_key: Option<String>,
    /// The CloudEvent as published (`publish`): its `type` and `data` equal
    /// the fields above. `None` for a targeted `deliver`, such as a child's
    /// lifecycle notification, which a wildcard subscription never takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Value>,
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
}

/// Transactional operations, independent of SQL. `now_ms` is supplied by the
/// host clock and must use a common time basis across workers. Every fenced
/// operation checks ownership and expiry in the same commit as its changes.
/// Synchronous methods match the interpreter's Journal/Events contract.
pub trait RunStore: Send + Sync {
    fn admit(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<bool>;
    fn start(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<Option<Fence>>;
    fn claim(&self, id: &str, now_ms: i64) -> anyhow::Result<Option<Fence>>;
    fn renew(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    fn release(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    fn load_records(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<BTreeMap<String, Record>>;
    fn save_record(&self, fence: &Fence, key: &str, record: &Record, now_ms: i64) -> anyhow::Result<()>;
    fn register_wait(&self, fence: &Fence, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    fn unregister_wait(&self, fence: &Fence, key: &str, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    /// A delivery targets the oldest eligible registration. One pending copy
    /// per run/type; consumed invocation keys remain durable for replay.
    fn deliver(&self, type_: &str, data: &Value, now_ms: i64) -> anyhow::Result<Option<String>>;
    /// `types` may hold `*`, which takes any published event (never a
    /// targeted delivery). A published event is taken as its envelope.
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
    /// `target` when given, subscribed or not. A run that already holds an
    /// event with the same `source` and `id` is skipped, so publishing again
    /// is idempotent. Several pending events of one type queue in order.
    /// Receiving runs wake; they are returned.
    fn publish(&self, _event: &Value, _target: Option<&str>, _now_ms: i64) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("this store does not support published events")
    }
    /// Every definition name with at least one saved version.
    fn definition_names(&self) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("this store does not list definitions")
    }
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
    fn due(&self, now_ms: i64, limit: usize) -> anyhow::Result<Vec<String>>;
    fn cancel(&self, id: &str, now_ms: i64) -> anyhow::Result<bool>;
    fn sweep(&self, now_ms: i64, kept_ms: i64) -> anyhow::Result<usize>;
    fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
    fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>>;
    fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>>;
    fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition>;
    fn activate(&self, name: &str, version: u64) -> anyhow::Result<()>;
    fn definitions(&self, name: &str) -> anyhow::Result<Definitions>;
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
