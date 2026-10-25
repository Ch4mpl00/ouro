//! The observer seam (#26): every event kind with its envelope and span;
//! no event an earlier execution produced is delivered again after a crash
//! and a restart (memory, SQLite, Postgres); observers that panic or fall
//! behind cannot fail or change a run; composed observers see the same
//! events in order; dry runs and replays report nothing to them; payloads
//! are bounded and secrets redacted.
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use ouro_runtime::{
    Config, Event, EventKind, Function, FunctionContext, Observer, Registry, Runtime, SecretResolver,
    durable::{self, Clock, RunStore},
    observer::{CallOutcome, SpanRef, TaskOutcome},
    ows::run::{Status, WorkflowError},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
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
impl Time {
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

/// Every event it was given, in order.
#[derive(Default)]
struct Recorder(Mutex<Vec<Event>>);
impl Observer for Recorder {
    fn observe(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
}
impl Recorder {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

/// An observer that panics on every event.
struct Panics;
impl Observer for Panics {
    fn observe(&self, _: &Event) {
        panic!("an observer's bug");
    }
}

/// The event's kind as its serialized tag (`task_entered`).
fn kind(event: &Event) -> String {
    serde_json::to_value(&event.kind).unwrap()["type"].as_str().unwrap().to_owned()
}

fn doc(name: &str, body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0' }}\n{body}\n")
}

fn noop() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new()))
}

async fn save(rt: &Runtime, name: &str, yaml: &str) {
    let d = rt.save(name, yaml).await.unwrap();
    rt.activate(name, d.version).await.unwrap();
}

/// `echo` answers its arguments; `flaky` fails its first call; `fail`
/// always fails; `block` signals `entered` and never answers its first call.
fn registry(flaky: Arc<AtomicUsize>, blocks: Arc<AtomicUsize>, entered: Arc<tokio::sync::Notify>) -> Registry {
    let mut registry = Registry::new();
    registry
        .register(Function::new("echo", json!({}), json!({}), |a: Value, _: FunctionContext| async { Ok(a) }))
        .unwrap();
    registry
        .register(Function::new("flaky", json!({}), json!({}), move |a: Value, _: FunctionContext| {
            let first = flaky.fetch_add(1, Ordering::SeqCst) == 0;
            async move { if first { Err(WorkflowError::communication(503, "busy")) } else { Ok(a) } }
        }))
        .unwrap();
    registry
        .register(Function::new("fail", json!({}), json!({}), |_: Value, _: FunctionContext| async {
            Err::<Value, _>(WorkflowError::runtime("down"))
        }))
        .unwrap();
    registry
        .register(Function::new("block", json!({}), json!({}), move |a: Value, _: FunctionContext| {
            let first = blocks.fetch_add(1, Ordering::SeqCst) == 0;
            let entered = entered.clone();
            async move {
                if first {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(a)
            }
        }))
        .unwrap();
    registry
}

const MAIN: &str = r#"do:
  - init: { set: { n: 0 } }
  - step: { set: { n: '${ .n + 1 }' } }
  - again: { switch: [ { more: { when: '${ .n < 2 }', then: step } }, { done: { then: continue } } ] }
  - skipped: { if: '${ false }', set: { never: true } }
  - fetch: { call: echo, with: { n: '${ .n }' } }
  - retrying:
      try: [ { attempt: { call: flaky } } ]
      catch: { retry: { delay: { seconds: 1 }, limit: { attempt: { count: 2 } } } }
  - pause: { wait: PT1S }
  - kid: { run: { workflow: { namespace: test, name: leaf, version: latest } } }
  - hear: { listen: { to: { one: { with: { type: answer } } } } }
  - tell: { emit: { event: { with: { type: told, source: 'https://test', data: { ok: true } } } } }"#;

/// Every event kind, each in its envelope: the run, its tree, its
/// definition, the task, the span the OpenTelemetry observer opened and the
/// runtime's clock.
#[tokio::test]
async fn every_event_kind_is_reported_in_its_envelope() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let time = Arc::new(Time::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let recorder = Arc::new(Recorder::default());
    let config = Config { breaker_threshold: 1, ..Config::default() };
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), entered.clone())),
        config,
        time.clone(),
        opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("observer"))),
    )
    .unwrap()
    .with_observer(recorder.clone());
    save(&rt, "main", &doc("main", MAIN)).await;
    save(&rt, "leaf", &doc("leaf", "do: [ { ok: { set: { ok: true } } } ]")).await;
    save(&rt, "broken", &doc("broken", "do: [ { a: { call: fail } } ]")).await;
    save(&rt, "slow", &doc("slow", "do: [ { hold: { call: block } } ]")).await;
    let go = CancellationToken::new();

    rt.start("r", "main", json!({})).await.unwrap();
    // The retry's delay, then the wait, then the child.
    assert!(matches!(rt.drive("r", &go).await.unwrap(), Some(Status::Waiting { .. })));
    time.set(1_000);
    assert!(matches!(rt.drive("r", &go).await.unwrap(), Some(Status::Waiting { .. })));
    time.set(2_000);
    for _ in 0..5 {
        rt.recover_once(&go).await.unwrap();
    }
    let answer = json!({ "id": "a1", "source": "https://test", "type": "answer", "data": 42 });
    assert_eq!(rt.deliver(answer, Some("r")).await.unwrap(), Some("r".into()));
    assert!(matches!(rt.drive("r", &go).await.unwrap(), Some(Status::Completed(_))));
    // A worker that stops while a call is in flight interrupts its run.
    rt.start("s", "slow", json!({})).await.unwrap();
    let stop = CancellationToken::new();
    let (stopped, ()) = tokio::join!(rt.drive("s", &stop), async {
        entered.notified().await;
        stop.cancel();
    });
    assert!(matches!(stopped.unwrap(), Some(Status::Interrupted(_))));
    // A fault trips the breaker (threshold 1): reported when handed on.
    rt.start("b", "broken", json!({})).await.unwrap();
    assert!(matches!(rt.drive("b", &go).await.unwrap(), Some(Status::Faulted(_))));
    assert_eq!(rt.notify(&go).await.unwrap(), 1);

    let events = recorder.events();
    let kinds: BTreeSet<String> = events.iter().map(kind).collect();
    let all = [
        "run_admitted",
        "run_started",
        "run_resumed",
        "run_suspended",
        "run_interrupted",
        "run_ended",
        "task_entered",
        "task_skipped",
        "task_exited",
        "switch_taken",
        "jumped",
        "retry_scheduled",
        "timer_set",
        "call_started",
        "call_finished",
        "listening",
        "event_consumed",
        "event_emitted",
        "breaker_tripped",
    ];
    assert_eq!(kinds, all.iter().map(|k| k.to_string()).collect(), "{events:#?}");
    // The run's own events: its tree, its definition, a task path exactly
    // for a task's events.
    let main: Vec<&Event> = events.iter().filter(|e| e.run_id == "r").collect();
    for e in &main {
        assert_eq!((e.parent_id.as_deref(), e.root_id.as_deref()), (None, Some("r")), "{e:?}");
        assert_eq!((e.definition.as_str(), e.version), ("main", 1));
        assert_eq!(e.task_path.is_none(), kind(e).starts_with("run_"), "{e:?}");
    }
    // The child is owned by `r` and in its tree.
    let leaf: Vec<&Event> = events.iter().filter(|e| e.definition == "leaf").collect();
    assert!(!leaf.is_empty());
    assert!(leaf.iter().all(|e| e.parent_id.as_deref() == Some("r") && e.root_id.as_deref() == Some("r")));
    assert_eq!(kind(leaf[0]), "run_admitted");
    // Times are the runtime's clock: the resumption after the retry's delay.
    let resumed: Vec<i64> = main.iter().filter(|e| kind(e) == "run_resumed").map(|e| e.time_ms).collect();
    assert_eq!(resumed[0], 1_000);
    let find = |k: &str, path: &str| {
        main.iter().find(|e| kind(e) == k && e.task_path.as_deref() == Some(path)).copied().unwrap_or_else(|| {
            panic!("no {k} at {path}: {main:#?}");
        })
    };
    // Payloads and details.
    assert!(matches!(&find("task_entered", "/do/4/fetch").kind,
        EventKind::TaskEntered { kind, input: Some(input), .. } if kind == "call" && input["n"] == 2));
    assert!(matches!(&find("switch_taken", "/do/2/again").kind, EventKind::SwitchTaken { case } if case == "more"));
    assert!(matches!(&find("jumped", "/do/2/again").kind, EventKind::Jumped { to, granted: true } if to == "step"));
    assert!(matches!(&find("retry_scheduled", "/do/5/retrying").kind,
        EventKind::RetryScheduled { attempt: 1, delay_ms: 1_000, error } if error.status == 503));
    assert!(matches!(&find("timer_set", "/do/6/pause").kind, EventKind::TimerSet { deadline_ms: 2_000 }));
    assert!(matches!(&find("call_finished", "/do/4/fetch").kind,
        EventKind::CallFinished { function, outcome: CallOutcome::Completed, result: Some(r), .. }
            if function == "echo" && r["n"] == 2));
    assert!(matches!(&find("call_finished", "/do/5/retrying/try/0/attempt").kind,
        EventKind::CallFinished { outcome: CallOutcome::Failed, error: Some(e), .. } if e.status == 503));
    assert!(matches!(&find("event_consumed", "/do/8/hear").kind,
        EventKind::EventConsumed { event: Some(e) } if e["data"] == 42));
    assert!(matches!(&find("event_consumed", "/do/7/kid").kind,
        EventKind::EventConsumed { event: Some(e) } if e["data"]["status"] == "completed"));
    assert!(matches!(&find("event_emitted", "/do/9/tell").kind,
        EventKind::EventEmitted { event: Some(e) } if e["type"] == "told"));
    assert!(matches!(
        &find("task_exited", "/do/6/pause").kind,
        EventKind::TaskExited { outcome: TaskOutcome::Suspended { wake_at_ms: 2_000 } }
    ));
    let interrupted = events.iter().find(|e| e.run_id == "s" && kind(e) == "task_exited").unwrap();
    assert!(
        matches!(&interrupted.kind, EventKind::TaskExited { outcome: TaskOutcome::Interrupted { error } } if error.status == 499)
    );
    let tripped = events.iter().find(|e| kind(e) == "breaker_tripped").unwrap();
    assert_eq!((tripped.run_id.as_str(), tripped.definition.as_str()), ("b", "broken"));
    assert!(
        matches!(&tripped.kind, EventKind::BreakerTripped { faults: 1, threshold: 1, digest, .. } if digest.len() == 64)
    );
    // Spans: a task's events carry its span, a call's its function span,
    // the run's its drive's root.
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let span = |name: &str| {
        let s = spans.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no span {name}"));
        Some(SpanRef { trace_id: s.span_context.trace_id().to_string(), span_id: s.span_context.span_id().to_string() })
    };
    assert_eq!(find("task_entered", "/do/4/fetch").span, span("task /do/4/fetch"));
    assert_eq!(find("task_exited", "/do/4/fetch").span, span("task /do/4/fetch"));
    assert_eq!(find("call_started", "/do/4/fetch").span, span("function echo"));
    let started = main.iter().find(|e| kind(e) == "run_started").unwrap();
    assert_eq!(
        started.span,
        spans
            .iter()
            .filter(|s| s.name == "workflow main")
            .map(|s| SpanRef {
                trace_id: s.span_context.trace_id().to_string(),
                span_id: s.span_context.span_id().to_string(),
            })
            .next()
    );
}

