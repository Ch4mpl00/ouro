//! Durable state: 1. public transactional contract; 2. lease journal;
//! 3. shared transition engine; 4. storage adapters and conformance.
//!
//! Operations commit before returning. An error may follow a successful
//! commit: callers must reload, never assume an empty journal. Store clocks
//! are explicit so expiry can be tested without changing persisted rows.
mod backend;
pub mod conformance;
pub mod control;
mod engine;
pub mod memory;
pub mod postgres;
pub mod sqlite;

pub use backend::{
    AtomicBackend, CommitFault, Conflict, FaultInjector, IoCounts, Key, Metrics, ReadOnly, Rows, SchemaError, Snapshot,
    Transaction, TransactionalStore,
};
pub use ows::model::{Breaker, Overlap};
use ows::{
    durable::{Journal, Record},
    run::{Events, Status, WorkflowError},
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
/// The admission's runtime binding (`true`) of a run the host started at a
/// named version instead of the active one (`Admission::pinned`).
pub const PINNED: &str = "pinned";

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
    /// Whether the host started this run at a version it named, rather than
    /// at the active one.
    pub fn pinned(&self) -> bool {
        self.runtime.get(PINNED).and_then(Value::as_bool).unwrap_or(false)
    }
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
    /// When a ready or waiting run is due. For a running run, when it
    /// became due for the claim that runs it (its previous lease's end, for
    /// a run taken over).
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
    /// Its calls' numeric annotations summed per name (its tokens, its
    /// cost), added in the commit that saves each answer, so a replayed call
    /// never counts twice; bounded like one call's annotations
    /// (`ows::durable::accumulate`). Kept after `sweep` removes the records.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: ows::durable::Totals,
    /// Frozen by an operator (`control::Command::Hold`): never returned by
    /// `due` nor claimed, whatever its wake-up, until released. The run and
    /// its live owned descendants are held in one commit; an ended run is
    /// never held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<control::Hold>,
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
    /// `digest_of(yaml)`, stored with the version when it is saved.
    pub digest: String,
    /// The breaker policy the definition's text declares
    /// (`document.metadata.ouro.breaker`), extracted when it is saved. Rows
    /// saved before the policy existed have none, which is `Inherit`.
    #[serde(default, skip_serializing_if = "is_inherit")]
    pub breaker: Breaker,
}
fn is_inherit(b: &Breaker) -> bool {
    *b == Breaker::Inherit
}

