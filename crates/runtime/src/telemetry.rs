//! Standard OpenTelemetry API only. The embedding host supplies its tracer
//! and exporter. Contexts are attached while polling futures, never across an
//! await with a thread-local context guard.
use opentelemetry::{
    Context, KeyValue,
    global::BoxedTracer,
    trace::{Span, Status, TraceContextExt, Tracer},
};
use ows::run::{Event, Observer};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// A point-in-time observation (a skipped schedule slot, expired mailbox
/// events) as a span with no duration.
pub(crate) fn mark(tracer: &BoxedTracer, name: &str, attributes: Vec<KeyValue>) {
    let mut span = tracer.start(name.to_string());
    span.set_attributes(attributes);
    span.end();
}

pub(crate) struct Spans {
    pub tracer: Arc<BoxedTracer>,
    root: Context,
    attributes: Vec<KeyValue>,
    tasks: Mutex<BTreeMap<String, Context>>,
}
impl Spans {
    pub fn new(tracer: Arc<BoxedTracer>, parent: &Context, run: &ouro_durable::Admission) -> Self {
        let attributes = vec![
            KeyValue::new("ouro.run.id", run.id.clone()),
            KeyValue::new("ouro.definition.name", run.name.clone()),
            KeyValue::new("ouro.definition.version", run.version.to_string()),
        ];
        let root = parent.with_span(tracer.start_with_context(format!("workflow {}", run.name), parent));
        root.span().set_attributes(attributes.clone());
        root.span().set_attributes([KeyValue::new("ouro.task.path", "/"), KeyValue::new("ouro.attempt", 1_i64)]);
        Self { tracer, root, attributes, tasks: Mutex::new(BTreeMap::new()) }
    }
    pub fn root(&self) -> Context {
        self.root.clone()
    }
    pub fn task(&self, path: &str) -> Context {
        self.tasks.lock().unwrap().get(path).cloned().unwrap_or_else(|| self.root.clone())
    }
    /// A function call's span. The engine adds no read/write attribute: a
    /// host that classifies its functions adds its own through the context.
    pub fn function(&self, name: &str, path: &str, attempt: u32) -> ActiveSpan {
        let parent = self.task(path);
        parent.span().set_attribute(KeyValue::new("ouro.attempt", i64::from(attempt)));
        let context = parent.with_span(self.tracer.start_with_context(format!("function {name}"), &parent));
        context.span().set_attributes(self.attributes.clone());
        context.span().set_attributes([
            KeyValue::new("ouro.task.path", path.to_string()),
            KeyValue::new("ouro.function.name", name.to_string()),
            KeyValue::new("ouro.attempt", i64::from(attempt)),
        ]);
        ActiveSpan { context, finished: false }
    }
    /// The run ended through its jump budget (`Outcome::limit_exceeded`).
    pub fn limit_exceeded(&self) {
        self.root.span().set_attribute(KeyValue::new("ouro.limit_exceeded", true));
    }
    pub fn finish(&self, outcome: &str) {
        self.root.span().set_attribute(KeyValue::new("ouro.outcome", outcome.to_string()));
        self.close_tasks();
        self.root.span().end();
    }
    fn close_tasks(&self) {
        for (_, cx) in std::mem::take(&mut *self.tasks.lock().unwrap()) {
            cx.span().set_attribute(KeyValue::new("ouro.outcome", "interrupted"));
            cx.span().end();
        }
    }
    fn end_task(&self, reference: &str, outcome: &str, error: Option<String>) {
        if let Some(cx) = self.tasks.lock().unwrap().remove(reference) {
            cx.span().set_attribute(KeyValue::new("ouro.outcome", outcome.to_string()));
            if let Some(error) = error {
                cx.span().set_status(Status::error(error));
            }
            cx.span().end();
        }
    }
}
impl Observer for Spans {
    fn event(&self, event: &Event<'_>) {
        match event {
            Event::TaskStarted { reference, parent, .. } => {
                let parent = parent.map(|p| self.task(p)).unwrap_or_else(|| self.root.clone());
                let cx = parent.with_span(self.tracer.start_with_context(format!("task {reference}"), &parent));
                cx.span().set_attributes(self.attributes.clone());
                cx.span().set_attribute(KeyValue::new("ouro.attempt", 1_i64));
                cx.span().set_attribute(KeyValue::new("ouro.task.path", reference.to_string()));
                self.tasks.lock().unwrap().insert(reference.to_string(), cx);
            }
            Event::TaskCompleted { reference, .. } => self.end_task(reference, "completed", None),
            Event::TaskFaulted { reference, error } => self.end_task(reference, "faulted", Some(error.to_string())),
            Event::TaskSuspended { reference, .. } => self.end_task(reference, "waiting", None),
            Event::TaskSkipped { .. } | Event::SwitchCase { .. } | Event::TaskRetrying { .. } => {}
        }
    }
}
impl Drop for Spans {
    fn drop(&mut self) {
        self.close_tasks();
        self.root.span().end();
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
}
impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("interrupted", None);
        }
    }
}
