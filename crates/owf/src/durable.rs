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
//!    (`metadata.delivery`), never loosen it. This is no external exactly-once
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
use crate::{model::Flow, run::WorkflowError};
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
pub const VERSION: u32 = 8;

/// The key of a run's `Record::Exceeded`.
pub const EXCEEDED: &str = "exceeded";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    AtLeastOnce,
    AtMostOnce,
}

/// The host must classify failures conservatively: an ordinary transport
/// error is Unknown, not proof that dispatch never happened.
#[derive(Debug)]
pub enum CallFailure {
    NotExecuted(WorkflowError),
    Response(WorkflowError),
    Unknown(WorkflowError),
    /// A stop of the host (its process or the call's owner going away), not
    /// an answer: nothing is saved for the call, the run ends `Interrupted`
    /// and recovery calls it again. For a call that has no external effect
    /// of its own (a model turn). Explicit cancellation of the run still
    /// ends it `Cancelled`, and an at-most-once call's outcome is unknown.
    Interrupted(WorkflowError),
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
pub trait Journal: Send + Sync {
    fn load(&self) -> anyhow::Result<BTreeMap<String, Record>>;
    fn save(&self, key: &str, record: &Record) -> anyhow::Result<()>;
    /// Spends one unit of the run tree's shared budget under `key` if fewer
    /// than `limit` units are spent, and saves the `Record::Budget` that says
    /// so at `key` in the same commit; repeating returns the saved record. A
    /// refusal's commit also saves `Record::Exceeded { by: key }` at
    /// `EXCEEDED` unless the run has one, and the refusal names the run's
    /// first refused key (`first`). `None`: this journal keeps no shared
    /// budget, and the interpreter counts the run's own units in records it
    /// saves itself (each refusal naming the first in its one record).
    fn spend(&self, _key: &str, _limit: u64) -> anyhow::Result<Option<Record>> {
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

/// Only pending tasks/calls may advance. Everything else is immutable.
pub fn check_transition(old: &Record, new: &Record) -> anyhow::Result<()> {
    let allowed = old == new
        || match (old, new) {
            (Record::Task { started_ms: a, result: None }, Record::Task { started_ms: b, .. }) => a == b,
            (
                Record::Call { logical: l, function: a, arguments: b, delivery: d, state },
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
    /// Proven not executed (`CallFailure::NotExecuted`).
    NotExecuted,
    /// An at-most-once call whose intent has no answer: it may have run and
    /// is never dispatched again.
    Unknown,
}

impl CallEntry {
    /// The entry a call record stands for; `None` for any other record.
    pub fn of(key: &str, record: &Record) -> Option<Self> {
        let Record::Call { function, delivery, state, .. } = record else { return None };
        let state = match state {
            CallState::Pending if *delivery == Delivery::AtMostOnce => CallStatus::Unknown,
            CallState::Pending => CallStatus::Pending,
            CallState::NotExecuted(_) => CallStatus::NotExecuted,
            CallState::Done(Ok(_)) => CallStatus::Completed,
            CallState::Done(Err(_)) => CallStatus::Failed,
        };
        Some(Self { key: key.into(), function: function.clone(), delivery: *delivery, state })
    }
}

/// A run's call journal from its records, in record-key order.
pub fn call_journal(records: &BTreeMap<String, Record>) -> Vec<CallEntry> {
    records.iter().filter_map(|(key, record)| CallEntry::of(key, record)).collect()
}
