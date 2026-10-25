//! The built-in OpenTelemetry observer (standard API only): 1. spans of
//! runs, tasks and calls, opened and closed by the runtime's events; 2. the
//! runtime's metrics. The embedding host supplies its tracer, meter and
//! exporters. Contexts are attached while polling futures, never across an
//! await with a thread-local context guard.
//!
//! The observer sees what every observer sees (`crate::observer`): each fact
//! once. A drive's spans are what that drive did: a task an earlier drive
//! entered and this one resumed has no span in it, and its calls hang under
//! the drive's root. It is the one observer the runtime asks for a trace
//! context: the drive's root span for the interpreter, the call's span for
//! its handler (`FunctionContext::span`).
//!
//! Metric attributes come from bounded sets only: definition names, outcomes,
//! function names, deliveries, task kinds, schedule kinds and skip reasons,
//! annotation names. Run ids and task paths are span attributes, never
//! metric attributes: each would make a series per run.
use crate::observer::{CallOutcome, Event, EventKind, Observer, RunOutcome, SpanRef, TaskOutcome};
use opentelemetry::{
    Context, KeyValue,
    global::BoxedTracer,
    metrics::{Counter, Gauge, Histogram, InstrumentProvider, Meter},
    trace::{Link, Span, Status, TraceContextExt, Tracer},
};
use ows::durable::{Annotation, Annotations, Delivery};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Instant,
};

// ── 1. spans ───────────────────────────────────────────────────────────────

/// A point-in-time observation (a skipped schedule slot, expired mailbox
/// events) as a span with no duration.
pub(crate) fn mark(tracer: &BoxedTracer, name: &str, attributes: Vec<KeyValue>) {
    let mut span = tracer.start(name.to_string());
    span.set_attributes(attributes);
    span.end();
}

/// The span context current where a run is admitted (the host's request
/// starting it, a parent run's span), pinned in its admission's runtime
/// bindings (`trace_id`, `span_id`) when there is one, so the control API
/// can lead from a run to its trace.
pub(crate) fn pin_trace(runtime: &mut serde_json::Value) {
    let current = Context::current();
    let span = current.span();
    let context = span.span_context();
    if context.is_valid() {
        runtime["trace_id"] = serde_json::json!(context.trace_id().to_string());
        runtime["span_id"] = serde_json::json!(context.span_id().to_string());
    }
}

fn span_ref(context: &Context) -> Option<SpanRef> {
    SpanRef::of(context.span().span_context())
}

/// One open task span: its context, kind and when it started, for the
/// step duration.
struct OpenTask {
    context: Context,
    kind: String,
    started: Instant,
}

/// One dispatched call's span, its metric attributes and when it started.
struct OpenCall {
    span: ActiveSpan,
    attributes: Vec<KeyValue>,
    started: Instant,
}

/// One drive of a run: its root span and the task and call spans open in
/// it, by task path.
struct Drive {
    root: Context,
    attributes: Vec<KeyValue>,
    definition: String,
    admitted_ms: i64,
    tasks: Mutex<BTreeMap<String, OpenTask>>,
    calls: Mutex<BTreeMap<String, OpenCall>>,
}
impl Drive {
    fn task(&self, path: &str) -> Context {
        self.tasks.lock().unwrap().get(path).map(|t| t.context.clone()).unwrap_or_else(|| self.root.clone())
    }
    fn finish(&self, metrics: &Metrics, outcome: &str) {
        self.root.span().set_attribute(KeyValue::new("ouro.outcome", outcome.to_string()));
        self.close(metrics);
        self.root.span().end();
    }
    /// Calls and tasks still open when the drive ends: interrupted.
    fn close(&self, metrics: &Metrics) {
        drop(std::mem::take(&mut *self.calls.lock().unwrap()));
        for (_, task) in std::mem::take(&mut *self.tasks.lock().unwrap()) {
            metrics.step(&self.definition, task.kind, "interrupted", task.started);
            task.context.span().set_attribute(KeyValue::new("ouro.outcome", "interrupted"));
            task.context.span().end();
        }
    }
    fn end_task(&self, metrics: &Metrics, path: &str, outcome: &'static str, error: Option<String>) -> Option<SpanRef> {
        let task = self.tasks.lock().unwrap().remove(path)?;
        metrics.step(&self.definition, task.kind, outcome, task.started);
        let cx = task.context;
        cx.span().set_attribute(KeyValue::new("ouro.outcome", outcome));
        if let Some(error) = error {
            cx.span().set_status(Status::error(error));
        }
        cx.span().end();
        span_ref(&cx)
    }
}

