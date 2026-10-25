//! Call outcomes (#25): a handler, a process runner or a dry run's mock says
//! how far its call got by the type of its error (`FunctionError::
//! not_executed`, `unknown_outcome`, `interrupted`); a plain error stays a
//! definite failure. Each outcome is recorded with its call state, shown by
//! the call journal and `Control::inspect`, reported to observers and the
//! OpenTelemetry span and metrics, kept by recovery and replay, and routed
//! by YAML. Cancellation of a long handler: what its token means and which
//! state the call it was making ends in.
use async_trait::async_trait;
use opentelemetry::{KeyValue, trace::TracerProvider};
use opentelemetry_sdk::{
    metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    },
    trace::{InMemorySpanExporter, SdkTracerProvider},
};
use ouro_runtime::{
    CALL_NOT_EXECUTED, CALL_UNKNOWN, CallStatus, Config, Delivery, Event, EventKind, Function, FunctionContext,
    FunctionError, Observer, ProcessRunner, Registry, Runtime,
    control::Control,
    durable::{self, Clock, RunStore},
    mock,
    observer::CallOutcome,
    ows::run::Status,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Time(AtomicI64);
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn noop() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new()))
}

fn doc(name: &str, body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0' }}\n{body}\n")
}

async fn save(rt: &Runtime, name: &str, body: &str) {
    let d = rt.save(name, &doc(name, body)).await.unwrap();
    rt.activate(name, d.version).await.unwrap();
}