/// The content digest of a definition: the lowercase hex SHA-256 of its
/// canonical form. The canonical form is the parsed YAML document written as
/// compact JSON with every object's keys sorted (done here, so it does not
/// depend on `serde_json`'s `preserve_order`). Whitespace, comments, quoting
/// style and key order therefore leave the digest alone; any change of a
/// value, a key, a list's order or a scalar's type (`1` against `"1"`)
/// gives a new one. It identifies content, not a version number: two
/// versions with the same content share a digest. Text that is not YAML
/// has no document; its raw bytes are hashed under a distinct prefix, so
/// the digest still identifies exactly that text. A tripped breaker refuses
/// admissions by this digest.
///
/// It is computed once, when a version is saved, and stored with the version
/// (`Definition::digest`); the trip and the run row copy the stored value.
/// Nothing recomputes a digest from YAML later, so a future change to the
/// canonical form or the parser cannot alter the digest of an existing row.
pub fn digest_of(yaml: &str) -> String {
    use sha2::{Digest, Sha256};
    fn canonical(v: &Value, out: &mut String) {
        match v {
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    canonical(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort_unstable();
                out.push('{');
                for (i, key) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    canonical(&map[key], out);
                }
                out.push('}');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let text = match serde_yaml_ng::from_str::<Value>(yaml) {
        Ok(document) => {
            let mut out = String::from("yaml:");
            canonical(&document, &mut out);
            out
        }
        Err(_) => format!("raw:{yaml}"),
    };
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Definitions {
    pub versions: BTreeMap<u64, Definition>,
    pub active: Option<u64>,
    /// Consecutive faults of runs admitted in the current epoch. Frozen at
    /// the threshold once the breaker trips.
    pub consecutive_faults: u32,
    /// Only an activation starts an epoch, and only runs admitted in the
    /// current epoch count toward its breaker.
    pub epoch: u64,
    /// The breaker's report once it reached its threshold: no new runs (any
    /// path, `DefinitionTripped`) and no schedule until activation (or a manual pause is resumed). The
    /// engine never chooses another version; the host decides.
    pub tripped: Option<Trip>,
    /// Trigger keys the current activation armed. Activation and tripping
    /// replace them in their own commit.
    pub triggers: Vec<String>,
    /// The latest activation of each activated version. One entry per
    /// version, so the row stays bounded by its versions (it is read in
    /// every admission and terminal commit of the name): activating an older
    /// version again replaces its entry, and the epochs order them.
    #[serde(default)]
    pub activations: BTreeMap<u64, Activated>,
}

/// Who activates a version and why, beside the instant (the runtime's
/// clock): the host's attribution, when it gives one (`Runtime::activate_as`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
impl Activation {
    /// An unattributed activation at `at_ms`.
    pub fn at(at_ms: i64) -> Self {
        Self { at_ms, actor: None, note: None }
    }
}

/// A version's latest activation as the catalog keeps it: the epoch it
/// started and its `Activation`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activated {
    pub epoch: u64,
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The CloudEvent type of a breaker trip's notice (`Trip::event`).
pub const TRIPPED: &str = "ouro.definition.tripped";

/// What a tripped breaker reports, in `Definitions::tripped` and as a notice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trip {
    /// Present while an operator's pause stands. A pause alone has
    /// `breaker == false` and `reason.type_` `manual`; a pause of a
    /// definition the breaker tripped keeps the breaker's report beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<control::ManualTrip>,
    /// The breaker tripped (its threshold observation, reported once as a
    /// notice): only an activation clears it, and `resume` lifts a pause
    /// on top of it, never the trip itself.
    pub breaker: bool,
    /// The active version and the epoch the breaker tripped in.
    pub version: u64,
    /// The content digest of that version (`digest_of`): while an automatic
    /// trip stands, an admission whose definition has this digest is refused,
    /// of whatever version number, and so is one of a version the catalog
    /// does not hold; one with other content is admitted. A
    /// manual pause (`manual`) refuses every admission, whatever its content.
    pub digest: String,
    pub epoch: u64,
    /// The consecutive faults counted, which reached `threshold`.
    pub faults: u32,
    pub threshold: u32,
    /// The run whose terminal fault reached the threshold, and its error.
    pub run: String,
    pub reason: WorkflowError,
    pub at_ms: i64,
}
impl Trip {
    /// The refusal of a new run of `name` while this trip stands.
    pub fn refusal(&self, name: &str) -> DefinitionTripped {
        DefinitionTripped {
            name: name.into(),
            epoch: self.epoch,
            version: self.version,
            faults: self.faults,
            paused: self.manual.is_some(),
        }
    }
    /// The trip as an identified CloudEvent of type `TRIPPED`: its `source`
    /// names the definition and its `id` the epoch (one trip per epoch), so a
    /// repeated hand-off is recognisably the same event.
    pub fn event(&self, name: &str) -> Value {
        let time = chrono::DateTime::from_timestamp_millis(self.at_ms).map(|t| t.to_rfc3339()).unwrap_or_default();
        serde_json::json!({
            "specversion": "1.0",
            "type": TRIPPED,
            "source": format!("ouro/definitions/{name}"),
            "id": format!("tripped:{}", self.epoch),
            "subject": name,
            "time": time,
            "data": {
                "name": name,
                "version": self.version,
                "digest": self.digest,
                "epoch": self.epoch,
                "faults": self.faults,
                "threshold": self.threshold,
                "run": self.run,
                "reason": self.reason,
            },
        })
    }
}

/// What an activation arms: the triggers of the activated version, which
/// replace the definition's current ones in the activation's commit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Arming {
    pub triggers: BTreeMap<String, Trigger>,
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
    /// The slot's run the host could not admit (`Refused`), given instead of
    /// `admit`: stored in this commit as a run that ended faulted at its
    /// admission, which counts toward its definition's breaker as a terminal
    /// fault does (`finish`) and may trip it in this commit. The slot is
    /// consumed like an admitted one: overlap (`busy`), ingress and the next
    /// trigger apply unchanged.
    pub refused: Option<&'a Refused>,
    /// Policy and per-definition host ceiling. With `None`, the legacy
    /// exclusive `busy` guard applies. With `Some`, `busy` records an overlap
    /// skip and `ceiling` records a capacity skip. One admission per firing.
    pub overlap: Option<OverlapGuard<'a>>,
}

/// A trigger's run the host refused to admit: a function the definition
/// calls is no longer registered, or the definition no longer validates
/// against the host. Recorded with the slot (`Firing::refused`) so a broken
/// definition is a per-definition outcome, visible in the runs API and
/// counted by its breaker, never an error that stops the scheduler. Only a
/// deterministic refusal belongs here: an admission interrupted by the
/// host's infrastructure is no fault of the definition and consumes nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refused {
    /// The run as far as the host could build its admission: the slot's id,
    /// the definition version it would run, its input. It is never executed.
    pub admission: Admission,
    /// Why it was refused: the run's terminal fault.
    pub error: WorkflowError,
    /// The breaker threshold the fault counts against, as for `finish`.
    pub breaker_threshold: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct OverlapGuard<'a> {
    pub policy: Overlap,
    pub max_concurrent_runs: usize,
    pub ceiling: &'a Trigger,
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
    /// One admission was durably buffered; it starts after the live runs end.
    Queued,
    /// The host ceiling refused this slot; `ceiling` was stored instead.
    Ceiling,
}