/// Spans and run metrics from the runtime's events: the default observer.
pub(crate) struct Telemetry {
    tracer: Arc<BoxedTracer>,
    metrics: Arc<Metrics>,
    drives: Mutex<HashMap<String, Arc<Drive>>>,
}
impl Telemetry {
    pub fn new(tracer: Arc<BoxedTracer>, metrics: Arc<Metrics>) -> Self {
        Self { tracer, metrics, drives: Mutex::new(HashMap::new()) }
    }
    fn drive(&self, run: &str) -> Option<Arc<Drive>> {
        self.drives.lock().unwrap().get(run).cloned()
    }
    /// The root span of the run's current drive.
    pub fn root(&self, run: &str) -> Option<Context> {
        self.drive(run).map(|d| d.root.clone())
    }
    /// The span of the call dispatched at `path` in the run's drive.
    pub fn call(&self, run: &str, path: &str) -> Option<Context> {
        self.drive(run)?.calls.lock().unwrap().get(path).map(|c| c.span.context.clone())
    }
    /// Records `event` and returns the span it belongs to.
    pub fn record(&self, event: &Event) -> Option<SpanRef> {
        let definition = &event.definition;
        let path = event.task_path.as_deref();
        match &event.kind {
            EventKind::RunAdmitted { .. } => {
                self.metrics.started(definition);
                None
            }
            EventKind::BreakerTripped { .. } => {
                self.metrics.tripped(definition);
                None
            }
            EventKind::RunStarted { admitted_ms, due_ms, admitted_in }
            | EventKind::RunResumed { admitted_ms, due_ms, admitted_in } => {
                // Due since `due_ms`: a waiting or ready run's wake-up, a
                // taken over run's lease expiry (the store records it at the
                // claim).
                self.metrics.claimed(definition, event.time_ms.saturating_sub(*due_ms));
                let attributes = vec![
                    KeyValue::new("ouro.run.id", event.run_id.clone()),
                    KeyValue::new("ouro.definition.name", definition.clone()),
                    KeyValue::new("ouro.definition.version", event.version.to_string()),
                ];
                // Under the context the drive was called in, linked to the
                // one the run was admitted in.
                let parent = Context::current();
                let mut builder = self.tracer.span_builder(format!("workflow {definition}"));
                if let Some(admitted) = admitted_in.as_ref().and_then(SpanRef::context) {
                    builder = builder.with_links(vec![Link::with_context(admitted)]);
                }
                let root = parent.with_span(self.tracer.build_with_context(builder, &parent));
                root.span().set_attributes(attributes.clone());
                root.span()
                    .set_attributes([KeyValue::new("ouro.task.path", "/"), KeyValue::new("ouro.attempt", 1_i64)]);
                let span = span_ref(&root);
                let drive = Arc::new(Drive {
                    root,
                    attributes,
                    definition: definition.clone(),
                    admitted_ms: *admitted_ms,
                    tasks: Mutex::default(),
                    calls: Mutex::default(),
                });
                if let Some(earlier) = self.drives.lock().unwrap().insert(event.run_id.clone(), drive) {
                    earlier.finish(&self.metrics, "interrupted");
                }
                span
            }
            EventKind::RunSuspended { .. } | EventKind::RunInterrupted { .. } | EventKind::RunEnded { .. } => {
                let drive = self.drives.lock().unwrap().remove(&event.run_id)?;
                let span = span_ref(&drive.root);
                let outcome = match &event.kind {
                    EventKind::RunSuspended { .. } => "waiting",
                    EventKind::RunEnded { outcome, limit_exceeded } => {
                        // The run ended through its jump budget.
                        if *limit_exceeded {
                            drive.root.span().set_attribute(KeyValue::new("ouro.limit_exceeded", true));
                            self.metrics.budget_exhausted(definition);
                        }
                        let label = match outcome {
                            RunOutcome::Completed { .. } => "completed",
                            RunOutcome::Faulted { .. } => "faulted",
                            RunOutcome::Cancelled => "cancelled",
                        };
                        self.metrics.ended(definition, label, event.time_ms.saturating_sub(drive.admitted_ms));
                        label
                    }
                    _ => "interrupted",
                };
                drive.finish(&self.metrics, outcome);
                span
            }
            EventKind::TaskEntered { parent, kind, .. } => {
                let drive = self.drive(&event.run_id)?;
                let path = path?;
                let parent = parent.as_deref().map(|p| drive.task(p)).unwrap_or_else(|| drive.root.clone());
                let cx = parent.with_span(self.tracer.start_with_context(format!("task {path}"), &parent));
                cx.span().set_attributes(drive.attributes.clone());
                cx.span().set_attribute(KeyValue::new("ouro.attempt", 1_i64));
                cx.span().set_attribute(KeyValue::new("ouro.task.path", path.to_string()));
                let span = span_ref(&cx);
                let task = OpenTask { context: cx, kind: kind.clone(), started: Instant::now() };
                drive.tasks.lock().unwrap().insert(path.to_string(), task);
                span
            }
            EventKind::TaskExited { outcome } => {
                let drive = self.drive(&event.run_id)?;
                let path = path?;
                match outcome {
                    TaskOutcome::Completed { .. } => drive.end_task(&self.metrics, path, "completed", None),
                    TaskOutcome::Suspended { .. } => drive.end_task(&self.metrics, path, "waiting", None),
                    TaskOutcome::Faulted { error } | TaskOutcome::Interrupted { error } => {
                        drive.end_task(&self.metrics, path, "faulted", Some(error.to_string()))
                    }
                }
            }
            // A function call's span. The engine adds no read/write
            // attribute: a host that classifies its functions adds its own
            // through the context.
            EventKind::CallStarted { function, delivery, attempt, .. } => {
                let drive = self.drive(&event.run_id)?;
                let path = path?;
                let parent = drive.task(path);
                parent.span().set_attribute(KeyValue::new("ouro.attempt", i64::from(*attempt)));
                let context = parent.with_span(self.tracer.start_with_context(format!("function {function}"), &parent));
                context.span().set_attributes(drive.attributes.clone());
                context.span().set_attributes([
                    KeyValue::new("ouro.task.path", path.to_string()),
                    KeyValue::new("ouro.function.name", function.clone()),
                    KeyValue::new("ouro.attempt", i64::from(*attempt)),
                ]);
                let span = span_ref(&context);
                let call = OpenCall {
                    span: ActiveSpan { context, finished: false },
                    attributes: call_attributes(definition, function, *delivery),
                    started: Instant::now(),
                };
                drive.calls.lock().unwrap().insert(path.to_string(), call);
                span
            }
            EventKind::CallFinished { outcome, error, annotations, .. } => {
                let drive = self.drive(&event.run_id)?;
                let mut call = drive.calls.lock().unwrap().remove(path?)?;
                // A result's annotations; an error answer's stay with its record.
                let empty = Annotations::new();
                let annotations = if *outcome == CallOutcome::Completed { annotations } else { &empty };
                self.metrics.call(&call.attributes, outcome.label(), call.started, annotations);
                call.span.annotate(annotations);
                // As the call's failure prints itself (`CallFailure`).
                let variant = match outcome {
                    CallOutcome::Completed | CallOutcome::Failed => "Response",
                    CallOutcome::NotExecuted => "NotExecuted",
                    CallOutcome::Unknown => "Unknown",
                    CallOutcome::Interrupted => "Interrupted",
                };
                let failure = error.as_ref().map(|e| format!("{variant}({e:?})"));
                call.span.finish(if failure.is_none() { "completed" } else { "faulted" }, failure);
                span_ref(&call.span.context)
            }
            _ => {
                let drive = self.drive(&event.run_id)?;
                span_ref(&path.map(|p| drive.task(p)).unwrap_or_else(|| drive.root.clone()))
            }
        }
    }
}
impl Observer for Telemetry {
    fn observe(&self, event: &Event) {
        self.record(event);
    }
}

