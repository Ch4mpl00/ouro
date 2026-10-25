//! Store-only operator commands and bounded observations. Authorization belongs
//! to the embedding host; request identities make ambiguous commits retryable.
use crate::{RunSnapshot, RunState, SavedRun, Trigger, Trip};
use ows::{durable::CallEntry, events::PendingWait};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_PAGE: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub actor: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Cancels the run and its transitive owned descendants atomically.
    Cancel {
        run: String,
    },
    Pause {
        definition: String,
        note: String,
    },
    Resume {
        definition: String,
    },
    /// Freezes the run and its live owned descendants, atomically: none is
    /// claimed or returned as due until `Release`, and a worker driving one
    /// stops at its next fenced step or dispatch boundary and releases it.
    /// A call already dispatched is not retracted and its answer is still
    /// recorded: the owner keeps its lease until it has (`RunStore::renew`).
    /// Idempotent; an ended run is left alone.
    Hold {
        run: String,
        note: String,
    },
    /// Lets a held run and its held descendants continue from the same
    /// place; due work the hold kept is due again in the same commit.
    Release {
        run: String,
    },
    Stop {
        enabled: bool,
    },
    Respond {
        run: String,
        wait: String,
        event: Value,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Changed(bool),
    Responded(Option<String>),
}
/// A command's reply and whether its request was a repeat of one already
/// committed (`RunStore::apply_control`): a host that reports each request
/// once reports it when `repeated` is false.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub reply: Reply,
    pub repeated: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub request: Request,
    pub command: Command,
    pub at_ms: i64,
    pub reply: Reply,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualTrip {
    pub actor: String,
    pub reason: String,
    pub note: String,
}
/// Who froze a run and why, as `RunStore::control` recorded it (`SavedRun::held`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub actor: String,
    pub reason: String,
    pub note: String,
    pub at_ms: i64,
}

/// A fenced step the store refused because an operator holds the run. The
/// worker driving it stops and releases its lease; nothing is lost, the run
/// continues from its evidence when released.
#[derive(Debug)]
pub struct Held(pub String);
impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run {} is held by an operator", self.0)
    }
}
impl std::error::Error for Held {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Paused {
    pub epoch: u64,
    pub triggers: BTreeMap<String, Trigger>,
    pub queues: BTreeMap<String, Value>,
}

#[derive(Debug)]
pub struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("runtime emergency stop is set")
    }
}
impl std::error::Error for Stopped {}

/// What an operator command or query names and the store does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    Run,
    Definition,
}