/// The type of the workflow error a tripped definition answers a new run
/// with (`DefinitionTripped::error`).
pub const TRIPPED_ERROR: &str = "urn:ouro:errors:definition.tripped";

/// A new run of a tripped definition, refused in the admission's commit
/// whichever path admits it (a start, a child, a firing). Typed so a host can
/// tell it apart, and never retried: the trip stands until an activation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionTripped {
    pub name: String,
    pub epoch: u64,
    pub version: u64,
    pub faults: u32,
    /// An operator's pause, not a breaker trip: it refused this admission
    /// whatever version the run named, and `resume` lifts it.
    pub paused: bool,
}
impl DefinitionTripped {
    /// As a workflow error of type `TRIPPED_ERROR`: a `run: workflow` task
    /// answers with it, catchable by `try`/`catch` like any task error.
    pub fn error(&self) -> WorkflowError {
        WorkflowError {
            type_: TRIPPED_ERROR.into(),
            status: 409,
            title: Some("Definition Tripped".into()),
            detail: Some(self.to_string()),
            instance: None,
        }
    }
}
impl std::fmt::Display for DefinitionTripped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.paused {
            return write!(
                f,
                "{} is paused by an operator in epoch {}: no version is admitted until it is resumed or a version is activated",
                self.name, self.epoch
            );
        }
        write!(
            f,
            "{} is tripped in epoch {}: version {} faulted {} times in a row; activate a version to resume",
            self.name, self.epoch, self.version, self.faults
        )
    }
}
impl std::error::Error for DefinitionTripped {}

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
    /// Worker rows stale for longer than `control::WORKERS_KEPT_MS`.
    pub workers: usize,
}