pub(crate) struct ActiveSpan {
    pub context: Context,
    finished: bool,
}
impl ActiveSpan {
    pub fn finish(&mut self, outcome: &str, error: Option<String>) {
        self.context.span().set_attribute(KeyValue::new("ouro.outcome", outcome.to_string()));
        if let Some(error) = error {
            self.context.span().set_status(Status::error(error));
        }
        self.finished = true;
        self.context.span().end();
    }
    /// A call's annotations as attributes of its span, under their own
    /// names (`gen_ai.usage.input_tokens`).
    pub fn annotate(&self, annotations: &Annotations) {
        for (name, value) in annotations {
            let value = match value {
                Annotation::Number(n) => match n.as_i64() {
                    Some(i) => opentelemetry::Value::I64(i),
                    None => opentelemetry::Value::F64(n.as_f64().unwrap_or_default()),
                },
                Annotation::Text(text) => opentelemetry::Value::String(text.clone().into()),
            };
            self.context.span().set_attribute(KeyValue::new(name.clone(), value));
        }
    }
}
impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("interrupted", None);
        }
    }
}

// ── 2. metrics ─────────────────────────────────────────────────────────────

/// A meter that records nothing: the default until the host injects one
/// (`Runtime::with_meter`), and the meter of dry runs and replays, which
/// must not count as runs of the host.
pub(crate) fn noop_meter() -> Meter {
    struct Noop;
    impl InstrumentProvider for Noop {}
    Meter::new(Arc::new(Noop))
}