/// The facts of a run, from every observer that heard them: what entered
/// and exited each task, what each call was dispatched as.
fn count(events: &[Event], what: &str, path: &str) -> usize {
    events.iter().filter(|e| kind(e) == what && e.task_path.as_deref() == Some(path)).count()
}

const RESTART: &str = r#"do:
  - a: { call: echo, with: { n: 1 } }
  - pick: { switch: [ { go: { when: '${ true }', then: continue } } ] }
  - b: { call: block }
  - pause: { wait: PT1S }
  - c: { call: echo, with: { n: 3 } }"#;

/// A worker dies inside `b`'s call (its drive is dropped; the process is
/// gone) and a new runtime on the same store takes the run over after the
/// lease. The new runtime's observer hears the resumption and what happens
/// next, never what the dead worker's execution produced: `a`, `pick` and
/// their calls and switch case are silent, `b` is re-entered silently and
/// its call, which had no answer, is dispatched again. Across both, every
/// task entered once and exited once with its result.
async fn no_event_is_delivered_again_after_a_restart(open: impl Fn() -> Arc<dyn RunStore>) {
    let time = Arc::new(Time::default());
    let blocks = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let before = Arc::new(Recorder::default());
    let runtime = |observer: Arc<Recorder>| {
        Runtime::new(
            open(),
            Arc::new(registry(Arc::default(), blocks.clone(), entered.clone())),
            Config::default(),
            time.clone(),
            noop(),
        )
        .unwrap()
        .with_observer(observer)
    };
    let rt = runtime(before.clone());
    save(&rt, "restart", &doc("restart", RESTART)).await;
    rt.start("r", "restart", json!({})).await.unwrap();
    let go = CancellationToken::new();
    tokio::select! {
        _ = rt.drive("r", &go) => panic!("the blocked call answered"),
        _ = entered.notified() => {}
    }
    drop(rt);
    let after = Arc::new(Recorder::default());
    let rt = runtime(after.clone());
    time.set(durable::LEASE_MS + 1);
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    time.set(durable::LEASE_MS + 2_000);
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));

    let (before, after) = (before.events(), after.events());
    let kinds: Vec<String> = after.iter().map(kind).collect();
    assert_eq!(kinds.first().map(String::as_str), Some("run_resumed"), "{after:#?}");
    assert!(!kinds.contains(&"run_started".to_string()) && !kinds.contains(&"run_admitted".to_string()));
    assert_eq!(kinds.iter().filter(|k| *k == "run_resumed").count(), 2);
    for path in ["/do/0/a", "/do/1/pick"] {
        assert!(after.iter().all(|e| e.task_path.as_deref() != Some(path)), "{path} again: {after:#?}");
    }
    assert_eq!(count(&after, "task_entered", "/do/2/b"), 0, "b is re-entered silently");
    assert_eq!(count(&after, "call_started", "/do/2/b"), 1, "b's unanswered call is dispatched again");
    assert_eq!(count(&before, "call_started", "/do/2/b"), 1);
    let all: Vec<Event> = before.iter().chain(&after).cloned().collect();
    for path in ["/do/0/a", "/do/1/pick", "/do/2/b", "/do/3/pause", "/do/4/c"] {
        assert_eq!(count(&all, "task_entered", path), 1, "{path} entered: {all:#?}");
        let results = all
            .iter()
            .filter(|e| {
                e.task_path.as_deref() == Some(path)
                    && matches!(&e.kind, EventKind::TaskExited { outcome: TaskOutcome::Completed { .. } })
            })
            .count();
        assert_eq!(results, 1, "{path} completed: {all:#?}");
    }
    assert_eq!(count(&all, "switch_taken", "/do/1/pick"), 1);
    assert_eq!(count(&all, "call_started", "/do/0/a"), 1);
    assert_eq!(count(&all, "timer_set", "/do/3/pause"), 1);
    // The dead worker's drive reported its own end as it went.
    assert_eq!(kind(before.last().unwrap()), "run_interrupted");
}

