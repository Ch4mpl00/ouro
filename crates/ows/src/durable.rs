//! Storage contract for resumable execution. The host owns transactions,
//! leases and fencing; the interpreter owns the records' meaning.
//!
//! # Invariants
//!
//! The interpreter and Journal host jointly enforce the run/call contract.
//! Event waits use the same invocation identities and checkpoints. Retry
//! safety is a call's effective delivery, bound into its record. The engine
//! does not classify calls as reads or writes: what a dry run mocks and
//! whether a call changed the world are the host's to judge, from the call
//! journal (`call_journal`) and its own knowledge of its functions.
//!
//! ## Run identity and replay
//!
//! 1. A run has a stable ID and immutable definition, original input and
//!    runtime bindings. Persist them before executing its first step. State
//!    format and interpreter semantics are versioned; incompatible recovery
//!    fails explicitly instead of running the saved state under new semantics.
//! 2. Recovery replays the DSL from the beginning. A completed step restores
//!    its saved output, context and flow/error outcome without invoking its
//!    body again. No serialized Rust stack or arbitrary-code continuation is
//!    required. Unfinished parents replay their completed children.
//! 3. Every logical invocation has a stable identity: run ID, task reference
//!    and full invocation path, including nested loop iterations, `then` visits,
//!    reusable-function callers and fork branches. Recovery keeps that identity;
//!    a genuinely new invocation gets a different one. A DSL retry attempt is
//!    not a new invocation for a protected call (one whose effective delivery
//!    is at most once): its identity is the first attempt's (`Record::Call::logical`),
//!    taken from the attempts the interpreter is running, never parsed out of
//!    a path that holds author-chosen task names. A protected call an earlier
//!    attempt completed returns that saved result instead of being sent again;
//!    different function or arguments under the same identity are a conflict
//!    that fails the run without sending. A failed one is sent again under its
//!    delivery policy. Every other call, a write included, runs afresh on
//!    every attempt, under the same idempotency key.
//! 4. Function, arguments and delivery policy are bound to the invocation ID.
//!    A mismatch on replay is an error, never a cache miss or a new side effect.
//!    Decisions influenced by time, external data or concurrency must be saved
//!    or reproducible from saved inputs, including the winner of `compete`.
//!
//! ## Checkpoints and ownership
//!
//! 5. A checkpoint becomes visible to dependent steps only after durable commit.
//!    Its output, context and control outcome are committed atomically. A saved
//!    call result survives a crash before the enclosing task checkpoint, so
//!    finishing that task does not call the function again. Completed invocation
//!    results cannot be overwritten by pending state or a different result.
//! 6. Save/load failure, corrupt state or lost executor ownership stops progress;
//!    there is no fallback to an empty journal or non-durable execution. A save
//!    error may mean the commit succeeded: recover from storage before proceeding.
//! 7. Only the current executor may advance a run. Ownership checks and commits
//!    are atomic; a replaced executor cannot overwrite its successor's state.
//!    Storage fencing does not undo or fence an external request already sent.
//! 8. Store the current complete run state, not necessarily every historical
//!    snapshot. It may span SQLite rows, but retains every result, decision,
//!    pending operation and wait still needed for replay and deduplication.
//!    Deleting old snapshots must not delete that evidence.
//!    Deadlines are absolute instants fixed when they start: the workflow's
//!    `timeout` from the run's admission (`resume_from`), a task's from its
//!    start, so moving between processes never resets them.
//!
//! ## External calls
//!
//! 9. Delivery policy belongs to the host's durable layer and applies to
//!    third-party functions without requiring service changes. Every call
//!    defaults to `at least once`; `at most once` is declared by the function
//!    (`Functions::at_most_once`) and a task may tighten it
//!    (`metadata.ouro.delivery`), never loosen it. This is no external exactly-once
//!    guarantee and does not require a host-side idempotency guard.
//! 10. `At least once`: durably save the received result before using it. Without
//!     a saved result, recovery may resend the call, so effects can be duplicated.
//!     A tool error response is also a result: save it and apply DSL error/retry
//!     policy rather than treating it as success or as an absent response.
//! 11. `At most once`: durably save intent before any possible dispatch, then
//!     save the response. Intent without a saved response means outcome unknown
//!     on recovery; never resend it. A crash between intent and dispatch may
//!     therefore lose the execution. HTTP 200 is not a universal durable ACK.
//! 12. An at-most-once invocation may be retried only with proof of no execution:
//!     it was not sent, or the server definitively rejected it before execution.
//!     The adapter must distinguish `not_executed` from `outcome_unknown`;
//!     timeout, disconnect after sending or a tool error alone are not such proof.
//!     Evidence allowing a retry must itself survive recovery.
//! 13. Unknown at-most-once outcomes prohibit resending at EVERY layer: restart
//!     recovery, DSL retries, host wrappers and transport retries. Allocating a
//!     new attempt ID cannot bypass this prohibition. Resuming an existing
//!     response stream is distinct from issuing a new call.
//! 14. Unknown is not successful completion and has no fabricated output.
//!     Surface it explicitly for error handling/reconciliation; dependent steps
//!     requiring the missing result cannot proceed. A host may instead name an
//!     explicit marker for it (`Functions::unknown_outcome`), saved as the
//!     call's result for a reader that reconciles it (a model told the call
//!     may have run); the call is still never sent again. Cancellation, a timeout or
//!     dropping a losing fork branch does not prove an in-flight effect stopped.
//!     For the same reason an interrupted at-most-once call (`CallFailure::Interrupted`)
//!     is unknown; an interrupted at-least-once call keeps its pending record
//!     and is called again on recovery.
//!     A function says how far its call got by the type of its error
//!     (`CallFailure::answer`): `CALL_NOT_EXECUTED` is proof of no execution
//!     (decision 12), `CALL_UNKNOWN` an outcome it cannot tell, and
//!     `CALL_INTERRUPTED` a stop; any other error is a definite answer. An
//!     unknown at-most-once outcome keeps the intent without an answer, the
//!     same record a crash after dispatch leaves, so recovery decides it the
//!     same way and the workflow sees the same error either way:
//!     `CALL_UNKNOWN` with the call's reference, which only a `catch` that
//!     names that type handles (decision 13: it never retries the attempt).
//!     A not-executed answer is saved, and recovery returns it again: the
//!     workflow already acted on it. Only a new attempt (a DSL retry) may
//!     send the call again, at most once included.
//!
//! ## Waiting and completion
//!
//! 15. Persist absolute deadlines, retry progress and wait correlation before
//!     releasing the worker. Restart does not reset timers, attempts or limits;
//!     waiting needs no live Rust future or model session. Persist a `compete`
//!     winner before exposing its result; replay cannot select a different one.
//! 16. An event is durably accepted by the host before its upstream ACK. Event,
//!     timeout and cancellation race through one atomic wait resolution;
//!     duplicate events cannot resolve it twice. Early events remain available
//!     for matching. Terminal run states are durable and cannot be reopened by
//!     recovery or late events; cancellation does not undo external effects.
//! 17. Taking an event and saving its `Record::Event` are one fenced atomic
//!     commit in the event adapter. A crash before the task checkpoint restores
//!     the same event, even after its deadline; it never consumes another one.
//!     Each loop iteration has a distinct event key. Registration is idempotent;
//!     terminal runs cannot receive events. A host must preserve a delivery's
//!     wake-up when the executor concurrently releases its worker as waiting.
//! 18. A listen consuming several events takes each under its own key
//!     (`<task>#event[i]`) and replays them in order through a pure
//!     consumer, so its decisions, output and `foreach` iterations recover.
//!     Its subscription stays open from the first take to the end, so an
//!     event published between two takes is queued, not lost; publishing is
//!     idempotent per `source` + `id`. An emitted event is saved with its
//!     generated `id` before it is handed over and re-emitted unchanged.
//! 19. A child run is admitted and linked atomically with the parent's
//!     ownership; its outcome reaches the parent as an event the parent
//!     takes at `<task>#child`. Recovery joins the same child (its id is
//!     the parent's and the invocation's) and never starts another.
//! 20. Secret values are never persisted: records hold markers restored on
//!     load from the values resolved for this execution.
//!
//! ## Budgets
//!
//! 21. A jump to a named task spends one unit of the run tree's budget under
//!     its own key (`<task>#jump`), saved as `Record::Budget` in the same
//!     commit that counts it (`Journal::spend`), so a restart neither resets
//!     the count nor spends a unit twice, and replay restores the decision.
//!     A child run spends its root's budget; a refused unit stays refused.
//! 22. A run's first refusal is decided once: the commit that refuses it
//!     also saves `Record::Exceeded` at `EXCEEDED` naming its key, and every
//!     refusal of the run carries that key (`Record::Budget::first`). Only
//!     the jump it names acts on `on_exceeded`; other branches refused at the
//!     same moment follow its outcome, and recovery and replay pick the same.
//!
//! ## Annotations
//!
//! 23. A call's answer may carry annotations: a few named numbers and short
//!     strings (a model step's token counts) the host hands back with it
//!     (`run::CallInfo::annotations`). They are saved in the same commit as
//!     the call's result, within `MAX_ANNOTATIONS` keys and
//!     `MAX_ANNOTATION_BYTES`; a call answered beyond those bounds is
//!     answered with a validation error instead. A restored call returns its
//!     saved annotations and is never asked for them again.
use crate::{model::Flow, run::WorkflowError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// 2: a listen keeps every consumed event (`#event[i]`), child runs are
/// joined at `#child`, attempts may have a deadline (`#attempt[k]`).
/// 3: the workflow deadline counts from admission (`Execution::started_ms`
/// is the admission instant), and a mailbox copy that expired before a listen
/// took it leaves `Record::Expired` at `mailbox#expired[i]`.
/// 4: the store keeps one row per record and per event copy instead of one
/// aggregate per run; a run row of an earlier version is refused.
/// 5: every jump to a named task leaves `Record::Budget` at `<task>#jump`,
/// and a run's first refusal `Record::Exceeded` at `EXCEEDED`.
/// 6: a call record no longer carries a read/write effect, nor do a run's
/// pinned function bindings.
/// 7: run rows track their admitting trigger and maintain live-run indexes;
/// one buffered firing per trigger is stored separately (queue layout 1).
/// 8: current listen matching contexts, terminal budget counts and control indexes.
/// 9: a call record keeps the annotations its answer carried.
/// 10: a run row keeps its calls' numeric annotation totals (`accumulate`),
/// added in the commit that saves each answer.
/// 11: a run row keeps the content digest of the definition version it was
/// admitted with (engine layout; the OWS record formats are unchanged).
/// 12: a run row may be held by an operator (`SavedRun::held`), which keeps
/// the run out of `due` and refuses every fenced step but the answer of a
/// call already dispatched.
/// 13: an unknown at-most-once outcome is `CALL_UNKNOWN`, which a `catch`
/// naming that type handles, and recovery returns a saved not-executed
/// answer instead of sending the call again (record formats unchanged).
/// 14: the engine's extensions are read from `metadata.ouro`, not from the
/// flat `metadata` keys (record formats unchanged; a stored definition or
/// admitted run of 13 would lose its settings).
pub const VERSION: u32 = 14;

/// The key of a run's `Record::Exceeded`.
pub const EXCEEDED: &str = "exceeded";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    AtLeastOnce,
    AtMostOnce,
}