fn delivery(d: Delivery) -> &'static str {
    match d {
        Delivery::AtLeastOnce => "at_least_once",
        Delivery::AtMostOnce => "at_most_once",
    }
}

/// A call's bounded attributes (`FunctionContext::attributes`).
pub(crate) fn call_attributes(definition: &str, function: &str, d: Delivery) -> Vec<KeyValue> {
    vec![
        KeyValue::new("ouro.definition", definition.to_string()),
        KeyValue::new("ouro.function", function.to_string()),
        KeyValue::new("ouro.delivery", delivery(d)),
    ]
}

fn definition(name: &str) -> KeyValue {
    KeyValue::new("ouro.definition", name.to_string())
}

fn seconds(ms: i64) -> f64 {
    ms.max(0) as f64 / 1000.0
}

/// The runtime's instruments, built once from the host's meter. Durations
/// are histograms in seconds (`s`), counts are counters, backlog figures are
/// gauges a worker records on its heartbeat cadence.
pub(crate) struct Metrics {
    pub meter: Meter,
    /// The host injected the meter: worth reading the store's backlog for.
    enabled: bool,
    runs: Counter<u64>,
    started: Counter<u64>,
    run_duration: Histogram<f64>,
    schedule_to_start: Histogram<f64>,
    steps: Histogram<f64>,
    calls: Counter<u64>,
    call_duration: Histogram<f64>,
    annotations: Counter<f64>,
    trips: Counter<u64>,
    budget: Counter<u64>,
    fires: Counter<u64>,
    skips: Counter<u64>,
    retries: Counter<u64>,
    backlog: Gauge<u64>,
    backlog_age: Gauge<f64>,
    waits: Gauge<u64>,
    wait_age: Gauge<f64>,
}