/// Why a control command or query failed, typed so a console or CLI can
/// react without matching messages. A store raises the operator variants
/// inside its `anyhow` errors (`RunStore::control`); `ControlError::from`
/// recovers them, together with `Stopped` and `ReadOnly`, and keeps anything
/// else as `Storage` with its source chain.
#[derive(Debug)]
#[non_exhaustive]
pub enum ControlError {
    /// The command names a run or definition the store does not hold.
    NotFound { entity: Entity, id: String },
    /// The request id was already used with another command or attribution:
    /// a retry must repeat the original exactly.
    RequestConflict { id: String },
    /// The response's event identity (`source` + `id`) already answered with
    /// another payload or for another wait.
    ResponseConflict { identity: String },
    /// The command does not apply in the target's current state: resuming an
    /// automatic breaker trip (only an activation clears it), pausing a
    /// definition without an active version, resuming a pause whose
    /// activation changed.
    InvalidState(String),
    /// The emergency stop refuses this operation.
    Stopped,
    /// The wait is not open: never opened, already answered, closed by its
    /// run's end, or past its timeout.
    WaitClosed { run: String, wait: String },
    /// The response does not satisfy the wait's persisted matcher (its
    /// filters or correlation).
    WaitMismatch { run: String, wait: String },
    /// The store was opened read-only.
    ReadOnly,
    /// A malformed request: a page limit outside 1..=`MAX_PAGE`, a cursor of
    /// another query, an inverted or missing time window, missing request
    /// attribution or response identity.
    InvalidInput(String),
    /// Anything else, typically storage; displayed as its source.
    Storage(anyhow::Error),
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { entity: Entity::Run, id } => write!(f, "unknown run {id}"),
            Self::NotFound { entity: Entity::Definition, id } => write!(f, "unknown definition {id}"),
            Self::RequestConflict { id } => write!(f, "control request {id} was already used with other content"),
            Self::ResponseConflict { identity } => {
                write!(f, "response event {identity} was already used with other content")
            }
            Self::InvalidState(message) | Self::InvalidInput(message) => f.write_str(message),
            Self::Stopped => std::fmt::Display::fmt(&Stopped, f),
            Self::WaitClosed { run, wait } => write!(f, "wait {wait} of run {run} is not open"),
            Self::WaitMismatch { run, wait } => write!(f, "the event does not match wait {wait} of run {run}"),
            Self::ReadOnly => std::fmt::Display::fmt(&crate::ReadOnly, f),
            Self::Storage(e) => std::fmt::Display::fmt(e, f),
        }
    }
}
impl std::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Transparent: `Storage` displays its error, so the chain continues
        // with that error's own source.
        match self {
            Self::Storage(e) => e.source(),
            _ => None,
        }
    }
}
impl From<anyhow::Error> for ControlError {
    /// Recovers a typed refusal from a store's error, wherever it was
    /// raised; anything else stays `Storage`.
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<ControlError>() {
            Ok(typed) => return typed,
            Err(error) => error,
        };
        if error.is::<Stopped>() {
            Self::Stopped
        } else if error.is::<crate::ReadOnly>() {
            Self::ReadOnly
        } else {
            Self::Storage(error)
        }
    }
}

/// Cursors are opaque and bound to their query. Pages examine at most `limit`
/// index entries: combined residual filters may yield an empty page with a
/// continuation. Keep paging until `next` is absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub(crate) query: String,
    pub(crate) after: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}