/// The error type of a function's answer that proves the call did nothing
/// (it refused before any effect): saved as `CallState::NotExecuted`, safe to
/// send again in a new attempt, and caught like any error.
pub const CALL_NOT_EXECUTED: &str = "urn:ouro:errors:call.not_executed";
/// The error type of an answer that cannot tell whether the call had its
/// effect (a request that timed out after it was sent). An at-most-once call
/// keeps its intent without an answer and is never sent again; the workflow
/// sees this type with the call's reference, and only a `catch` that names
/// it handles it. An at-least-once call stays pending, may be sent again,
/// and its error is an ordinary one.
pub const CALL_UNKNOWN: &str = "urn:ouro:errors:call.unknown";
/// The error type of an answer that is a stop, not an answer
/// (`CallFailure::Interrupted`): nothing is saved and the run stays
/// recoverable.
pub const CALL_INTERRUPTED: &str = "urn:ouro:errors:call.interrupted";

/// The host must classify failures conservatively: an ordinary transport
/// error is Unknown, not proof that dispatch never happened.
#[derive(Debug)]
pub enum CallFailure {
    /// Proof that the call had no effect: it was not sent, or its recipient
    /// refused it before doing anything.
    NotExecuted(WorkflowError),
    /// A definite answer: the function's error.
    Response(WorkflowError),
    /// The call may or may not have had its effect.
    Unknown(WorkflowError),
    /// A stop of the host (its process or the call's owner going away), not
    /// an answer: nothing is saved for the call, the run ends `Interrupted`
    /// and recovery calls it again. For a call that has no external effect
    /// of its own (a model turn). Explicit cancellation of the run still
    /// ends it `Cancelled`, and an at-most-once call's outcome is unknown.
    Interrupted(WorkflowError),
}

