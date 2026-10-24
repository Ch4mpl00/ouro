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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub version: u64,
    /// None when the catalog did not record an activation instant. Existing
    /// stores retain versions and epochs, but no activation timestamps.
    pub activated_ms: Option<i64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    Every,
    Cron,
    After,
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
    /// The call journal, each call with its annotations.
    pub calls: Vec<CallEntry>,
    /// The calls' numeric annotations summed per name
    /// (`ows::durable::annotation_totals`): a run's tokens, its cost.
    pub annotations: BTreeMap<String, serde_json::Number>,
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
