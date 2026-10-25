//! Run events for the host: 1. the events and their envelope; 2. the
//! observer seam; 3. a bounded buffer for slow or asynchronous sinks; 4.
//! dispatch inside the runtime: the built-in OpenTelemetry observer and the
//! host's, payload bounds, panic isolation, and the interpreter's events in
//! their envelope.
//!
//! An event is a fact an execution established, reported once: replaying
//! what an earlier execution recorded reports nothing, and resuming a run is
//! one `RunResumed` (`ows::run::Event` decides it for the steps of a run).
//! Observers only inform: they receive a shared reference after the fact
//! and return nothing the runtime reads. Each is called synchronously, in
//! registration order after the OpenTelemetry observer, on the task that
//! established the fact: the run waits while an observer runs, so an
//! observer returns promptly and never blocks; a slow sink sits behind
//! `buffered`, which costs one clone and a non-blocking enqueue. A panic is
//! caught at each call and ends only that call. Delivery is at most once:
//! a crash between a commit and its event loses the event, and nothing
//! delivers it later. Within a run, an observer sees events in the order
//! they happened in one execution (concurrent branches interleave); runs
//! are not ordered with each other.
use crate::telemetry::Telemetry;
use ouro_durable::{Admission, Clock, SavedRun};
use ows::{
    durable::{Annotations, CallFailure, Delivery},
    run::{self, WorkflowError},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

// ── 1. events ──────────────────────────────────────────────────────────────

/// A span the event belongs to, as W3C trace-context hex ids: the span the
/// built-in OpenTelemetry observer opened for the run's drive, task or call
/// (or, for an admission, the span the run was admitted in).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanRef {
    pub trace_id: String,
    pub span_id: String,
}
impl SpanRef {
    pub fn of(context: &opentelemetry::trace::SpanContext) -> Option<Self> {
        context
            .is_valid()
            .then(|| SpanRef { trace_id: context.trace_id().to_string(), span_id: context.span_id().to_string() })
    }
    /// The span context, for a link or a remote parent.
    pub fn context(&self) -> Option<opentelemetry::trace::SpanContext> {
        let trace = opentelemetry::trace::TraceId::from_hex(&self.trace_id).ok()?;
        let span = opentelemetry::trace::SpanId::from_hex(&self.span_id).ok()?;
        let context = opentelemetry::trace::SpanContext::new(trace, span, Default::default(), true, Default::default());
        context.is_valid().then_some(context)
    }
    /// The span a run was admitted in, pinned in its admission.
    pub(crate) fn pinned(admission: &Admission) -> Option<Self> {
        let pinned = SpanRef {
            trace_id: admission.runtime["trace_id"].as_str()?.to_owned(),
            span_id: admission.runtime["span_id"].as_str()?.to_owned(),
        };
        pinned.context().map(|_| pinned)
    }
}

/// One event of a run, in its envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub run_id: String,
    /// The run that owns this one (an owned child's parent).
    pub parent_id: Option<String>,
    /// The root of the run's tree, the run itself for a root; `None` where
    /// the runtime does not know it (a breaker trip names only its run).
    pub root_id: Option<String>,
    pub definition: String,
    pub version: u64,
    /// The task the event is about (`/do/0/fetch`); `None` for the run.
    pub task_path: Option<String>,
    pub span: Option<SpanRef>,
    /// When it happened, by the runtime's clock.
    pub time_ms: i64,
    pub kind: EventKind,
}