#[tokio::test]
async fn memory_delivers_no_event_again_after_a_restart() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    no_event_is_delivered_again_after_a_restart(|| store.clone()).await;
}

#[tokio::test]
async fn sqlite_delivers_no_event_again_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    no_event_is_delivered_again_after_a_restart(|| Arc::new(durable::sqlite::open(&path).unwrap())).await;
}

#[tokio::test]
async fn postgres_delivers_no_event_again_after_a_restart() {
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };
    let container;
    let server = if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
        url.parse::<tokio_postgres::Config>().unwrap()
    } else {
        container = GenericImage::new("postgres", "16")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
            .with_env_var("POSTGRES_PASSWORD", "local-test-password")
            .start()
            .await
            .unwrap();
        let mut config = tokio_postgres::Config::new();
        config
            .host(container.get_host().await.unwrap().to_string())
            .port(container.get_host_port_ipv4(5432).await.unwrap())
            .user("postgres")
            .password("local-test-password")
            .dbname("postgres");
        config
    };
    let (admin, connection) = server.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let name = format!("ouro_observer_{}", uuid::Uuid::new_v4().simple());
    admin.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
    let mut config = server.clone();
    config.dbname(&name);
    // Each runtime opens its own pool, as a restarted process would.
    let first = durable::postgres::PostgresBackend::connect_config(config.clone()).await.unwrap();
    let second = durable::postgres::PostgresBackend::connect_config(config).await.unwrap();
    let stores: Mutex<Vec<Arc<dyn RunStore>>> = Mutex::new(vec![
        Arc::new(durable::TransactionalStore::new(Arc::new(second))),
        Arc::new(durable::TransactionalStore::new(Arc::new(first))),
    ]);
    no_event_is_delivered_again_after_a_restart(|| stores.lock().unwrap().pop().unwrap()).await;
    admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
}