/// How often each function was dispatched.
#[derive(Default)]
struct Dispatches(Mutex<BTreeMap<String, usize>>);
impl Dispatches {
    /// Counts a dispatch of `name`; returns how many came before it.
    fn count(&self, name: &str) -> usize {
        let mut counts = self.0.lock().unwrap();
        let n = counts.entry(name.into()).or_default();
        *n += 1;
        *n - 1
    }
    fn of(&self, name: &str) -> usize {
        self.0.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

/// A shipping service whose create has no idempotency key, so it is at
/// most once: order `timeout` times out after the request went out
/// (unknown), order `lost` loses its answer when the worker stops, any
/// other order is created. `find_shipment` looks an order's shipment up.
/// `reserve` refuses its first call before doing anything; `turn` (at least
/// once) and `send` (at most once) are stopped on their first call.
fn shipping(dispatches: Arc<Dispatches>) -> Registry {
    let mut registry = Registry::new();
    let d = dispatches.clone();
    registry
        .register(
            Function::new("create_shipment", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
                d.count("create_shipment");
                async move {
                    match args["order"].as_str() {
                        Some("timeout") => Err(FunctionError::unknown_outcome("no answer within 30 s after sending")),
                        Some("lost") => {
                            cx.cancel.cancelled().await;
                            Ok(json!({ "shipment": "never saved" }))
                        }
                        _ => Ok(json!({ "shipment": format!("S-{}", args["order"].as_str().unwrap_or("?")) })),
                    }
                }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    let d = dispatches.clone();
    registry
        .register(Function::new("find_shipment", json!({}), json!({}), move |args: Value, _: FunctionContext| {
            d.count("find_shipment");
            async move { Ok(json!({ "found": format!("S-{}", args["order"].as_str().unwrap_or("?")) })) }
        }))
        .unwrap();
    let d = dispatches.clone();
    registry
        .register(
            Function::new("reserve", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
                let first = d.count("reserve") == 0;
                async move {
                    if first {
                        Err(FunctionError::not_executed("quota check failed; nothing was reserved"))
                    } else {
                        Ok(json!({ "reserved": cx.attempt }))
                    }
                }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    for (name, delivery) in [("turn", Delivery::AtLeastOnce), ("send", Delivery::AtMostOnce)] {
        let d = dispatches.clone();
        registry
            .register(
                Function::new(name, json!({}), json!({}), move |_: Value, _: FunctionContext| {
                    let first = d.count(name) == 0;
                    async move {
                        if first { Err(FunctionError::interrupted("the host is shutting down")) } else { Ok(json!({})) }
                    }
                })
                .with_delivery(delivery),
            )
            .unwrap();
    }
    registry
        .register(Function::new("fail", json!({}), json!({}), |_: Value, _: FunctionContext| async {
            Err::<Value, _>(FunctionError::communication(502, "the carrier refused the parcel"))
        }))
        .unwrap();
    registry
}

/// The README's example: an unknown create is looked up, never sent again.
const SHIP: &str = r#"do:
  - ship:
      try:
        - create: { call: create_shipment, with: { order: '${ .order }' } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.unknown } }
        do:
          - find: { call: find_shipment, with: { order: '${ .order }' } }"#;

const RESERVE: &str = r#"do:
  - reserving:
      try:
        - reserve: { call: reserve }
      catch:
        errors: { with: { type: urn:ouro:errors:call.not_executed } }
        retry: { limit: { attempt: { count: 2 } } }"#;

fn states(calls: &[ouro_runtime::CallEntry]) -> Vec<(&str, CallStatus)> {
    calls.iter().map(|c| (c.function.as_str(), c.state)).collect()
}

#[tokio::test]
async fn an_unknown_create_is_reconciled_by_a_lookup_and_never_sent_twice() {
    let dispatches = Arc::new(Dispatches::default());
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = Runtime::new(
        store.clone(),
        Arc::new(shipping(dispatches.clone())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    save(&rt, "ship", SHIP).await;
    // The handler answered unknown: the catch that names the type looks the
    // shipment up instead.
    rt.start("timeout", "ship", json!({ "order": "timeout" })).await.unwrap();
    let found = Some(Status::Completed(json!({ "found": "S-timeout" })));
    assert_eq!(rt.drive("timeout", &CancellationToken::new()).await.unwrap(), found);
    let calls = rt.calls("timeout").await.unwrap();
    assert_eq!(states(&calls), [("find_shipment", CallStatus::Completed), ("create_shipment", CallStatus::Unknown)]);
    // An operator sees the same journal from the store alone.
    let control = Control::new(store.clone(), Arc::new(Time::default()));
    assert_eq!(control.inspect("timeout").await.unwrap().unwrap().calls, calls);
    // The worker stops while the create is in flight: its answer is lost.
    // The run's recovery finds the same evidence and takes the same branch.
    rt.start("lost", "ship", json!({ "order": "lost" })).await.unwrap();
    let stop = CancellationToken::new();
    let stopping = async {
        while dispatches.of("create_shipment") < 2 {
            tokio::task::yield_now().await;
        }
        stop.cancel();
    };
    let (status, ()) = tokio::join!(rt.drive("lost", &stop), stopping);
    assert!(matches!(status.unwrap(), Some(Status::Interrupted(_))));
    assert_eq!(states(&rt.calls("lost").await.unwrap()), [("create_shipment", CallStatus::Unknown)]);
    let found = Some(Status::Completed(json!({ "found": "S-lost" })));
    assert_eq!(rt.drive("lost", &CancellationToken::new()).await.unwrap(), found);
    assert_eq!(dispatches.of("create_shipment"), 2, "one create per run, none sent again");
    assert_eq!(dispatches.of("find_shipment"), 2);
    // Answered, a create needs no lookup.
    rt.start("ok", "ship", json!({ "order": "7" })).await.unwrap();
    let created = Some(Status::Completed(json!({ "shipment": "S-7" })));
    assert_eq!(rt.drive("ok", &CancellationToken::new()).await.unwrap(), created);
    // A catch-all does not take an unknown outcome for a definite failure.
    save(&rt, "careless", &SHIP.replace("errors: { with: { type: urn:ouro:errors:call.unknown } }", "as: error")).await;
    rt.start("careless", "careless", json!({ "order": "timeout" })).await.unwrap();
    let status = rt.drive("careless", &CancellationToken::new()).await.unwrap();
    assert!(
        matches!(&status, Some(Status::Faulted(e)) if e.type_ == CALL_UNKNOWN
        && e.instance.as_deref() == Some("/do/0/ship/try/0/create")),
        "{status:?}"
    );
    assert_eq!(dispatches.of("find_shipment"), 2);
}

#[tokio::test]
async fn a_refused_call_is_not_executed_and_only_a_retry_sends_it_again() {
    let dispatches = Arc::new(Dispatches::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(shipping(dispatches.clone())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    save(&rt, "reserve", RESERVE).await;
    rt.start("r", "reserve", json!({})).await.unwrap();
    assert_eq!(
        rt.drive("r", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({ "reserved": 2 })))
    );
    // At most once, and still sent twice: the first answer proved nothing
    // was done.
    assert_eq!(dispatches.of("reserve"), 2);
    let calls = rt.calls("r").await.unwrap();
    let keys: Vec<_> = calls.iter().map(|c| (c.key.as_str(), c.state)).collect();
    assert_eq!(
        keys,
        [
            ("/do/0/reserving/try/0/reserve#call", CallStatus::NotExecuted),
            ("/do/0/reserving/try[2]/0/reserve#call", CallStatus::Completed),
        ]
    );
    let Some(ouro_runtime::ows::durable::Record::Call {
        state: ouro_runtime::ows::durable::CallState::NotExecuted(error),
        ..
    }) = rt.records("r").await.unwrap().remove("/do/0/reserving/try/0/reserve#call")
    else {
        panic!("the refusal is saved")
    };
    assert_eq!(error.type_, CALL_NOT_EXECUTED);
    // A plain error is a definite failure, not a refusal.
    save(&rt, "failing", "do: [ { f: { call: fail } } ]").await;
    rt.start("f", "failing", json!({})).await.unwrap();
    assert!(
        matches!(rt.drive("f", &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(e)) if e.status == 502)
    );
    assert_eq!(states(&rt.calls("f").await.unwrap()), [("fail", CallStatus::Failed)]);
}

#[tokio::test]
async fn an_interrupted_call_is_called_again_on_recovery_unless_at_most_once() {
    let dispatches = Arc::new(Dispatches::default());
    let time = Arc::new(Time::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(shipping(dispatches.clone())),
        Config::default(),
        time.clone(),
        noop(),
    )
    .unwrap();
    // A stop is no answer: nothing is saved, the run stays recoverable and
    // the at-least-once call goes out again.
    save(&rt, "turn", "do: [ { t: { call: turn } } ]").await;
    rt.start("turn", "turn", json!({})).await.unwrap();
    assert!(matches!(rt.drive("turn", &CancellationToken::new()).await.unwrap(), Some(Status::Interrupted(_))));
    assert_eq!(states(&rt.calls("turn").await.unwrap()), [("turn", CallStatus::Pending)]);
    // After the interruption's backoff.
    time.0.store(60_000, Ordering::SeqCst);
    assert_eq!(rt.drive("turn", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({}))));
    assert_eq!(dispatches.of("turn"), 2);
    // A stop is no proof that an at-most-once call did nothing.
    save(&rt, "send", "do: [ { s: { call: send } } ]").await;
    rt.start("send", "send", json!({})).await.unwrap();
    let status = rt.drive("send", &CancellationToken::new()).await.unwrap();
    assert!(matches!(&status, Some(Status::Faulted(e)) if e.type_ == CALL_UNKNOWN), "{status:?}");
    assert_eq!(states(&rt.calls("send").await.unwrap()), [("send", CallStatus::Unknown)]);
    assert_eq!(dispatches.of("send"), 1);
}

/// A process runner answers like a function: `deploy` was refused before
/// it started, `migrate` (at most once) lost its connection mid-run.
struct Runner(Arc<Dispatches>);
#[async_trait]
impl ProcessRunner for Runner {
    fn runs(&self, kind: &str) -> bool {
        kind == "shell"
    }
    fn delivery(&self, _: &str) -> Delivery {
        Delivery::AtMostOnce
    }
    async fn run(&self, _: &str, definition: Value, _: FunctionContext) -> Result<Value, FunctionError> {
        let command = definition["command"].as_str().unwrap_or_default().to_owned();
        self.0.count(&command);
        match command.as_str() {
            "deploy" => Err(FunctionError::not_executed("the host is draining; nothing started")),
            _ => Err(FunctionError::unknown_outcome("the connection dropped while it ran")),
        }
    }
}

#[tokio::test]
async fn a_process_runner_answers_with_the_same_outcomes() {
    let dispatches = Arc::new(Dispatches::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(Registry::new()),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap()
    .with_process_runner(Arc::new(Runner(dispatches.clone())));
    let body = r#"do:
  - deploying:
      try: [ { deploy: { run: { shell: { command: deploy } } } } ]
      catch: { errors: { with: { type: urn:ouro:errors:call.not_executed } }, do: [ { later: { set: { later: true } } } ] }
  - migrating:
      try: [ { migrate: { run: { shell: { command: migrate } } } } ]
      catch: { errors: { with: { type: urn:ouro:errors:call.unknown } }, do: [ { check: { set: { check: true } } } ] }"#;
    save(&rt, "processes", body).await;
    rt.start("p", "processes", json!({})).await.unwrap();
    assert_eq!(
        rt.drive("p", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({ "check": true })))
    );
    let calls = rt.calls("p").await.unwrap();
    assert_eq!(states(&calls), [("run:shell", CallStatus::NotExecuted), ("run:shell", CallStatus::Unknown)]);
    assert_eq!((dispatches.of("deploy"), dispatches.of("migrate")), (1, 1));
}

#[tokio::test]
async fn replay_and_dry_runs_take_the_recorded_and_the_mocked_outcome() {
    let dispatches = Arc::new(Dispatches::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(shipping(dispatches.clone())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    save(&rt, "ship", SHIP).await;
    save(&rt, "reserve", RESERVE).await;
    rt.start("ship", "ship", json!({ "order": "timeout" })).await.unwrap();
    rt.start("reserve", "reserve", json!({})).await.unwrap();
    let mut recorded = Vec::new();
    for id in ["ship", "reserve"] {
        recorded.push(rt.drive(id, &CancellationToken::new()).await.unwrap().unwrap());
    }
    let before = dispatches.0.lock().unwrap().clone();
    // Replay dispatches nothing: the unknown create raises the same error
    // and the same catch looks it up; the refusal is answered again and
    // the retry gets the recorded success.
    for (id, status) in ["ship", "reserve"].into_iter().zip(&recorded) {
        let replay = rt.replay(id, &CancellationToken::new()).await.unwrap();
        assert_eq!(&replay.status, status, "{id}");
        assert_eq!(replay.calls_replayed, 2, "{id}");
        assert_eq!(states(&replay.calls), states(&rt.calls(id).await.unwrap()), "{id}");
    }
    assert_eq!(*dispatches.0.lock().unwrap(), before);
    // A dry run's mock answers the create unknown: the workflow's
    // reconciliation is exercised without creating anything.
    let unknown = mock(
        |function: &str, _: &Value| function == "create_shipment",
        |_: &str, _: &Value, _: &FunctionContext| Err::<Value, _>(FunctionError::unknown_outcome("simulated timeout")),
    );
    let dry =
        rt.dry_run(&doc("ship", SHIP), json!({ "order": "9" }), unknown, &CancellationToken::new()).await.unwrap();
    assert_eq!(dry.status, Status::Completed(json!({ "found": "S-9" })));
    assert_eq!(
        states(&dry.calls),
        [("find_shipment", CallStatus::Completed), ("create_shipment", CallStatus::Unknown)]
    );
    assert_eq!(dispatches.of("create_shipment"), before["create_shipment"]);
}

/// Every event it was given, in order.
#[derive(Default)]
struct Recorder(Mutex<Vec<Event>>);
impl Observer for Recorder {
    fn observe(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn observers_spans_and_metrics_name_each_outcome() {
    let spans = InMemorySpanExporter::default();
    let tracers = SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build();
    let metrics = InMemoryMetricExporter::default();
    let meters = SdkMeterProvider::builder().with_reader(PeriodicReader::builder(metrics.clone()).build()).build();
    let recorder = Arc::new(Recorder::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(shipping(Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        opentelemetry::global::BoxedTracer::new(Box::new(tracers.tracer("ouro"))),
    )
    .unwrap()
    .with_meter(opentelemetry::metrics::MeterProvider::meter(&meters, "ouro"))
    .with_observer(recorder.clone());
    save(&rt, "ship", SHIP).await;
    save(&rt, "reserve", RESERVE).await;
    save(&rt, "turn", "do: [ { t: { call: turn } } ]").await;
    for (id, name, input) in [
        ("ship", "ship", json!({ "order": "timeout" })),
        ("reserve", "reserve", json!({})),
        ("turn", "turn", json!({})),
    ] {
        rt.start(id, name, input).await.unwrap();
        rt.drive(id, &CancellationToken::new()).await.unwrap();
    }
    let answers: Vec<_> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::CallFinished { function, outcome, error, .. } => {
                Some((function.clone(), *outcome, error.as_ref().map(|e| e.type_.clone())))
            }
            _ => None,
        })
        .collect();
    let typed = |t: &str| Some(t.to_owned());
    assert_eq!(
        answers,
        [
            // The function's own error reaches observers, as it answered.
            ("create_shipment".into(), CallOutcome::Unknown, typed(CALL_UNKNOWN)),
            ("find_shipment".into(), CallOutcome::Completed, None),
            ("reserve".into(), CallOutcome::NotExecuted, typed(CALL_NOT_EXECUTED)),
            ("reserve".into(), CallOutcome::Completed, None),
            ("turn".into(), CallOutcome::Interrupted, typed(ouro_runtime::CALL_INTERRUPTED)),
        ]
    );
    tracers.force_flush().unwrap();
    let mut outcomes: Vec<_> = spans
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .filter(|s| s.name.starts_with("function "))
        .map(|s| {
            let outcome =
                s.attributes.iter().find(|kv| kv.key.as_str() == "ouro.call.outcome").map(|kv| kv.value.to_string());
            (s.name.to_string(), outcome.unwrap_or_default())
        })
        .collect();
    outcomes.sort();
    assert_eq!(
        outcomes,
        [
            ("function create_shipment".into(), "unknown".into()),
            ("function find_shipment".into(), "completed".into()),
            ("function reserve".into(), "completed".into()),
            ("function reserve".into(), "not_executed".into()),
            ("function turn".into(), "interrupted".into()),
        ]
    );
    meters.force_flush().unwrap();
    let exported = metrics.get_finished_metrics().unwrap();
    let calls = exported
        .iter()
        .rev()
        .flat_map(|r| r.scope_metrics())
        .flat_map(|s| s.metrics())
        .find(|m| m.name() == "ouro.calls")
        .unwrap();
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = calls.data() else { panic!("ouro.calls is a counter") };
    let mut counted: Vec<_> = sum
        .data_points()
        .map(|p| {
            let label =
                |key: &str| p.attributes().find(|kv: &&KeyValue| kv.key.as_str() == key).unwrap().value.to_string();
            (label("ouro.function"), label("ouro.outcome"), p.value())
        })
        .collect();
    counted.sort();
    assert_eq!(
        counted,
        [
            ("create_shipment".into(), "unknown".into(), 1),
            ("find_shipment".into(), "completed".into(), 1),
            ("reserve".into(), "completed".into(), 1),
            ("reserve".into(), "not_executed".into(), 1),
            ("turn".into(), "interrupted".into(), 1),
        ]
    );
}

/// Dropped with the handler's future: whether the runtime let it go.
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// The cancellation contract of a long handler. Its token fires when the run
// is cancelled (in this process at once, by another at the next renewal),
// the worker stops or the lease is lost. The runtime does not wait for the
// handler: it drops the handler's future at once, even one that never looks
// at its token, so a handler that started work outside its future (a remote
// job, a spawned task) watches the token to stop it. Whatever the handler
// would have answered afterwards is not recorded. The call keeps its intent
// without an answer: an at-most-once call is unknown and never sent again;
// an at-least-once call stays pending, sent again on recovery only if the
// run goes on (a stopped worker), never for a cancelled run.
#[tokio::test]
async fn a_cancelled_long_handler_is_dropped_and_its_call_keeps_its_intent() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let dispatches = Arc::new(Dispatches::default());
    let mut registry = Registry::new();
    for (name, delivery) in [("upload", Delivery::AtLeastOnce), ("charge", Delivery::AtMostOnce)] {
        let (dropped, entered, dispatches) = (dropped.clone(), entered.clone(), dispatches.clone());
        registry
            .register(
                Function::new(name, json!({}), json!({}), move |_: Value, _: FunctionContext| {
                    let guard = Dropped(dropped.clone());
                    dispatches.count(name);
                    entered.notify_one();
                    // Never looks at its token, and never answers.
                    async move {
                        let _guard = guard;
                        std::future::pending::<Result<Value, FunctionError>>().await
                    }
                })
                .with_delivery(delivery),
            )
            .unwrap();
    }
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    for name in ["upload", "charge"] {
        save(&rt, name, &format!("do: [ {{ c: {{ call: {name} }} }} ]")).await;
        // Cancelled: the run ends at once, the handler is dropped.
        let cancelled = format!("{name}-cancelled");
        rt.start(&cancelled, name, json!({})).await.unwrap();
        let before = dropped.load(Ordering::SeqCst);
        let cancel = async {
            entered.notified().await;
            assert!(rt.cancel(&cancelled).await.unwrap());
        };
        let running = CancellationToken::new();
        let (status, ()) = tokio::join!(rt.drive(&cancelled, &running), cancel);
        assert_eq!(status.unwrap(), Some(Status::Cancelled), "{name}");
        assert_eq!(dropped.load(Ordering::SeqCst), before + 1, "{name}: the handler was dropped");
        let expected = if name == "charge" { CallStatus::Unknown } else { CallStatus::Pending };
        assert_eq!(states(&rt.calls(&cancelled).await.unwrap()), [(name, expected)], "{name}");
        assert_eq!(rt.drive(&cancelled, &CancellationToken::new()).await.unwrap(), Some(Status::Cancelled));
        assert_eq!(dispatches.of(name), 1, "{name}: a cancelled run sends nothing again");
        // A stopped worker: the run goes on elsewhere, so only the
        // at-least-once call is sent again.
        let stopped = format!("{name}-stopped");
        rt.start(&stopped, name, json!({})).await.unwrap();
        let stop = CancellationToken::new();
        let stopping = async {
            entered.notified().await;
            stop.cancel();
        };
        let (status, ()) = tokio::join!(rt.drive(&stopped, &stop), stopping);
        assert!(matches!(status.unwrap(), Some(Status::Interrupted(_))), "{name}");
        assert_eq!(states(&rt.calls(&stopped).await.unwrap()), [(name, expected)], "{name}");
        let resumed = CancellationToken::new();
        let resuming = async {
            if name == "upload" {
                entered.notified().await;
                resumed.cancel();
            }
        };
        let (status, ()) = tokio::join!(rt.drive(&stopped, &resumed), resuming);
        let status = status.unwrap();
        if name == "upload" {
            assert!(matches!(status, Some(Status::Interrupted(_))));
            assert_eq!(dispatches.of(name), 3, "sent again, under the same idempotency key");
        } else {
            assert!(matches!(&status, Some(Status::Faulted(e)) if e.type_ == CALL_UNKNOWN), "{status:?}");
            assert_eq!(dispatches.of(name), 2, "one dispatch per run");
        }
    }
}

/// The README's definition, as it is printed there.
const README: &str = r#"do:
  - ship:
      try:
        - create: { call: create_shipment, with: { order: '${ .order }' } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.unknown } }
        as: unknown              # its instance names the create's task
        do:
          - find: { call: find_shipment, with: { order: '${ .order }' } }
          - escalate:
              if: '${ .shipment == null }'
              call: open_ticket
              with: { about: '${ $unknown.instance }' }"#;

#[tokio::test]
async fn the_readme_reconciliation_finds_the_shipment_or_escalates() {
    let dispatches = Arc::new(Dispatches::default());
    let mut registry = Registry::new();
    let d = dispatches.clone();
    registry
        .register(
            Function::new("create_shipment", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                d.count("create_shipment");
                async { Err::<Value, _>(FunctionError::unknown_outcome("no answer after sending")) }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    let d = dispatches.clone();
    registry
        .register(Function::new("find_shipment", json!({}), json!({}), move |args: Value, _: FunctionContext| {
            d.count("find_shipment");
            // The carrier did create order 1's shipment; order 2's never arrived.
            let shipment = if args["order"] == 1 { json!("S-1") } else { Value::Null };
            async move { Ok(json!({ "shipment": shipment })) }
        }))
        .unwrap();
    let d = dispatches.clone();
    registry
        .register(Function::new("open_ticket", json!({}), json!({}), move |args: Value, _: FunctionContext| {
            d.count("open_ticket");
            async move { Ok(json!({ "ticket": args["about"] })) }
        }))
        .unwrap();
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    save(&rt, "ship", README).await;
    for (order, expected) in [(1, json!({ "shipment": "S-1" })), (2, json!({ "ticket": "/do/0/ship/try/0/create" }))] {
        let id = format!("order-{order}");
        rt.start(&id, "ship", json!({ "order": order })).await.unwrap();
        assert_eq!(rt.drive(&id, &CancellationToken::new()).await.unwrap(), Some(Status::Completed(expected)));
    }
    assert_eq!(
        (dispatches.of("create_shipment"), dispatches.of("find_shipment"), dispatches.of("open_ticket")),
        (2, 2, 1)
    );
}