/// What happened. Payloads (inputs, outputs, arguments, results, events)
/// are `None` when their JSON exceeds `Config::event_payload_bytes`; the
/// run's secrets are redacted from every value and error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// The run was admitted (`start`, a child, a schedule slot), in the
    /// commit that stores it. A slot the host refuses and a buffered slot
    /// the store promotes later are not reported.
    RunAdmitted { input: Option<Value> },
    /// An execution of the run began and found no record of an earlier
    /// one: the run's first, or every earlier one ended before saving
    /// anything. `due_ms`: when the run became due for this execution.
    RunStarted { admitted_ms: i64, due_ms: i64, admitted_in: Option<SpanRef> },
    /// An execution of the run began after an earlier one (a wake-up, a
    /// takeover, a recovery), or could not read whether there was one. What
    /// the earlier executions reported is not reported again.
    RunResumed { admitted_ms: i64, due_ms: i64, admitted_in: Option<SpanRef> },
    /// The execution ended with the run waiting until `wake_at_ms` (or a
    /// delivery), committed.
    RunSuspended { wake_at_ms: i64 },
    /// The execution ended without a terminal state: its worker stopped,
    /// its lease was lost, a commit or a host call failed. The run stays
    /// recoverable (an interruption past the store's budget faults it, as
    /// `RunEnded`).
    RunInterrupted { error: WorkflowError },
    /// The execution ended the run, or found it cancelled. A run cancelled
    /// while no execution holds it is not reported.
    RunEnded { outcome: RunOutcome, limit_exceeded: bool },
    /// The task was entered, once per run: an execution that re-enters it
    /// reports nothing.
    TaskEntered { parent: Option<String>, kind: String, input: Option<Value> },
    /// The task's `if` was false.
    TaskSkipped { parent: Option<String> },
    /// The task left: with its committed result (once), or, for this
    /// execution only, suspended or interrupted (a later execution re-enters
    /// it silently and reports its exit).
    TaskExited { outcome: TaskOutcome },
    /// A `switch` took `case`.
    SwitchTaken { case: String },
    /// The task's `then: to` asked for a unit of the jump budget.
    Jumped { to: String, granted: bool },
    /// A `try` caught `error` and runs its body again after `delay_ms`.
    RetryScheduled { attempt: u32, delay_ms: u64, error: WorkflowError },
    /// A `wait` task's durable timer, due at `deadline_ms`.
    TimerSet { deadline_ms: i64 },
    /// A call (or a process run, `run:<kind>`) is dispatched. A call whose
    /// earlier dispatch got no answer before a crash is dispatched, and
    /// reported, again (at least once).
    CallStarted { function: String, delivery: Delivery, attempt: u32, arguments: Option<Value> },
    /// The dispatched call's answer. A call its task's interruption drops
    /// has none.
    CallFinished {
        function: String,
        delivery: Delivery,
        outcome: CallOutcome,
        result: Option<Value>,
        error: Option<WorkflowError>,
        annotations: Annotations,
    },
    /// A `listen` waits for an event of `types`.
    Listening { types: Vec<String> },
    /// The task consumed an event: a listen's, or the outcome of the child
    /// its `run: workflow` joined.
    EventConsumed { event: Option<Value> },
    /// `emit` handed an event over (its envelope).
    EventEmitted { event: Option<Value> },
    /// The definition's breaker tripped (the envelope names the run whose
    /// fault reached the threshold and the tripped version), reported once
    /// by the worker whose hand-off acknowledged the notice.
    BreakerTripped { epoch: u64, faults: u32, threshold: u32, error: WorkflowError },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    Completed { output: Option<Value> },
    Faulted { error: WorkflowError },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed { output: Option<Value> },
    Faulted { error: WorkflowError },
    Suspended { wake_at_ms: i64 },
    Interrupted { error: WorkflowError },
}

/// How a dispatched call was answered: the labels of the `ouro.calls`
/// metric. `Completed`, `Failed` and `NotExecuted` answers are saved;
/// `Unknown` and `Interrupted` leave the call's intent pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    Completed,
    Failed,
    NotExecuted,
    Unknown,
    Interrupted,
}
impl CallOutcome {
    pub fn label(self) -> &'static str {
        match self {
            CallOutcome::Completed => "completed",
            CallOutcome::Failed => "failed",
            CallOutcome::NotExecuted => "not_executed",
            CallOutcome::Unknown => "unknown",
            CallOutcome::Interrupted => "interrupted",
        }
    }
}

// ── 2. the observer seam ───────────────────────────────────────────────────

/// Receives the runtime's events (`Runtime::with_observer`). Called
/// synchronously on the task that established the fact, so it returns
/// promptly; a panic is caught and ends only that call. It cannot change
/// the run: it gets the event after the fact and returns nothing.
pub trait Observer: Send + Sync {
    fn observe(&self, event: &Event);
}