/// An observer that panics and one that never drains its buffer cannot
/// fail, stop or change a run; the observers after them still hear every
/// event.
#[tokio::test]
async fn observers_that_panic_or_fall_behind_cannot_affect_a_run() {
    let definition = doc(
        "plain",
        "do:\n  - a: { call: echo, with: { n: 1 } }\n  - pick: { switch: [ { go: { when: '${ true }', then: continue } } ] }\n  - b: { call: fail }",
    );
    let quiet = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap();
    let (full, _undrained) = ouro_runtime::buffered(1);
    let recorder = Arc::new(Recorder::default());
    let noisy = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap()
    .with_observer(Arc::new(Panics))
    .with_observer(full.clone())
    .with_observer(recorder.clone());
    let mut outcomes = Vec::new();
    for rt in [&quiet, &noisy] {
        save(rt, "plain", &definition).await;
        rt.start("r", "plain", json!({})).await.unwrap();
        let status = rt.drive("r", &CancellationToken::new()).await.unwrap();
        outcomes.push((status, rt.calls("r").await.unwrap()));
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert!(matches!(&outcomes[1].0, Some(Status::Faulted(e)) if e.detail.as_deref() == Some("down")));
    let heard = recorder.events();
    assert_eq!(kind(heard.last().unwrap()), "run_ended");
    // The buffer kept one event and dropped the others.
    assert_eq!(full.dropped(), u64::try_from(heard.len()).unwrap() - 1);
}

/// Observers registered together hear the same events in the same order,
/// and a drained buffer keeps that order.
#[tokio::test]
async fn composed_observers_hear_the_same_events_in_order() {
    let (first, second) = (Arc::new(Recorder::default()), Arc::new(Recorder::default()));
    let (buffer, mut drained) = ouro_runtime::buffered(1_000);
    let time = Arc::new(Time::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        Config::default(),
        time.clone(),
        noop(),
    )
    .unwrap()
    .with_observer(first.clone())
    .with_observer(buffer.clone())
    .with_observer(second.clone());
    save(&rt, "main", &doc("main", MAIN)).await;
    save(&rt, "leaf", &doc("leaf", "do: [ { ok: { set: { ok: true } } } ]")).await;
    rt.start("r", "main", json!({})).await.unwrap();
    for at in [0, 1_000, 2_000] {
        time.set(at);
        for _ in 0..3 {
            rt.recover_once(&CancellationToken::new()).await.unwrap();
        }
    }
    let first = first.events();
    assert!(first.len() > 20);
    assert_eq!(first, second.events());
    let mut buffered = Vec::new();
    while let Some(event) = drained.try_recv() {
        buffered.push(event);
    }
    assert_eq!(first, buffered);
    assert_eq!(buffer.dropped(), 0);
    // An event survives serialization whole: an owned, self-describing record.
    let text = serde_json::to_string(&first).unwrap();
    assert_eq!(serde_json::from_str::<Vec<Event>>(&text).unwrap(), first);
}

/// A dry run and a replay are no runs of the host: its observers hear
/// nothing of them.
#[tokio::test]
async fn dry_runs_and_replays_report_nothing_to_observers() {
    let recorder = Arc::new(Recorder::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap()
    .with_observer(recorder.clone());
    let definition = doc("plain", "do: [ { a: { call: echo, with: { n: 1 } } }, { b: { call: echo } } ]");
    let mock =
        ouro_runtime::mock(|f: &str, _: &Value| f == "echo", |_: &str, a: &Value, _: &FunctionContext| Ok(a.clone()));
    let dry = rt.dry_run(&definition, json!({}), mock, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)));
    assert!(recorder.events().is_empty(), "{:#?}", recorder.events());
    save(&rt, "plain", &definition).await;
    rt.start("r", "plain", json!({})).await.unwrap();
    rt.drive("r", &CancellationToken::new()).await.unwrap();
    let heard = recorder.events().len();
    let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
    assert!(matches!(replay.status, Status::Completed(_)));
    assert_eq!(recorder.events().len(), heard);
}