/// Catalog metadata only; immutable YAML and run evidence are not returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefinitionSummary {
    pub name: String,
    /// Version numbers in ascending order.
    pub versions: Vec<VersionSummary>,
    pub active: Option<u64>,
    pub epoch: u64,
    pub consecutive_faults: u32,
    /// Includes the original report and attribution for a manual pause.
    pub tripped: Option<Trip>,
    /// One entry per kind configured by the active version (a schedule may
    /// combine kinds). Paused entries retain their suspended slot and queue;
    /// an automatic trip disarms them, leaving no next due time or queue.
    pub schedules: Vec<ScheduleSummary>,
}
/// A version and its latest activation, if it was ever activated: an older
/// version activated again shows that activation (the epochs order them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub version: u64,
    /// The version's content digest (`digest_of`).
    pub digest: String,
    /// When, by the runtime's clock.
    pub activated_ms: Option<i64>,
    /// The activation epoch it started.
    pub activated_epoch: Option<u64>,
    /// Who activated it and why, when the host said (`Runtime::activate_as`).
    pub activated_by: Option<String>,
    pub activation_note: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    Every,
    Cron,
    After,
    At,
    On,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleCatchup {
    Latest,
    None,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedSlot {
    /// The pinned run identity also identifies the buffered schedule slot.
    pub run: String,
    pub version: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSummary {
    pub kind: ScheduleKind,
    pub next_due_ms: Option<i64>,
    pub overlap: crate::Overlap,
    pub catchup: ScheduleCatchup,
    pub queued: Option<QueuedSlot>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runs {
    pub definition: Option<String>,
    pub state: Option<RunState>,
    pub parent: Option<String>,
    /// Inclusive admission instant (end instant for history).
    pub since_ms: Option<i64>,
    /// Exclusive admission instant (end instant for history).
    pub before_ms: Option<i64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waits {
    pub definition: Option<String>,
    pub run: Option<String>,
    pub event_type: Option<String>,
    pub since_ms: Option<i64>,
    pub before_ms: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub run: String,
    pub definition: String,
    pub key: String,
    pub waiting: PendingWait,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub snapshot: RunSnapshot,
    /// The run was started at a version the host named
    /// (`Runtime::start_at`); the version is `snapshot.run.admission.version`.
    #[serde(default)]
    pub pinned: bool,
    /// The content digest the admitted version had in the catalog when the
    /// run was admitted (stored with the run, not recomputed); empty when the
    /// catalog did not hold that version. What a trip compares admissions by.
    pub digest: String,
    /// The call journal, each call with its annotations.
    pub calls: Vec<CallEntry>,
    /// The calls' numeric annotations summed per name: a run's tokens, its
    /// cost. The run row's totals (`SavedRun::annotations`), so they remain
    /// once `sweep` has removed the call journal.
    pub annotations: ows::durable::Totals,
    pub waits: BTreeMap<String, PendingWait>,
    pub children: Vec<SavedRun>,
    pub budget_spent: u64,
    pub budget_limit: Option<u64>,
    /// The span context the run was admitted in (the host's request, a
    /// parent run's span), when there was one; every drive's root span
    /// links to it.
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
}
/// One worker's liveness row (`RunStore::beat`): who it is, how much it
/// holds and when it last beat. One row per worker id, replaced by each
/// beat, so a beat costs the same however many runs or workers there are. A
/// serverless host writes one per invocation, under the invocation's id.
/// Process memory and CPU are not here: an OpenTelemetry Collector or a
/// process exporter observes the process from outside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    /// The host name, as the embedding application supplied it.
    pub host: Option<String>,
    /// The application version the worker declares.
    pub application: Option<String>,
    pub started_ms: i64,
    pub beat_ms: i64,
    /// Stale from then on unless it beats again; the row is removed by
    /// `sweep` once it has been stale for `WORKERS_KEPT_MS`.
    pub expires_ms: i64,
    /// The runs it was executing at the beat.
    pub held: u64,
}
/// How long a stale worker's row stays listed before `sweep` removes it.
pub const WORKERS_KEPT_MS: i64 = 24 * 3_600_000;
/// A worker row as the control API lists it: stale when it did not beat
/// before its expiry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub worker: Worker,
    pub stale: bool,
}
/// Counts cover at most the supplied number of indexed entries. `next` means
/// these are lower bounds; pass it back to accumulate the next bounded page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    pub states: BTreeMap<String, usize>,
    pub next: Option<Cursor>,
}

/// The width of a totals bucket: one hour, aligned to the Unix epoch (UTC).
/// Part of the stored layout: a run's totals are added to its definition's
/// bucket of the hour it ended in, in its terminal commit.
pub const TOTALS_BUCKET_MS: i64 = 3_600_000;
/// The most buckets one `RunStore::totals` window may cover (41 days and a
/// bit): a query reads at most this many rows.
pub const MAX_TOTALS_BUCKETS: usize = MAX_PAGE;
/// One definition's annotation totals over a window (`RunStore::totals`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefinitionTotals {
    pub definition: String,
    /// The window rounded out to whole buckets, `[since_ms, before_ms)`.
    pub since_ms: i64,
    pub before_ms: i64,
    /// The runs that ended in the window with annotation totals.
    pub runs: u64,
    /// Their totals summed per name, bounded like a run's
    /// (`ows::durable::accumulate`).
    pub annotations: ows::durable::Totals,
    /// The buckets that hold any, oldest first.
    pub buckets: Vec<TotalsBucket>,
}
/// The runs of one definition that ended within one bucket with annotation
/// totals, and those totals summed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TotalsBucket {
    pub start_ms: i64,
    pub runs: u64,
    pub annotations: ows::durable::Totals,
}