// ── 3. a bounded buffer ────────────────────────────────────────────────────

/// An observer that queues a copy of each event for the host's own task
/// (`BufferedEvents`), at most `capacity` at once, so a slow or
/// asynchronous sink never slows a run: a full queue drops the event and
/// counts it (`dropped`). The order is kept; a drop leaves a gap. The
/// runtime starts no task for it: the host drains the queue.
pub struct Buffered {
    sender: tokio::sync::mpsc::Sender<Event>,
    dropped: AtomicU64,
}
/// The receiving end of `buffered`: the events in the order they were
/// observed. `recv` ends once the runtime and every handle to the
/// `Buffered` observer are gone.
pub struct BufferedEvents(tokio::sync::mpsc::Receiver<Event>);

/// A bounded buffer of `capacity` (at least one) events.
pub fn buffered(capacity: usize) -> (Arc<Buffered>, BufferedEvents) {
    let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1));
    (Arc::new(Buffered { sender, dropped: AtomicU64::new(0) }), BufferedEvents(receiver))
}
impl Buffered {
    /// Events dropped because the queue was full (or its receiver gone).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
impl Observer for Buffered {
    fn observe(&self, event: &Event) {
        if self.sender.try_send(event.clone()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl BufferedEvents {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await
    }
    /// An event already queued, without waiting.
    pub fn try_recv(&mut self) -> Option<Event> {
        self.0.try_recv().ok()
    }
}

// ── 4. dispatch ────────────────────────────────────────────────────────────

/// The runtime's observers: the built-in OpenTelemetry observer, unless the
/// host removed it, then the host's, in registration order.
#[derive(Clone)]
pub(crate) struct Observers {
    pub telemetry: Option<Arc<Telemetry>>,
    pub host: Vec<Arc<dyn Observer>>,
    /// The largest payload an event carries, as JSON bytes.
    pub payload_bytes: usize,
}
impl Observers {
    fn silent(&self) -> bool {
        self.telemetry.is_none() && self.host.is_empty()
    }
    /// A payload for the event: only the host's observers read payloads,
    /// so none is copied without them, and none beyond the bound.
    fn payload(&self, value: &Value) -> Option<Value> {
        (!self.host.is_empty() && fits(value, self.payload_bytes)).then(|| value.clone())
    }
    /// Calls every observer, each under its own panic guard. The
    /// OpenTelemetry observer goes first: it opens the span the event
    /// carries to the others.
    fn dispatch(&self, mut event: Event) {
        if let Some(telemetry) = &self.telemetry
            && let Ok(Some(span)) = catch_unwind(AssertUnwindSafe(|| telemetry.record(&event)))
        {
            event.span = Some(span);
        }
        for observer in &self.host {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(&event)));
        }
    }
    /// A run admitted in a commit of this runtime, in its tree.
    pub fn admitted(&self, admission: &Admission, parent: Option<&str>, root: &str, now_ms: i64) {
        if self.silent() {
            return;
        }
        self.dispatch(Event {
            run_id: admission.id.clone(),
            parent_id: parent.map(str::to_owned),
            root_id: Some(root.to_owned()),
            definition: admission.name.clone(),
            version: admission.version,
            task_path: None,
            span: SpanRef::pinned(admission),
            time_ms: now_ms,
            kind: EventKind::RunAdmitted { input: self.payload(&admission.input) },
        });
    }
    /// A breaker trip's notice, handed on: its CloudEvent's report.
    pub fn tripped(&self, notice: &Value, now_ms: i64) {
        let data = &notice["data"];
        if self.silent() || notice["type"] != ouro_durable::TRIPPED {
            return;
        }
        let number = |k: &str| data[k].as_u64().unwrap_or_default();
        self.dispatch(Event {
            run_id: data["run"].as_str().unwrap_or_default().to_owned(),
            parent_id: None,
            root_id: None,
            definition: data["name"].as_str().unwrap_or_default().to_owned(),
            version: number("version"),
            task_path: None,
            span: None,
            time_ms: now_ms,
            kind: EventKind::BreakerTripped {
                epoch: number("epoch"),
                faults: u32::try_from(number("faults")).unwrap_or(u32::MAX),
                threshold: u32::try_from(number("threshold")).unwrap_or(u32::MAX),
                error: serde_json::from_value(data["reason"].clone())
                    .unwrap_or_else(|_| WorkflowError::runtime("the breaker tripped")),
            },
        });
    }
}

/// Whether `value`'s JSON is at most `limit` bytes, counted without
/// building it and abandoned at the bound.
fn fits(value: &Value, limit: usize) -> bool {
    struct Count(usize, usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > self.1 { Err(std::io::ErrorKind::FileTooLarge.into()) } else { Ok(bytes.len()) }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count(0, limit), value).is_ok()
}

/// One drive's events: the run's envelope, the interpreter's steps in it,
/// and the drive's start and end. Dropped with the drive: a drive that
/// opened (`RunStarted`/`RunResumed`) and never reported its end (its future
/// was dropped) reports itself interrupted then.
pub(crate) struct RunEvents {
    observers: Arc<Observers>,
    clock: Arc<dyn Clock>,
    run_id: String,
    parent_id: Option<String>,
    root_id: String,
    definition: String,
    version: u64,
    open: AtomicBool,
}
impl RunEvents {
    pub fn new(observers: Arc<Observers>, clock: Arc<dyn Clock>, run: &SavedRun) -> Self {
        let a = &run.admission;
        RunEvents {
            observers,
            clock,
            run_id: a.id.clone(),
            parent_id: run.child.as_ref().map(|c| c.parent.clone()),
            root_id: run.root.clone().unwrap_or_else(|| a.id.clone()),
            definition: a.name.clone(),
            version: a.version,
            open: AtomicBool::new(false),
        }
    }
    pub fn observers(&self) -> &Observers {
        &self.observers
    }
    /// The root of this run's tree, which its owned children share.
    pub fn root(&self) -> &str {
        &self.root_id
    }
    fn emit(&self, task_path: Option<&str>, kind: EventKind) {
        self.observers.dispatch(Event {
            run_id: self.run_id.clone(),
            parent_id: self.parent_id.clone(),
            root_id: Some(self.root_id.clone()),
            definition: self.definition.clone(),
            version: self.version,
            task_path: task_path.map(str::to_owned),
            span: None,
            time_ms: self.clock.now_ms(),
            kind,
        });
    }
    /// The drive began: `resumed` when it found an earlier execution's
    /// records (or could not read them).
    pub fn began(&self, run: &SavedRun, resumed: bool) {
        if self.observers.silent() {
            return;
        }
        let (admitted_ms, due_ms, admitted_in) = (run.admitted_ms, run.wake_at_ms, SpanRef::pinned(&run.admission));
        self.open.store(true, Ordering::SeqCst);
        self.emit(
            None,
            match resumed {
                false => EventKind::RunStarted { admitted_ms, due_ms, admitted_in },
                true => EventKind::RunResumed { admitted_ms, due_ms, admitted_in },
            },
        );
    }
    /// The drive ended: suspended, interrupted, or with the run's end.
    pub fn ended(&self, status: &run::Status, limit_exceeded: bool) {
        if !self.open.swap(false, Ordering::SeqCst) {
            return;
        }
        let kind = match status {
            run::Status::Waiting { wake_at_ms } => EventKind::RunSuspended { wake_at_ms: *wake_at_ms },
            run::Status::Interrupted(error) => EventKind::RunInterrupted { error: error.clone() },
            run::Status::Completed(output) => EventKind::RunEnded {
                outcome: RunOutcome::Completed { output: self.observers.payload(output) },
                limit_exceeded,
            },
            run::Status::Faulted(error) => {
                EventKind::RunEnded { outcome: RunOutcome::Faulted { error: error.clone() }, limit_exceeded }
            }
            run::Status::Cancelled => EventKind::RunEnded { outcome: RunOutcome::Cancelled, limit_exceeded },
        };
        self.emit(None, kind);
    }
    /// The trace context the call at `path` runs in: the span the
    /// OpenTelemetry observer opened for its dispatch.
    pub fn call_context(&self, path: &str) -> Option<opentelemetry::Context> {
        self.observers.telemetry.as_ref()?.call(&self.run_id, path)
    }
    /// The trace context the drive runs in: its root span.
    pub fn root_context(&self) -> Option<opentelemetry::Context> {
        self.observers.telemetry.as_ref()?.root(&self.run_id)
    }
}
impl Drop for RunEvents {
    fn drop(&mut self) {
        self.ended(&run::Status::Interrupted(WorkflowError::runtime("the drive was dropped")), false);
    }
}
impl run::Observer for RunEvents {
    fn event(&self, event: &run::Event<'_>) {
        if self.observers.silent() {
            return;
        }
        let payload = |value: &Value| self.observers.payload(value);
        let (path, kind) = match event {
            run::Event::TaskStarted { reference, parent, kind, input } => (
                reference,
                EventKind::TaskEntered {
                    parent: parent.map(str::to_owned),
                    kind: (*kind).to_owned(),
                    input: payload(input),
                },
            ),
            run::Event::TaskSkipped { reference, parent } => {
                (reference, EventKind::TaskSkipped { parent: parent.map(str::to_owned) })
            }
            run::Event::TaskCompleted { reference, output } => {
                (reference, EventKind::TaskExited { outcome: TaskOutcome::Completed { output: payload(output) } })
            }
            run::Event::TaskFaulted { reference, error } => {
                (reference, EventKind::TaskExited { outcome: TaskOutcome::Faulted { error: (*error).clone() } })
            }
            run::Event::TaskSuspended { reference, wake_at_ms } => {
                (reference, EventKind::TaskExited { outcome: TaskOutcome::Suspended { wake_at_ms: *wake_at_ms } })
            }
            run::Event::TaskInterrupted { reference, error } => {
                (reference, EventKind::TaskExited { outcome: TaskOutcome::Interrupted { error: (*error).clone() } })
            }
            run::Event::SwitchCase { reference, case } => {
                (reference, EventKind::SwitchTaken { case: (*case).to_owned() })
            }
            run::Event::TaskRetrying { reference, attempt, delay, error } => (
                reference,
                EventKind::RetryScheduled {
                    attempt: *attempt,
                    delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error: (*error).clone(),
                },
            ),
            run::Event::Jumped { reference, to, granted } => {
                (reference, EventKind::Jumped { to: (*to).to_owned(), granted: *granted })
            }
            run::Event::Timer { reference, deadline_ms } => {
                (reference, EventKind::TimerSet { deadline_ms: *deadline_ms })
            }
            run::Event::CallStarted { reference, function, delivery, attempt, arguments } => (
                reference,
                EventKind::CallStarted {
                    function: (*function).to_owned(),
                    delivery: *delivery,
                    attempt: *attempt,
                    arguments: payload(arguments),
                },
            ),
            run::Event::CallFinished { reference, function, delivery, result, annotations } => {
                let (outcome, error) = match result {
                    Ok(_) => (CallOutcome::Completed, None),
                    Err(CallFailure::Response(e)) => (CallOutcome::Failed, Some(e)),
                    Err(CallFailure::NotExecuted(e)) => (CallOutcome::NotExecuted, Some(e)),
                    Err(CallFailure::Unknown(e)) => (CallOutcome::Unknown, Some(e)),
                    Err(CallFailure::Interrupted(e)) => (CallOutcome::Interrupted, Some(e)),
                };
                (
                    reference,
                    EventKind::CallFinished {
                        function: (*function).to_owned(),
                        delivery: *delivery,
                        outcome,
                        result: result.ok().and_then(payload),
                        error: error.cloned(),
                        annotations: (*annotations).clone(),
                    },
                )
            }
            run::Event::Listening { reference, types } => (reference, EventKind::Listening { types: types.to_vec() }),
            run::Event::EventConsumed { reference, event } => {
                (reference, EventKind::EventConsumed { event: payload(event) })
            }
            run::Event::EventEmitted { reference, event } => {
                (reference, EventKind::EventEmitted { event: payload(event) })
            }
            _ => return,
        };
        self.emit(Some(path), kind);
    }
}
