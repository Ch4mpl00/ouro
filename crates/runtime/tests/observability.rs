//! Observability (#20): call annotations from a function to its record, its
//! span and the metrics, across a restart, a replay and a dry run; the call's
//! span context; worker heartbeat rows; the runtime's metrics, read through
//! the OpenTelemetry SDK's in-memory exporters.
use opentelemetry::{
    KeyValue,
    trace::{Span, TraceContextExt, Tracer, TracerProvider},
};
use opentelemetry_sdk::{
    metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    },
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use ouro_runtime::{
    Annotations, CallStatus, Config, Function, FunctionContext, FunctionError, Output, Registry, Runtime,
    control::Control,
    durable::{self, Clock, Compatibility, RunStore},
    ows::run::{Status, WorkflowError},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Time(AtomicI64);
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn yaml(name: &str, body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0' }}\ndo:\n{body}\n")
}

/// The SDK's in-memory span and metric exporters behind one tracer and one
/// meter, as a host would configure them.
struct Telemetry {
    spans: InMemorySpanExporter,
    tracers: SdkTracerProvider,
    metrics: InMemoryMetricExporter,
    meters: SdkMeterProvider,
}

/// One data point: its attributes, and its value (a sum, a gauge) or, for
/// a histogram, its sum and count.
#[derive(Debug)]
struct Point {
    attributes: BTreeMap<String, String>,
    value: f64,
    count: u64,
}

impl Telemetry {
    fn new() -> Self {
        let spans = InMemorySpanExporter::default();
        let tracers = SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build();
        let metrics = InMemoryMetricExporter::default();
        let meters = SdkMeterProvider::builder().with_reader(PeriodicReader::builder(metrics.clone()).build()).build();
        Self { spans, tracers, metrics, meters }
    }
    fn runtime(&self, store: Arc<dyn RunStore>, registry: Registry, config: Config, clock: Arc<dyn Clock>) -> Runtime {
        let tracer = opentelemetry::global::BoxedTracer::new(Box::new(self.tracers.tracer("ouro")));
        Runtime::new(store, Arc::new(registry), config, clock, tracer)
            .unwrap()
            .with_meter(opentelemetry::metrics::MeterProvider::meter(&self.meters, "ouro"))
    }
    fn finished_spans(&self) -> Vec<SpanData> {
        self.tracers.force_flush().unwrap();
        self.spans.get_finished_spans().unwrap()
    }
    /// The latest export of metric `name`, after a flush.
    fn points(&self, name: &str) -> Vec<Point> {
        self.meters.force_flush().unwrap();
        let exported = self.metrics.get_finished_metrics().unwrap();
        let Some(metric) =
            exported.iter().rev().flat_map(|r| r.scope_metrics()).flat_map(|s| s.metrics()).find(|m| m.name() == name)
        else {
            return Vec::new();
        };
        let labels = |attributes: &mut dyn Iterator<Item = &KeyValue>| {
            attributes.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect::<BTreeMap<_, _>>()
        };
        macro_rules! points {
            ($data:expr) => {
                match $data {
                    MetricData::Sum(sum) => sum
                        .data_points()
                        .map(|p| Point { attributes: labels(&mut p.attributes()), value: p.value() as f64, count: 1 })
                        .collect(),
                    MetricData::Gauge(gauge) => gauge
                        .data_points()
                        .map(|p| Point { attributes: labels(&mut p.attributes()), value: p.value() as f64, count: 1 })
                        .collect(),
                    MetricData::Histogram(histogram) => histogram
                        .data_points()
                        .map(|p| Point {
                            attributes: labels(&mut p.attributes()),
                            value: p.sum() as f64,
                            count: p.count(),
                        })
                        .collect(),
                    MetricData::ExponentialHistogram(_) => Vec::new(),
                }
            };
        }
        match metric.data() {
            AggregatedMetrics::F64(data) => points!(data),
            AggregatedMetrics::U64(data) => points!(data),
            AggregatedMetrics::I64(data) => points!(data),
        }
    }
    /// The points of `name` whose attributes include every one of `with`.
    fn matching(&self, name: &str, with: &[(&str, &str)]) -> Vec<Point> {
        self.points(name)
            .into_iter()
            .filter(|p| with.iter().all(|(k, v)| p.attributes.get(*k).map(String::as_str) == Some(*v)))
            .collect()
    }
    fn total(&self, name: &str, with: &[(&str, &str)]) -> f64 {
        self.matching(name, with).iter().map(|p| p.value).sum()
    }
    fn count(&self, name: &str, with: &[(&str, &str)]) -> u64 {
        self.matching(name, with).iter().map(|p| p.count).sum()
    }
}

/// What a model step's function hands back: its text and its usage.
fn usage(value: Value) -> Output {
    Output::new(value)
        .annotate("gen_ai.usage.input_tokens", 1830)
        .annotate("gen_ai.usage.output_tokens", 120)
        .annotate("gen_ai.request.model", "small")
        .annotate("cost_usd", 0.004)
}

fn expected_usage() -> Annotations {
    usage(Value::Null).annotations
}

/// A `model` function that annotates its answer, adds an attribute to its
/// span and opens a child span for its request; `bloated` annotates too much.
fn registry(
    calls: Arc<AtomicUsize>,
    tracer: opentelemetry_sdk::trace::Tracer,
    seen: Arc<Mutex<Vec<String>>>,
) -> Registry {
    let mut registry = Registry::new();
    registry
        .register(Function::new("model", json!({}), json!({}), move |a: Value, c: FunctionContext| {
            calls.fetch_add(1, Ordering::SeqCst);
            c.span.span().set_attribute(KeyValue::new("gen_ai.operation.name", "chat"));
            let mut request = tracer.start_with_context("POST /chat", &c.span);
            request.end();
            seen.lock().unwrap().push(format!(
                "{} {} {} {:?} {:?}",
                c.definition,
                c.version,
                c.function,
                c.delivery,
                c.attributes().iter().map(|kv| kv.key.to_string()).collect::<Vec<_>>()
            ));
            async move { Ok(usage(a)) }
        }))
        .unwrap();
    registry
        .register(Function::new("bloated", json!({}), json!({}), |a: Value, _: FunctionContext| async move {
            let mut output = Output::new(a);
            for n in 0..=ouro_runtime::ows::durable::MAX_ANNOTATIONS {
                output = output.annotate(format!("k{n}"), n);
            }
            Ok::<_, FunctionError>(output)
        }))
        .unwrap();
    registry
}

#[tokio::test]
async fn annotations_reach_the_journal_the_span_and_the_metrics_and_survive_a_restart_and_a_replay() {
    let telemetry = Telemetry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = telemetry.tracers.tracer("host");
    let open = || {
        telemetry.runtime(
            Arc::new(durable::sqlite::open(&path).unwrap()),
            registry(calls.clone(), host.clone(), seen.clone()),
            Config::default(),
            time.clone(),
        )
    };
    let rt = open();
    let definition = yaml(
        "chat",
        "  - ask: { call: model, with: { q: 1 } }\n  - pause: { wait: PT1S }\n  - again: { call: model, with: { q: 2 } }",
    );
    let d = rt.save("chat", &definition).await.unwrap();
    rt.activate("chat", d.version).await.unwrap();
    rt.start("r", "chat", json!({})).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    assert_eq!(rt.calls("r").await.unwrap()[0].annotations, expected_usage());
    // A restart: a new runtime on the same database restores the first call
    // (its result and annotations) without calling it again.
    drop(rt);
    time.0.store(1_000, Ordering::SeqCst);
    let rt = open();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let journal = rt.calls("r").await.unwrap();
    assert_eq!(journal.len(), 2);
    assert!(journal.iter().all(|c| c.state == CallStatus::Completed && c.annotations == expected_usage()));
    assert_eq!(
        seen.lock().unwrap()[0],
        r#"chat 1 model AtLeastOnce ["ouro.definition", "ouro.function", "ouro.delivery"]"#
    );
    // The control API: per call and summed per run.
    let control = Control::new(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone());
    let inspection = control.inspect("r").await.unwrap().unwrap();
    assert!(inspection.calls.iter().all(|c| c.annotations == expected_usage()));
    assert_eq!(inspection.annotations["gen_ai.usage.input_tokens"], serde_json::Number::from(3660));
    assert_eq!(inspection.annotations["cost_usd"].as_f64(), Some(0.008));
    assert!(!inspection.annotations.contains_key("gen_ai.request.model"));
    // The run row keeps the same totals, so a listing needs no inspection;
    // the restart's restored first call was not counted again.
    let listed = control.list(&ouro_runtime::control::Runs::default(), None, 10).await.unwrap();
    assert_eq!(listed.items[0].annotations, inspection.annotations);
    // A replay takes results and annotations from the journal: no call.
    let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
    assert!(matches!(replay.status, Status::Completed(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(replay.calls.iter().all(|c| c.annotations == expected_usage()), "{:?}", replay.calls);
    // Each call's span carries its annotations as attributes; the host's
    // attribute and child span hang off the same span.
    let spans = telemetry.finished_spans();
    let functions: Vec<_> = spans.iter().filter(|s| s.name == "function model").collect();
    assert_eq!(functions.len(), 2);
    for f in &functions {
        let attribute = |k: &str| f.attributes.iter().find(|a| a.key.as_str() == k).map(|a| a.value.clone());
        assert_eq!(attribute("gen_ai.usage.input_tokens"), Some(opentelemetry::Value::I64(1830)));
        assert_eq!(attribute("cost_usd"), Some(opentelemetry::Value::F64(0.004)));
        assert_eq!(attribute("gen_ai.request.model"), Some(opentelemetry::Value::String("small".into())));
        assert_eq!(attribute("gen_ai.operation.name"), Some(opentelemetry::Value::String("chat".into())));
        let request = spans.iter().find(|s| s.name == "POST /chat" && s.parent_span_id == f.span_context.span_id());
        assert!(request.is_some(), "no child span under {}", f.name);
    }
    // Numeric annotations are summed by name; text ones are not metrics.
    let model = [("ouro.definition", "chat"), ("ouro.function", "model")];
    let tokens = [model.as_slice(), &[("ouro.annotation", "gen_ai.usage.input_tokens")]].concat();
    assert_eq!(telemetry.total("ouro.call.annotations", &tokens), 3660.0);
    let cost = [model.as_slice(), &[("ouro.annotation", "cost_usd")]].concat();
    assert!((telemetry.total("ouro.call.annotations", &cost) - 0.008).abs() < 1e-12);
    assert!(telemetry.matching("ouro.call.annotations", &[("ouro.annotation", "gen_ai.request.model")]).is_empty());
    let completed = [model.as_slice(), &[("ouro.delivery", "at_least_once"), ("ouro.outcome", "completed")]].concat();
    assert_eq!(telemetry.total("ouro.calls", &completed), 2.0);
    assert_eq!(telemetry.count("ouro.call.duration", &completed), 2);
    // No metric attribute names a run.
    for name in ["ouro.calls", "ouro.call.annotations", "ouro.runs", "ouro.step.duration"] {
        assert!(telemetry.points(name).iter().all(|p| !p.attributes.contains_key("ouro.run.id")), "{name}");
    }
}

#[tokio::test]
async fn a_dry_run_keeps_the_annotations_of_mocked_and_dispatched_calls_and_counts_no_metrics() {
    let telemetry = Telemetry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = registry(calls.clone(), telemetry.tracers.tracer("host"), Arc::default());
    registry
        .register(Function::new("send", json!({}), json!({}), |a: Value, _: FunctionContext| async move { Ok(a) }))
        .unwrap();
    let rt =
        telemetry.runtime(Arc::new(durable::memory::memory()), registry, Config::default(), Arc::new(Time::default()));
    let mock = ouro_runtime::mock(
        |function: &str, _: &Value| function == "send",
        |_: &str, a: &Value, _: &FunctionContext| Ok(Output::new(a.clone()).annotate("cost_usd", 0.01)),
    );
    let definition =
        yaml("dry", "  - ask: { call: model, with: { q: 1 } }\n  - send: { call: send, with: { to: someone } }");
    let dry = rt.dry_run(&definition, json!({}), mock, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the unmarked call is dispatched");
    assert_eq!(dry.mocked.len(), 1);
    assert_eq!(dry.mocked[0].annotations, [("cost_usd".to_owned(), 0.01.into())].into());
    assert_eq!(dry.calls.len(), 2);
    assert_eq!(dry.calls[0].annotations, expected_usage());
    assert_eq!(dry.calls[1].annotations, dry.mocked[0].annotations);
    let totals = ouro_runtime::annotation_totals(&dry.calls);
    assert_eq!(totals["cost_usd"].as_f64(), Some(0.014));
    // A dry run is no run of the host: its spans are traced, not counted.
    assert!(telemetry.points("ouro.calls").is_empty());
    assert!(telemetry.points("ouro.runs").is_empty());
    assert!(telemetry.finished_spans().iter().any(|s| s.name == "function model"));
}

#[tokio::test]
async fn annotations_beyond_their_bounds_answer_the_call_with_a_validation_error() {
    let telemetry = Telemetry::new();
    let registry = registry(Arc::default(), telemetry.tracers.tracer("host"), Arc::default());
    let rt =
        telemetry.runtime(Arc::new(durable::memory::memory()), registry, Config::default(), Arc::new(Time::default()));
    let definition = yaml(
        "big",
        "  - guarded:\n      try: [ { big: { call: bloated } } ]\n      catch: { errors: { with: { type: 'https://open-workflow-specification.org/spec/1.0.0/errors/validation' } }, do: [ { caught: { set: { caught: true } } } ] }",
    );
    let d = rt.save("big", &definition).await.unwrap();
    rt.activate("big", d.version).await.unwrap();
    rt.start("r", "big", json!({})).await.unwrap();
    assert_eq!(
        rt.drive("r", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({"caught":true})))
    );
    let journal = rt.calls("r").await.unwrap();
    assert_eq!(journal[0].state, CallStatus::Failed);
    assert!(journal[0].annotations.is_empty());
    let failed = telemetry.matching("ouro.calls", &[("ouro.function", "bloated"), ("ouro.outcome", "failed")]);
    assert_eq!(failed.len(), 1);
    assert!(telemetry.points("ouro.call.annotations").is_empty());
}

#[tokio::test]
async fn a_run_admitted_in_a_span_pins_its_context_and_every_drive_links_to_it() {
    let telemetry = Telemetry::new();
    let mut registry = Registry::new();
    registry
        .register(Function::new("noop", json!({}), json!({}), |a: Value, _: FunctionContext| async { Ok(a) }))
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let time = Arc::new(Time::default());
    let rt = telemetry.runtime(store.clone(), registry, Config::default(), time.clone());
    let d = rt.save("pinned", &yaml("pinned", "  - a: { call: noop }")).await.unwrap();
    rt.activate("pinned", d.version).await.unwrap();
    let request = telemetry.tracers.tracer("host").start("POST /runs");
    let admitted = request.span_context().clone();
    let cx = opentelemetry::Context::current_with_span(request);
    {
        let _attached = cx.clone().attach();
        rt.start("r", "pinned", json!({})).await.unwrap();
    }
    rt.drive("r", &CancellationToken::new()).await.unwrap();
    let inspection = Control::new(store, time).inspect("r").await.unwrap().unwrap();
    assert_eq!(inspection.trace_id, Some(admitted.trace_id().to_string()));
    assert_eq!(inspection.span_id, Some(admitted.span_id().to_string()));
    cx.span().end();
    let root = telemetry.finished_spans().into_iter().find(|s| s.name == "workflow pinned").unwrap();
    assert!(
        root.links.iter().any(|l| l.span_context.trace_id() == admitted.trace_id()
            && l.span_context.span_id() == admitted.span_id()),
        "{:?}",
        root.links
    );
}

#[tokio::test]
async fn the_runtime_records_run_step_call_schedule_breaker_and_budget_metrics() {
    let telemetry = Telemetry::new();
    let time = Arc::new(Time::default());
    let mut registry = Registry::new();
    registry
        .register(Function::new("noop", json!({}), json!({}), |a: Value, _: FunctionContext| async { Ok(a) }))
        .unwrap();
    registry
        .register(Function::new("fail", json!({}), json!({}), |_: Value, _: FunctionContext| async {
            Err::<Value, _>(WorkflowError::runtime("down"))
        }))
        .unwrap();
    let config = Config { breaker_threshold: 1, ..Config::default() };
    let rt = telemetry.runtime(Arc::new(durable::memory::memory()), registry, config, time.clone());
    let save = async |name: &str, yaml: &str| {
        let d = rt.save(name, yaml).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    };
    // A run due at 1 000 and claimed at 1 500, which waits and resumes.
    save("metered", &yaml("metered", "  - a: { call: noop }\n  - pause: { wait: PT1S }\n  - b: { call: noop }")).await;
    rt.start("ok", "metered", json!({})).await.unwrap();
    time.0.store(500, Ordering::SeqCst);
    rt.drive("ok", &CancellationToken::new()).await.unwrap();
    time.0.store(2_000, Ordering::SeqCst);
    assert!(matches!(rt.drive("ok", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    let metered = [("ouro.definition", "metered")];
    assert_eq!(telemetry.total("ouro.runs.started", &metered), 1.0);
    assert_eq!(telemetry.total("ouro.runs", &[metered[0], ("ouro.outcome", "completed")]), 1.0);
    let duration = telemetry.matching("ouro.run.duration", &[metered[0], ("ouro.outcome", "completed")]);
    assert_eq!((duration[0].count, duration[0].value), (1, 2.0));
    // Two claims: 0.5 s after admission, then 0.5 s after the wait ended at 1.5 s.
    let latency = telemetry.matching("ouro.run.schedule_to_start", &metered);
    assert_eq!((latency[0].count, latency[0].value), (2, 1.0));
    assert_eq!(
        telemetry.count("ouro.step.duration", &[metered[0], ("ouro.task.kind", "call"), ("ouro.outcome", "completed")]),
        2
    );
    assert_eq!(
        telemetry.count("ouro.step.duration", &[metered[0], ("ouro.task.kind", "wait"), ("ouro.outcome", "waiting")]),
        1
    );
    // A child is a started run of its own definition.
    save("leaf", &yaml("leaf", "  - ok: { set: { ok: true } }")).await;
    save("parent", &yaml("parent", "  - kid: { run: { workflow: { namespace: test, name: leaf, version: latest } } }"))
        .await;
    rt.start("parent", "parent", json!({})).await.unwrap();
    for _ in 0..10 {
        rt.recover_once(&CancellationToken::new()).await.unwrap();
    }
    assert!(matches!(rt.get("parent").await.unwrap().unwrap().outcome, Some(Status::Completed(_))));
    assert_eq!(telemetry.total("ouro.runs.started", &[("ouro.definition", "leaf")]), 1.0);
    assert_eq!(telemetry.total("ouro.runs", &[("ouro.definition", "leaf"), ("ouro.outcome", "completed")]), 1.0);
    // A fault trips the breaker (threshold 1): counted once, when handed on.
    save("broken", &yaml("broken", "  - a: { call: fail }")).await;
    rt.start("bad", "broken", json!({})).await.unwrap();
    assert!(matches!(rt.drive("bad", &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(_))));
    assert_eq!(telemetry.total("ouro.runs", &[("ouro.definition", "broken"), ("ouro.outcome", "faulted")]), 1.0);
    assert_eq!(telemetry.total("ouro.calls", &[("ouro.function", "fail"), ("ouro.outcome", "failed")]), 1.0);
    assert_eq!(rt.notify(&CancellationToken::new()).await.unwrap(), 1);
    assert_eq!(rt.notify(&CancellationToken::new()).await.unwrap(), 0);
    assert_eq!(telemetry.total("ouro.breaker.trips", &[("ouro.definition", "broken")]), 1.0);
    // A loop that ends through its jump budget.
    save(
        "loop",
        "document: { dsl: '1.0.4', namespace: test, name: loop, version: '1.0.0', metadata: { limits: { max_jumps: 3, on_exceeded: end } } }\ndo:\n  - tick: { call: noop }\n  - again: { switch: [ { stop: { when: '${ false }', then: exit } }, { more: { then: tick } } ] }\n",
    )
    .await;
    rt.start("loop", "loop", json!({})).await.unwrap();
    assert!(matches!(rt.drive("loop", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    assert_eq!(telemetry.total("ouro.budget.exhausted", &[("ouro.definition", "loop")]), 1.0);
    // Schedule slots: admitted, skipped while one is live, missed under
    // `catchup: none`.
    let every = |name: &str, metadata: &str| {
        format!(
            "document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0', metadata: {{ {metadata} }} }}\nschedule: {{ every: PT1S }}\ndo:\n  - hold: {{ wait: PT1H }}\n"
        )
    };
    save("ticking", &every("ticking", "overlap: skip")).await;
    time.0.store(3_000, Ordering::SeqCst);
    let started = rt.fire_due().await.unwrap();
    rt.drive(&started[0], &CancellationToken::new()).await.unwrap();
    time.0.store(4_000, Ordering::SeqCst);
    assert!(rt.fire_due().await.unwrap().is_empty());
    let ticking = [("ouro.definition", "ticking"), ("ouro.schedule.kind", "every")];
    assert_eq!(telemetry.total("ouro.schedule.fires", &[ticking[0], ticking[1], ("ouro.outcome", "admitted")]), 1.0);
    assert_eq!(
        telemetry.total("ouro.schedule.skips", &[ticking[0], ticking[1], ("ouro.schedule.reason", "overlap")]),
        1.0
    );
    save("late", &every("late", "catchup: none")).await;
    time.0.store(10_000, Ordering::SeqCst);
    rt.fire_due().await.unwrap();
    assert_eq!(
        telemetry.total("ouro.schedule.skips", &[("ouro.definition", "late"), ("ouro.schedule.reason", "missed")]),
        1.0
    );
    // Every attribute set is bounded: no run ids, no task paths.
    for name in ["ouro.runs", "ouro.run.schedule_to_start", "ouro.step.duration", "ouro.schedule.fires"] {
        for p in telemetry.points(name) {
            assert!(
                p.attributes
                    .keys()
                    .all(|k| k.starts_with("ouro.") && !["ouro.run.id", "ouro.task.path"].contains(&k.as_str())),
                "{name}: {p:?}"
            );
        }
    }
}

/// A worker beats on its heartbeat cadence: one row with its identity and
/// the runs it holds, listed fresh while it runs and stale once it stops; it
/// also records the store's backlog of due runs and open listens.
#[tokio::test]
async fn a_worker_beats_one_row_and_records_the_backlog() {
    let telemetry = Telemetry::new();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let mut registry = Registry::new();
    registry
        .register(Function::new("noop", json!({}), json!({}), |a: Value, _: FunctionContext| async { Ok(a) }))
        .unwrap();
    let clock = Arc::new(durable::SystemClock);
    let config = Config {
        host_name: Some("host-a".into()),
        application: Compatibility { version: Some("2".into()), accepts: Vec::new() },
        heartbeat_interval: Duration::from_millis(20),
        poll_interval: Duration::from_millis(5),
        ..Config::default()
    };
    let rt = telemetry.runtime(store.clone(), registry, config, clock.clone());
    let listen = yaml("listening", "  - hear: { listen: { to: { one: { with: { type: answer } } } } }");
    let d = rt.save("listening", &listen).await.unwrap();
    rt.activate("listening", d.version).await.unwrap();
    let mut other = rt.start("waiting", "listening", json!({})).await.unwrap().admission;
    // A run of another application version: due, and no worker here claims it.
    other.id = "foreign".into();
    other.runtime["application_version"] = json!("1");
    store.admit(&other, 0).await.unwrap();
    let stop = CancellationToken::new();
    let control = Control::new(store.clone(), clock);
    let workers = {
        let worker = rt.worker(stop.clone());
        let observe = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let page = control.workers(None, 10).await.unwrap();
            stop.cancel();
            page
        };
        let (worked, page) = tokio::join!(worker, observe);
        worked.unwrap();
        page
    };
    assert_eq!(workers.items.len(), 1);
    let row = &workers.items[0];
    assert!(!row.stale);
    assert_eq!(row.worker.id, rt.worker_id());
    assert_eq!((row.worker.host.as_deref(), row.worker.application.as_deref()), (Some("host-a"), Some("2")));
    // Stopped cleanly: still one row, stale at once and holding nothing.
    let after = control.workers(None, 10).await.unwrap();
    assert_eq!(after.items.len(), 1);
    assert!(after.items[0].stale && after.items[0].worker.held == 0);
    // The foreign run stays due; the listening run's open listen is counted.
    assert_eq!(telemetry.points("ouro.backlog.runs").last().map(|p| p.value), Some(1.0));
    assert_eq!(telemetry.points("ouro.waits.pending").last().map(|p| p.value), Some(1.0));
    assert!(telemetry.points("ouro.backlog.age")[0].value >= 0.0);
}