/// Transactional operations, independent of SQL. `now_ms` is supplied by the
/// host clock and must use a common time basis across workers, within the
/// store's skew bound (`Policy::skew_ms`). Every fenced operation checks
/// ownership and expiry in the same commit as its changes.
///
/// Operations are asynchronous and need no particular Tokio flavour: a
/// `current_thread` runtime drives them as well as a multithreaded one. A
/// dropped operation leaves no partial transaction behind: either its commit
/// happened whole (the caller, which no longer knows, recovers from storage)
/// or nothing did. Nothing renews a lease by itself, so a dropped owner's
/// lease simply ends at its expiry.
#[async_trait::async_trait]
pub trait RunStore: Send + Sync {
    /// Executes an attributed operator command; its effect, its audit entry
    /// and its reply commit together. A request id already committed changes
    /// nothing and returns the stored reply, `Applied::repeated` saying so in
    /// that same transaction: of two concurrent submissions of one new
    /// request, exactly one is not a repeat.
    async fn apply_control(
        &self,
        request: &control::Request,
        command: &control::Command,
        now_ms: i64,
    ) -> anyhow::Result<control::Applied>;
    /// `apply_control`'s reply alone.
    async fn control(
        &self,
        request: &control::Request,
        command: &control::Command,
        now_ms: i64,
    ) -> anyhow::Result<control::Reply> {
        Ok(self.apply_control(request, command, now_ms).await?.reply)
    }
    async fn stopped(&self) -> anyhow::Result<bool>;
    async fn control_entry(&self, id: &str) -> anyhow::Result<Option<control::Entry>>;
    async fn control_history(
        &self,
        run: Option<&str>,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Entry>>;
    async fn query_runs(
        &self,
        filter: &control::Runs,
        history: bool,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<SavedRun>>;
    /// Catalog summaries in binary name order, in one consistent transaction.
    /// Limits and cursors follow the other control pages. No runs are scanned.
    async fn query_definitions(
        &self,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::DefinitionSummary>>;
    /// One consistent catalog/schedule observation; None for an unknown name.
    async fn definition_control(&self, name: &str) -> anyhow::Result<Option<control::DefinitionSummary>>;
    async fn pending_waits(
        &self,
        filter: &control::Waits,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Wait>>;
    async fn inspect_control(&self, id: &str) -> anyhow::Result<Option<control::Inspection>>;
    /// `name`'s annotation totals of the runs that ended in `[since_ms,
    /// before_ms)`, rounded out to whole buckets (`control::TOTALS_BUCKET_MS`)
    /// and covering at most `control::MAX_TOTALS_BUCKETS`: one range read of
    /// the definition's bucket rows, never of its runs.
    async fn totals(&self, _name: &str, _since_ms: i64, _before_ms: i64) -> anyhow::Result<control::DefinitionTotals> {
        anyhow::bail!("this store does not keep annotation totals")
    }
    /// Upserts a worker's liveness row in one commit: the row and its
    /// expiry index entry, nothing else. A beat older than the stored one
    /// changes nothing.
    async fn beat(&self, _worker: &control::Worker) -> anyhow::Result<()> {
        anyhow::bail!("this store does not support worker heartbeats")
    }
    /// A page of worker rows in id order, at most `limit` (1..=`MAX_PAGE`).
    async fn workers(
        &self,
        _cursor: Option<&control::Cursor>,
        _limit: usize,
    ) -> anyhow::Result<control::Page<control::Worker>> {
        anyhow::bail!("this store does not support worker heartbeats")
    }
    async fn describe_wait(
        &self,
        fence: &Fence,
        key: &str,
        wait: &ows::events::PendingWait,
        now_ms: i64,
    ) -> anyhow::Result<()>;

    /// The time policy this store applies.
    fn policy(&self) -> Policy {
        Policy::default()
    }
    /// Admits a run once (`false`: already admitted). A new run of a tripped
    /// definition is refused with `DefinitionTripped`; so are `start`,
    /// `start_child` and a firing's admissions.
    async fn admit(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<bool>;
    async fn start(&self, admission: &Admission, now_ms: i64) -> anyhow::Result<Option<Fence>>;
    /// A claim by an undeclared worker, which resumes every run.
    async fn claim(&self, id: &str, now_ms: i64) -> anyhow::Result<Option<Fence>> {
        self.claim_as(id, now_ms, &Compatibility::default()).await
    }
    /// Grants a due run only to a compatible worker. A run no worker is
    /// compatible with stays due and visible, never failed or lost. Every
    /// later owned mutation is fenced by the claim it granted.
    async fn claim_as(&self, id: &str, now_ms: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>>;
    /// A dispatch boundary: current ownership and the global stop gate.
    async fn check_lease(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    /// Extends the owner's lease. On a held run it extends it too and then
    /// fails with `control::Held`: the owner keeps its authority to record
    /// the answer of a call already dispatched, and learns to stop.
    async fn renew(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    async fn release(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<()>;
    async fn load_records(&self, fence: &Fence, now_ms: i64) -> anyhow::Result<BTreeMap<String, Record>>;
    async fn save_record(&self, fence: &Fence, key: &str, record: &Record, now_ms: i64) -> anyhow::Result<()>;
    async fn register_wait(&self, fence: &Fence, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    async fn unregister_wait(&self, fence: &Fence, key: &str, types: &[String], now_ms: i64) -> anyhow::Result<()>;
    /// Accepts an identified CloudEvent (non-empty `source`, `id`, `type`).
    /// With `target`, into that run's mailbox, whether or not it listens yet;
    /// without, for the oldest eligible registration of its type (one
    /// pending delivered copy per run and type). It waits until a listen
    /// takes it or `Policy::mailbox_ms` passes. Its identity is
    /// deduplicated per recipient run for the run's evidence lifetime:
    /// accepting it again returns the original recipient; the same identity
    /// with another payload (the event without `time`) is an error. Returns
    /// the recipient, `None` when none is eligible (a terminal target).
    async fn deliver(&self, event: &Value, target: Option<&str>, now_ms: i64) -> anyhow::Result<Option<String>>;
    /// `types` may hold `*`, which takes any published event (never a
    /// delivered one). The first matching pending copy is taken, in arrival
    /// order; an expired one is consumed by its expiry record instead.
    async fn take_event(
        &self,
        fence: &Fence,
        key: &str,
        types: &[String],
        now_ms: i64,
    ) -> anyhow::Result<Option<Value>>;
    /// Opens a subscription under `key` (idempotent) for published events of
    /// `types` (`*`: every published event). It stays open until
    /// `unsubscribe` or the run's end, across consumptions, so a listen that
    /// takes several events misses none published between two takes.
    async fn subscribe(&self, _fence: &Fence, _key: &str, _types: &[String], _now_ms: i64) -> anyhow::Result<()> {
        anyhow::bail!("this store does not support published events")
    }
    async fn unsubscribe(&self, _fence: &Fence, _key: &str, _now_ms: i64) -> anyhow::Result<()> {
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
    async fn publish(&self, _event: &Value, _target: Option<&str>, _now_ms: i64) -> anyhow::Result<Vec<String>> {
        anyhow::bail!("this store does not support published events")
    }
    /// Whether a trigger already took this identified event from ingress
    /// (an unexpired tombstone with its payload); another payload under the
    /// same identity is an error.
    async fn ingested(&self, _event: &Value, _now_ms: i64) -> anyhow::Result<bool> {
        anyhow::bail!("this store does not support triggers")
    }
    /// The trigger stored under `key`.
    async fn trigger(&self, _key: &str) -> anyhow::Result<Option<Trigger>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// Up to `limit` triggers due at `now_ms` or earlier, earliest first.
    async fn due_triggers(&self, _now_ms: i64, _limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// Up to `limit` triggers reacting to events of `type_` (or to `*`).
    async fn triggers_for(&self, _type_: &str, _limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        anyhow::bail!("this store does not support triggers")
    }
    /// The single buffered admission of a trigger, pinned when queued.
    async fn queued(&self, _key: &str) -> anyhow::Result<Option<Admission>> {
        Ok(None)
    }
    /// The claim-once primitive (`Firing`).
    async fn fire(&self, _firing: &Firing<'_>, _now_ms: i64) -> anyhow::Result<Fired> {
        anyhow::bail!("this store does not support triggers")
    }
    /// A plain firing: returns whether it committed.
    async fn swap_trigger(
        &self,
        key: &str,
        expected: Option<&Trigger>,
        next: Option<&Trigger>,
        admit: &[Admission],
        rearm: Option<&Rearm>,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let firing =
            Firing { key, expected, next, admit, rearm, ingress: None, busy: None, refused: None, overlap: None };
        Ok(self.fire(&firing, now_ms).await? == Fired::Committed)
    }
    /// Terminal outcomes of runs admitted in the active version's current
    /// epoch feed its breaker in the same commit: a fault (an unhandled
    /// error, a timeout, an unknown outcome) counts, a success resets the
    /// count; cancellation and running out of interruptions do not count.
    /// At `breaker_threshold` it trips: the definition records the `Trip`,
    /// its triggers are removed and a `TRIPPED` notice is stored, in that
    /// commit. A tripped breaker observes nothing more until an activation.
    /// It never changes the active version or the epoch.
    async fn finish(
        &self,
        fence: &Fence,
        outcome: &Status,
        now_ms: i64,
        breaker_threshold: u32,
    ) -> anyhow::Result<Status>;
    /// Parent ownership, child admission, and completion subscriptions commit
    /// together. Replaying a link joins the original immutable child.
    async fn start_child(
        &self,
        parent: &Fence,
        admission: &Admission,
        link: &ChildLink,
        now_ms: i64,
    ) -> anyhow::Result<Option<Fence>>;
    async fn child_outcome(&self, parent: &str, key: &str, child: &str) -> anyhow::Result<Option<Status>>;
    /// Spends one unit of the budget the run's tree shares (its root's
    /// counter, an owned child's included) under `key`, if fewer than
    /// `limit` are spent, and saves `Record::Budget` at `key` with the
    /// decision and the count after it, in one fenced commit; repeating
    /// returns the saved record. A refusal also saves the run's first
    /// refusal (`Record::Exceeded` at `ows::durable::EXCEEDED`) in that commit
    /// unless the run has one, and names it (`first`). The count survives
    /// restarts and ends with the root; the store never interprets what a
    /// unit stands for.
    async fn spend(&self, fence: &Fence, key: &str, limit: u64, now_ms: i64) -> anyhow::Result<Record>;
    /// Due runs for an undeclared worker: every application version.
    async fn due(&self, now_ms: i64, limit: usize) -> anyhow::Result<Vec<String>> {
        self.due_as(now_ms, limit, &Compatibility::default()).await
    }
    /// Up to `limit` due runs this worker is compatible with, earliest first.
    async fn due_as(&self, now_ms: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>>;
    async fn cancel(&self, id: &str, now_ms: i64) -> anyhow::Result<bool>;
    /// Removes finished runs' evidence older than `kept_ms`, expires due
    /// mailbox copies of unfinished runs (each into a `Record::Expired`),
    /// drops expired ingress tombstones and the rows of workers stale for
    /// longer than `control::WORKERS_KEPT_MS`.
    async fn sweep(&self, now_ms: i64, kept_ms: i64) -> anyhow::Result<Swept>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>>;
    async fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>>;
    async fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>>;
    /// Stores `yaml` as the next version of `name`, with the `breaker` policy
    /// its text declares (the host reads it from the parsed document; text
    /// the host did not parse has `Breaker::Inherit`).
    async fn save_definition(&self, name: &str, yaml: &str, breaker: Breaker) -> anyhow::Result<Definition>;
    /// Activates `version` in a new epoch with no triggers, unattributed,
    /// at `now_ms`.
    async fn activate(&self, name: &str, version: u64, now_ms: i64) -> anyhow::Result<()> {
        loop {
            let epoch = self.definitions(name).await?.epoch;
            let activation = Activation::at(now_ms);
            if self.activate_with(name, version, epoch, &Arming::default(), &activation).await?.is_some() {
                return Ok(());
            }
        }
    }
    /// Activates `version` in a new epoch, clears the breaker (a trip and
    /// its count), arms `arming` and records `activation` as the version's
    /// latest (`Definitions::activations`), in one commit, provided the
    /// definition is still in `epoch` (`None` otherwise: read it again).
    /// Reactivating the active version also starts a new epoch.
    async fn activate_with(
        &self,
        name: &str,
        version: u64,
        epoch: u64,
        arming: &Arming,
        activation: &Activation,
    ) -> anyhow::Result<Option<Definitions>>;
    async fn definitions(&self, name: &str) -> anyhow::Result<Definitions>;
    /// Up to `limit` notices, oldest first: CloudEvents the store committed
    /// for the host together with what they report (a breaker trip,
    /// `TRIPPED`), each kept under its key until acknowledged.
    async fn notices(&self, _limit: usize) -> anyhow::Result<Vec<(String, Value)>> {
        anyhow::bail!("this store does not support notices")
    }
    /// Removes a notice the host has handed on; `false`: already removed.
    async fn acknowledge(&self, _key: &str) -> anyhow::Result<bool> {
        anyhow::bail!("this store does not support notices")
    }
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

/// Binds an executor's fence to the interpreter's `Journal` and `Events`
/// seams. Dropping it does not mutate storage; the owner releases on
/// shutdown, and crashes recover after lease expiry.
pub struct Lease {
    pub store: Arc<dyn RunStore>,
    pub fence: Fence,
    pub clock: Arc<dyn Clock>,
}
#[async_trait::async_trait]
impl Journal for Lease {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.store.load_records(&self.fence, self.clock.now_ms()).await
    }
    async fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        self.store.save_record(&self.fence, key, record, self.clock.now_ms()).await
    }
    async fn spend(&self, key: &str, limit: u64) -> anyhow::Result<Option<Record>> {
        self.store.spend(&self.fence, key, limit, self.clock.now_ms()).await.map(Some)
    }
}
#[async_trait::async_trait]
impl Events for Lease {
    async fn waiting(&self, run_id: &str, key: &str, wait: &ows::events::PendingWait) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.describe_wait(&self.fence, key, wait, self.clock.now_ms()).await
    }

    async fn register(&self, run_id: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.register_wait(&self.fence, types, self.clock.now_ms()).await
    }
    async fn unregister(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.unregister_wait(&self.fence, key, types, self.clock.now_ms()).await
    }
    async fn subscribe(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.subscribe(&self.fence, key, types, self.clock.now_ms()).await
    }
    async fn unsubscribe(&self, run_id: &str, key: &str) -> anyhow::Result<()> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.unsubscribe(&self.fence, key, self.clock.now_ms()).await
    }
    async fn take(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        anyhow::ensure!(run_id == self.fence.run_id, "event run does not own this lease");
        self.store.take_event(&self.fence, key, types, self.clock.now_ms()).await
    }
    fn notification(&self, _: &str) -> futures::future::BoxFuture<'_, ()> {
        // Polling is also correct across independent connections and processes.
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::digest_of;

    const BASE: &str = "document: {dsl: '1.0.3', namespace: n, name: w, version: '1'}\nroot: 1\ndo:\n  - a:\n      set: {x: 1, y: [1, 2]}\n";

    #[test]
    fn the_digest_ignores_formatting_and_key_order_only() {
        let reformatted = "# a comment\ndo:\n- a:\n    set:\n      y:\n      - 1\n      - 2\n      x: 1\nroot: 1\ndocument:\n  name: \"w\"\n  namespace: 'n'\n  version: \"1\"\n  dsl: 1.0.3\n";
        assert_eq!(digest_of(BASE), digest_of(reformatted));
        assert_eq!(digest_of(BASE).len(), 64);
        for changed in [
            BASE.replace("x: 1", "x: 2"),
            BASE.replace("[1, 2]", "[2, 1]"),
            BASE.replace("root: 1", "root: '1'"),
            BASE.replace("name: w", "name: v"),
        ] {
            assert_ne!(digest_of(BASE), digest_of(&changed), "{changed}");
        }
        // Text without a document is hashed as it is.
        assert_ne!(digest_of("a: [unclosed"), digest_of("a: [unclosed "));
    }
}