impl CallFailure {
    /// A function's error answer, by its type: `CALL_NOT_EXECUTED`,
    /// `CALL_UNKNOWN` and `CALL_INTERRUPTED` say how far the call got; any
    /// other error is a definite answer (`Response`).
    pub fn answer(error: WorkflowError) -> Self {
        match error.type_.as_str() {
            CALL_NOT_EXECUTED => Self::NotExecuted(error),
            CALL_UNKNOWN => Self::Unknown(error),
            CALL_INTERRUPTED => Self::Interrupted(error),
            _ => Self::Response(error),
        }
    }
    pub fn error(&self) -> &WorkflowError {
        match self {
            Self::NotExecuted(e) | Self::Response(e) | Self::Unknown(e) | Self::Interrupted(e) => e,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CallState {
    Pending,
    NotExecuted(WorkflowError),
    Done(Result<Value, WorkflowError>),
}

/// All methods must reject an expired/replaced executor. A successful save
/// must be durable before it returns, including before an external call.
/// Load returns one consistent snapshot of this run; save replaces one record
/// atomically and must enforce `check_transition` against persisted state.
///
/// The methods are asynchronous so a journal over a network database needs
/// no thread per pending call. A dropped call must leave nothing half
/// applied: either its commit happened (and the caller, which no longer
/// knows, recovers from storage) or nothing did.
#[async_trait]
pub trait Journal: Send + Sync {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, Record>>;
    async fn save(&self, key: &str, record: &Record) -> anyhow::Result<()>;
    /// Spends one unit of the run tree's shared budget under `key` if fewer
    /// than `limit` units are spent, and saves the `Record::Budget` that says
    /// so at `key` in the same commit; repeating returns the saved record. A
    /// refusal's commit also saves `Record::Exceeded { by: key }` at
    /// `EXCEEDED` unless the run has one, and the refusal names the run's
    /// first refused key (`first`). `None`: this journal keeps no shared
    /// budget, and the interpreter counts the run's own units in records it
    /// saves itself (each refusal naming the first in its one record).
    async fn spend(&self, _key: &str, _limit: u64) -> anyhow::Result<Option<Record>> {
        Ok(None)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Execution {
        version: u32,
        run_id: String,
        definition: Value,
        input: Value,
        runtime: Value,
        started_ms: i64,
        max_tasks: usize,
        max_attempts: u32,
    },
    Task {
        started_ms: i64,
        result: Option<TaskResult>,
    },
    Call {
        /// The invocation's identity across DSL retry attempts: its reference
        /// with every enclosing attempt replaced by that `try`'s first one.
        /// Loop iterations, `then` visits, callers and branches stay distinct.
        logical: String,
        function: String,
        arguments: Value,
        delivery: Delivery,
        state: CallState,
        /// What the answer carried beside its result (decision 23).
        #[serde(default, skip_serializing_if = "Annotations::is_empty")]
        annotations: Annotations,
    },
    Event {
        event: Value,
    },
    /// A mailbox event no listen took before it expired: the store's
    /// observable evidence (`mailbox#expired[i]`), never consumed by a task.
    Expired {
        event: Value,
    },
    Timer {
        deadline_ms: i64,
    },
    Retry {
        delay_ms: Option<u64>,
    },
    Winner {
        output: Value,
    },
    Finished {
        status: crate::run::Status,
    },
    /// One unit of the run tree's budget asked for at this key: whether it
    /// was granted, and the units the tree had spent after the decision.
    Budget {
        granted: bool,
        spent: u64,
        /// For a refusal: the run's first refused key, the only jump that
        /// acts on `on_exceeded`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first: Option<String>,
    },
    /// The run's first refused jump (`by`, its key), decided in the commit
    /// that refused it.
    Exceeded {
        by: String,
    },
}

/// At most this many annotations per call.
pub const MAX_ANNOTATIONS: usize = 32;
/// At most this many bytes of a call's annotations, as JSON.
pub const MAX_ANNOTATION_BYTES: usize = 4096;

/// A small named value a call returns with its result: a number or a short
/// string. Numbers are JSON numbers (an integer stays an integer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Annotation {
    Number(serde_json::Number),
    Text(String),
}
impl Annotation {
    /// The value as a float, for a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Annotation::Number(n) => n.as_f64(),
            Annotation::Text(_) => None,
        }
    }
}
impl From<&str> for Annotation {
    fn from(value: &str) -> Self {
        Annotation::Text(value.into())
    }
}
impl From<String> for Annotation {
    fn from(value: String) -> Self {
        Annotation::Text(value)
    }
}
macro_rules! numeric_annotation {
    ($($t:ty),*) => {$(
        impl From<$t> for Annotation {
            fn from(value: $t) -> Self {
                Annotation::Number(value.into())
            }
        }
    )*};
}
numeric_annotation!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl From<f64> for Annotation {
    /// A float that is not finite has no JSON number: it is kept as text.
    fn from(value: f64) -> Self {
        serde_json::Number::from_f64(value).map_or_else(|| Annotation::Text(value.to_string()), Annotation::Number)
    }
}
impl From<f32> for Annotation {
    fn from(value: f32) -> Self {
        Annotation::from(f64::from(value))
    }
}

/// A call's annotations, by name.
pub type Annotations = BTreeMap<String, Annotation>;

/// Whether annotations fit the bounds: nonempty names, at most
/// `MAX_ANNOTATIONS` of them and `MAX_ANNOTATION_BYTES` as JSON.
pub fn check_annotations(annotations: &Annotations) -> Result<(), String> {
    if annotations.len() > MAX_ANNOTATIONS {
        return Err(format!("{} annotations, at most {MAX_ANNOTATIONS} allowed", annotations.len()));
    }
    if annotations.keys().any(String::is_empty) {
        return Err("an annotation needs a name".into());
    }
    let bytes = serde_json::to_vec(annotations).map_or(usize::MAX, |b| b.len());
    if bytes > MAX_ANNOTATION_BYTES {
        return Err(format!("annotations take {bytes} bytes, at most {MAX_ANNOTATION_BYTES} allowed"));
    }
    Ok(())
}

/// Numeric annotations summed per name.
pub type Totals = BTreeMap<String, serde_json::Number>;

/// `a + b`: integers stay integers while the sum fits; otherwise a float.
fn add(a: &serde_json::Number, b: &serde_json::Number) -> Option<serde_json::Number> {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64())
        && let Some(sum) = x.checked_add(y)
    {
        return Some(sum.into());
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64())
        && let Some(sum) = x.checked_add(y)
    {
        return Some(sum.into());
    }
    serde_json::Number::from_f64(a.as_f64()? + b.as_f64()?)
}
fn add_to(totals: &mut Totals, name: &str, n: &serde_json::Number) {
    let sum = match totals.get(name) {
        Some(total) => add(total, n).unwrap_or_else(|| total.clone()),
        None => n.clone(),
    };
    totals.insert(name.into(), sum);
}