/// A start pinned at a stored version is an admission like any other: the
/// observers hear `run_admitted` once, and a runtime without telemetry
/// counts nothing for it.
#[tokio::test]
async fn a_pinned_start_is_reported_as_an_admission() {
    let recorder = Arc::new(Recorder::default());
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap()
    .with_observer(recorder.clone());
    save(&rt, "plain", &doc("plain", "do: [ { a: { call: echo } } ]")).await;
    let first = rt.definitions("plain").await.unwrap().active.unwrap();
    rt.start_at("p", "plain", first, json!({})).await.unwrap();
    let admitted: Vec<Event> = recorder.events().into_iter().filter(|e| kind(e) == "run_admitted").collect();
    assert_eq!(admitted.len(), 1, "{admitted:#?}");
    assert_eq!((admitted[0].run_id.as_str(), admitted[0].root_id.as_deref()), ("p", Some("p")));
    // The same start again admits nothing new.
    rt.start_at("p", "plain", first, json!({})).await.unwrap();
    assert_eq!(recorder.events().iter().filter(|e| kind(e) == "run_admitted").count(), 1);
}

struct Vault;
impl SecretResolver for Vault {
    fn resolve(&self, name: &str) -> anyhow::Result<Option<Value>> {
        Ok((name == "api").then(|| json!({ "token": "tok-5c1d2e8f40" })))
    }
}