impl Metrics {
    pub fn new(meter: Meter) -> Self {
        let counter = |name: &'static str, description: &'static str| {
            meter.u64_counter(name).with_description(description).with_unit("{run}").build()
        };
        let histogram = |name: &'static str, description: &'static str| {
            meter.f64_histogram(name).with_description(description).with_unit("s").build()
        };
        Self {
            runs: counter("ouro.runs", "Runs ended by a drive of this runtime, by definition and outcome"),
            started: counter("ouro.runs.started", "Runs this runtime admitted: started, a child, a schedule's slot"),
            run_duration: histogram("ouro.run.duration", "From a run's admission to its end"),
            schedule_to_start: histogram("ouro.run.schedule_to_start", "From a run becoming due to its claim"),
            steps: histogram("ouro.step.duration", "A task's execution within one drive"),
            calls: meter
                .u64_counter("ouro.calls")
                .with_description("Function calls by outcome and delivery")
                .with_unit("{call}")
                .build(),
            call_duration: histogram("ouro.call.duration", "A function call, dispatch to answer"),
            annotations: meter
                .f64_counter("ouro.call.annotations")
                .with_description("Numeric call annotations summed by name (`ouro.annotation`)")
                .build(),
            trips: meter
                .u64_counter("ouro.breaker.trips")
                .with_description("Breaker trips, counted once when their notice is handed on")
                .with_unit("{trip}")
                .build(),
            budget: counter("ouro.budget.exhausted", "Runs that ended through their jump budget"),
            fires: meter
                .u64_counter("ouro.schedule.fires")
                .with_description("Schedule slots fired, by outcome")
                .with_unit("{slot}")
                .build(),
            skips: meter
                .u64_counter("ouro.schedule.skips")
                .with_description("Schedule slots skipped, by reason")
                .with_unit("{slot}")
                .build(),
            retries: meter
                .u64_counter("ouro.schedule.retries")
                .with_description("Schedule slots whose admission was interrupted and is retried")
                .with_unit("{slot}")
                .build(),
            backlog: meter
                .u64_gauge("ouro.backlog.runs")
                .with_description("Runs due now and not claimed, up to a bounded page")
                .with_unit("{run}")
                .build(),
            backlog_age: meter
                .f64_gauge("ouro.backlog.age")
                .with_description("How long the oldest due run has been waiting for a worker")
                .with_unit("s")
                .build(),
            waits: meter
                .u64_gauge("ouro.waits.pending")
                .with_description("Open listens, up to a bounded page")
                .with_unit("{wait}")
                .build(),
            wait_age: meter
                .f64_gauge("ouro.waits.age")
                .with_description("How long the oldest open listen has waited")
                .with_unit("s")
                .build(),
            meter,
            enabled: false,
        }
    }
    pub fn enabled(self) -> Self {
        Self { enabled: true, ..self }
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    /// A run this runtime admitted (`start`, a child, a fired slot). A
    /// queued slot is counted when it fires (`queued`), not again when the
    /// store promotes it.
    pub fn started(&self, name: &str) {
        self.started.add(1, &[definition(name)]);
    }
    /// A claim `latency_ms` after the run became due.
    pub fn claimed(&self, name: &str, latency_ms: i64) {
        self.schedule_to_start.record(seconds(latency_ms), &[definition(name)]);
    }
    /// A terminal outcome committed by a drive, `duration_ms` after admission.
    pub fn ended(&self, name: &str, outcome: &'static str, duration_ms: i64) {
        let attributes = [definition(name), KeyValue::new("ouro.outcome", outcome)];
        self.runs.add(1, &attributes);
        self.run_duration.record(seconds(duration_ms), &attributes);
    }
    pub fn budget_exhausted(&self, name: &str) {
        self.budget.add(1, &[definition(name)]);
    }
    fn step(&self, name: &str, kind: String, outcome: &'static str, started: Instant) {
        let attributes =
            [definition(name), KeyValue::new("ouro.task.kind", kind), KeyValue::new("ouro.outcome", outcome)];
        self.steps.record(started.elapsed().as_secs_f64(), &attributes);
    }
    /// A call's answer (or its absence: `interrupted`), `started` when it
    /// was dispatched, with the numeric annotations it carried.
    pub fn call(&self, attributes: &[KeyValue], outcome: &'static str, started: Instant, annotations: &Annotations) {
        let mut labelled = attributes.to_vec();
        labelled.push(KeyValue::new("ouro.outcome", outcome));
        self.calls.add(1, &labelled);
        self.call_duration.record(started.elapsed().as_secs_f64(), &labelled);
        for (name, value) in annotations {
            // A counter only adds: a negative number is kept with the call,
            // not summed.
            if let Some(n) = value.as_f64().filter(|n| *n >= 0.0) {
                let mut labelled: Vec<KeyValue> =
                    attributes.iter().filter(|a| a.key.as_str() != "ouro.delivery").cloned().collect();
                labelled.push(KeyValue::new("ouro.annotation", name.clone()));
                self.annotations.add(n, &labelled);
            }
        }
    }
    pub fn tripped(&self, name: &str) {
        self.trips.add(1, &[definition(name)]);
    }
    /// A schedule slot fired: `admitted`, `refused` or `queued`.
    pub fn fired(&self, name: &str, kind: &str, outcome: &'static str) {
        let attributes = [
            definition(name),
            KeyValue::new("ouro.schedule.kind", kind.to_string()),
            KeyValue::new("ouro.outcome", outcome),
        ];
        self.fires.add(1, &attributes);
    }
    /// A schedule slot skipped: `missed`, `overlap` or `ceiling`.
    pub fn skipped(&self, name: &str, kind: &str, reason: &'static str) {
        let attributes = [
            definition(name),
            KeyValue::new("ouro.schedule.kind", kind.to_string()),
            KeyValue::new("ouro.schedule.reason", reason),
        ];
        self.skips.add(1, &attributes);
    }
    pub fn retried(&self, name: &str, kind: &str) {
        self.retries.add(1, &[definition(name), KeyValue::new("ouro.schedule.kind", kind.to_string())]);
    }
    /// The store-wide backlog a worker read: due runs and the oldest one's
    /// wait, open listens and the oldest one's age.
    pub fn backlog(&self, due: u64, due_age_ms: i64, waits: u64, wait_age_ms: i64) {
        self.backlog.record(due, &[]);
        self.backlog_age.record(seconds(due_age_ms), &[]);
        self.waits.record(waits, &[]);
        self.wait_age.record(seconds(wait_age_ms), &[]);
    }
}