/// The numeric annotations of `calls`, summed per name: integers stay
/// integers while the sum fits; otherwise it is a float.
pub fn annotation_totals<'a>(calls: impl IntoIterator<Item = &'a CallEntry>) -> Totals {
    let mut totals = Totals::new();
    for (name, value) in calls.into_iter().flat_map(|c| &c.annotations) {
        if let Annotation::Number(n) = value {
            add_to(&mut totals, name, n);
        }
    }
    totals
}

/// Adds the numeric annotations of one answer (or other `totals`) to
/// `totals`, summed as `annotation_totals` sums; texts are ignored. Totals a
/// store keeps stay within the annotations' own bounds: a name not yet
/// present is admitted only while `totals` keeps at most `MAX_ANNOTATIONS`
/// names and `MAX_ANNOTATION_BYTES` as JSON, so a later new name is left
/// out (deterministically: the order of the commits that add them).
/// Present names keep summing.
pub fn accumulate<'a>(totals: &mut Totals, numbers: impl IntoIterator<Item = (&'a String, &'a serde_json::Number)>) {
    for (name, n) in numbers {
        if totals.contains_key(name) {
            add_to(totals, name, n);
            continue;
        }
        if totals.len() >= MAX_ANNOTATIONS {
            continue;
        }
        totals.insert(name.clone(), n.clone());
        if serde_json::to_vec(totals).map_or(usize::MAX, |b| b.len()) > MAX_ANNOTATION_BYTES {
            totals.remove(name);
        }
    }
}
/// The numeric annotations of an answer, for `accumulate`.
pub fn numbers(annotations: &Annotations) -> impl Iterator<Item = (&String, &serde_json::Number)> {
    annotations.iter().filter_map(|(name, value)| match value {
        Annotation::Number(n) => Some((name, n)),
        Annotation::Text(_) => None,
    })
}