/// A payload larger than the host's bound is left out; secrets never reach
/// an observer.
#[tokio::test]
async fn payloads_are_bounded_and_secrets_redacted() {
    let recorder = Arc::new(Recorder::default());
    let config = Config { event_payload_bytes: 64, ..Config::default() };
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default(), Arc::default())),
        config,
        Arc::new(Time::default()),
        noop(),
    )
    .unwrap()
    .with_secrets(Arc::new(Vault))
    .with_observer(recorder.clone());
    let definition = doc(
        "bounded",
        r#"use: { secrets: [ api ] }
do:
  - small: { call: echo, with: { n: 1 } }
  - large: { call: echo, with: { text: '${ $workflow.input.text }' } }
  - secret:
      input: { from: '${ . + { auth: $secrets.api.token } }' }
      call: echo
      with: { auth: '${ .auth }' }"#,
    );
    save(&rt, "bounded", &definition).await;
    rt.start("r", "bounded", json!({ "text": "x".repeat(100) })).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    let events = recorder.events();
    let arguments = |path: &str| {
        events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::CallStarted { arguments, .. } if e.task_path.as_deref() == Some(path) => {
                    Some(arguments.clone())
                }
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(arguments("/do/0/small"), Some(json!({ "n": 1 })));
    assert_eq!(arguments("/do/1/large"), None);
    assert!(arguments("/do/2/secret").is_some());
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("tok-5c1d2e8f40"), "a secret reached an observer: {text}");
    // Without a host observer, no payload is copied for the built-in one;
    // an omitted payload is still an event.
    let by_kind: BTreeMap<String, usize> = events.iter().fold(BTreeMap::new(), |mut m, e| {
        *m.entry(kind(e)).or_default() += 1;
        m
    });
    assert_eq!(by_kind["call_started"], 3);
}