/// Only pending tasks/calls may advance. Everything else is immutable.
pub fn check_transition(old: &Record, new: &Record) -> anyhow::Result<()> {
    let allowed = old == new
        || match (old, new) {
            (Record::Task { started_ms: a, result: None }, Record::Task { started_ms: b, .. }) => a == b,
            (
                Record::Call { logical: l, function: a, arguments: b, delivery: d, state, .. },
                Record::Call { logical: m, function: e, arguments: f, delivery: h, .. },
            ) => !matches!(state, CallState::Done(_)) && l == m && a == e && b == f && d == h,
            _ => false,
        };
    anyhow::ensure!(allowed, "invalid durable record transition");
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    pub context: Value,
    pub outcome: StepOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StepOutcome {
    Completed { output: Value, flow: Flow },
    Faulted { error: WorkflowError, fatal: bool },
}

/// One call of a run as its host sees it: what was called, how it may be
/// delivered and how far it got. The engine does not know whether a call
/// changed the world; a host decides that from this journal and its own
/// knowledge of its functions (to retry a failed run, hand it over, or
/// reconcile it). Emitted events appear as calls of `emit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEntry {
    /// The call's record key in the run's records (`<task reference>#call`),
    /// where its arguments and result are kept.
    pub key: String,
    pub function: String,
    pub delivery: Delivery,
    pub state: CallStatus,
    /// What its answer carried beside its result.
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    pub annotations: Annotations,
}

/// How far a call got, from its durable record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    /// Its intent is saved and no answer is: an at-least-once call that may
    /// be dispatched again (on recovery, under the same idempotency key).
    Pending,
    /// The function answered with a result. An unknown at-most-once outcome
    /// the host answered with its marker (`Functions::unknown_outcome`) is
    /// completed with that marker.
    Completed,
    /// The function answered with an error.
    Failed,
    /// Proven not executed (`CallFailure::NotExecuted`): refused before
    /// dispatch, or answered `CALL_NOT_EXECUTED` by its function. No effect.
    NotExecuted,
    /// An at-most-once call whose intent has no answer: it may have run and
    /// is never dispatched again. Its function answered `CALL_UNKNOWN` or
    /// was stopped, or the process stopped before its answer was saved.
    Unknown,
}

impl CallEntry {
    /// The entry a call record stands for; `None` for any other record.
    pub fn of(key: &str, record: &Record) -> Option<Self> {
        let Record::Call { function, delivery, state, annotations, .. } = record else { return None };
        let state = match state {
            CallState::Pending if *delivery == Delivery::AtMostOnce => CallStatus::Unknown,
            CallState::Pending => CallStatus::Pending,
            CallState::NotExecuted(_) => CallStatus::NotExecuted,
            CallState::Done(Ok(_)) => CallStatus::Completed,
            CallState::Done(Err(_)) => CallStatus::Failed,
        };
        Some(Self {
            key: key.into(),
            function: function.clone(),
            delivery: *delivery,
            state,
            annotations: annotations.clone(),
        })
    }
}

/// A run's call journal from its records, in record-key order.
pub fn call_journal(records: &BTreeMap<String, Record>) -> Vec<CallEntry> {
    records.iter().filter_map(|(key, record)| CallEntry::of(key, record)).collect()
}
