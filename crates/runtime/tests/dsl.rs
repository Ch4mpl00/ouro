// The DSL beyond the original subset, end to end through the runtime: child
// workflows, published and emitted events, listen strategies and foreach,
// extensions, attempt deadlines, processes, secrets, external schemas and
// schedules. Durability cases restart the runtime on the same SQLite file.
use async_trait::async_trait;
use ouro_runtime::{
    Config, Delivery, EventSink, Function, FunctionContext, FunctionError, ProcessRunner, Registry, ResourceResolver,
    Runtime, SecretResolver,
    durable::{self, AtomicBackend, Clock, RunStore},
    mock,
    owf::{
        self,
        run::{Status, WorkflowError},
    },
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
impl Time {
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

fn tracer() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new()))
}

fn doc(name: &str, body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0' }}\n{body}\n")
}

/// Functions every test may call: `write` counts and echoes, and is declared
/// at most once (it must never be repeated); `read` echoes.
fn registry(writes: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<Value>>>) -> Registry {
    let mut registry = Registry::new();
    let w = writes.clone();
    let s = seen.clone();
    registry
        .register(
            Function::new("write", json!({}), json!({}), move |a: Value, _: FunctionContext| {
                w.fetch_add(1, Ordering::SeqCst);
                s.lock().unwrap().push(a.clone());
                async move { Ok(a) }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    registry
        .register(Function::new("read", json!({}), json!({}), |a: Value, _: FunctionContext| async move { Ok(a) }))
        .unwrap();
    registry
}

struct Fixture {
    rt: Runtime,
    time: Arc<Time>,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn fixture_on(
    store: Arc<dyn RunStore>,
    time: Arc<Time>,
    writes: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Value>>>,
) -> Fixture {
    let rt = Runtime::new(
        store,
        Arc::new(registry(writes.clone(), seen.clone())),
        Config::default(),
        time.clone(),
        tracer(),
    )
    .unwrap();
    Fixture { rt, time, seen }
}

fn fixture() -> Fixture {
    fixture_on(Arc::new(durable::memory::memory()), Arc::default(), Arc::default(), Arc::default())
}

async fn save(rt: &Runtime, name: &str, yaml: &str) {
    let d = rt.save(name, yaml).await.unwrap_or_else(|e| panic!("{e}"));
    rt.activate(name, d.version).await.unwrap();
}

/// Drives everything due until `id` ends or nothing is left to do.
async fn settle(rt: &Runtime, id: &str) -> Option<Status> {
    for _ in 0..50 {
        if let Some(status) = rt.get(id).await.unwrap().and_then(|r| r.outcome) {
            return Some(status);
        }
        if rt.recover_once(&CancellationToken::new()).await.unwrap() == 0 {
            break;
        }
    }
    rt.get(id).await.unwrap().and_then(|r| r.outcome)
}

fn completed(status: Option<Status>) -> Value {
    match status {
        Some(Status::Completed(v)) => v,
        other => panic!("expected completion, got {other:?}"),
    }
}

fn event(id: &str, t: &str, data: Value) -> Value {
    json!({ "id": id, "source": "/test", "type": t, "data": data })
}

// ── child workflows ────────────────────────────────────────────────────────

#[tokio::test]
async fn run_workflow_awaits_the_child_and_a_restart_never_reruns_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let (time, writes, seen) = (Arc::new(Time::default()), Arc::new(AtomicUsize::new(0)), Arc::default());
    let f = fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone(), writes.clone(), seen);
    save(
        &f.rt,
        "child",
        &doc(
            "child",
            "do:\n  - w: { call: write, with: { n: '${ .n }' } }\n  - pause: { wait: PT1S }\n  - out: { set: { doubled: '${ .n * 2 }' } }",
        ),
    ).await;
    save(
        &f.rt,
        "parent",
        &doc(
            "parent",
            "do:\n  - sub: { run: { workflow: { namespace: test, name: child, version: '1.0.0', input: { n: '${ .n }' } } } }\n  - after: { set: { got: '${ .doubled }' } }",
        ),
    ).await;
    f.rt.start("p", "parent", json!({"n": 21})).await.unwrap();
    assert!(matches!(f.rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let child = f.rt.list(Some("child"), "").await.unwrap().pop().unwrap().admission.id;
    // The child writes, then waits; the parent waits for it.
    assert!(matches!(f.rt.drive(&child, &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    drop(f);
    // A new process on the same database: the child finishes, the parent
    // takes its outcome; nothing runs twice.
    time.set(1_000);
    let f = fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time, writes.clone(), Arc::default());
    assert_eq!(completed(settle(&f.rt, "p").await), json!({"got": 42}));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.rt.list(Some("child"), "").await.unwrap().len(), 1);
    // Replaying a finished parent joins the same child.
    f.rt.start("p", "parent", json!({"n": 21})).await.unwrap();
    assert_eq!(completed(settle(&f.rt, "p").await), json!({"got": 42}));
}

#[tokio::test]
async fn a_faulted_child_is_caught_and_await_false_outlives_the_parent() {
    let f = fixture();
    save(
        &f.rt,
        "bad",
        &doc("bad", "do: [ { r: { raise: { error: { type: 'urn:t:bad', status: 418, title: Bad } } } } ]"),
    )
    .await;
    save(&f.rt, "slow", &doc("slow", "do: [ { w: { wait: PT1H } }, { done: { set: { ok: true } } } ]")).await;
    save(
        &f.rt,
        "parent",
        &doc(
            "parent",
            r#"do:
  - fire: { run: { workflow: { namespace: test, name: slow, version: latest }, await: false } }
  - guarded:
      try: [ { sub: { run: { workflow: { namespace: test, name: bad, version: '1.0.0' } } } } ]
      catch: { as: e, do: [ { keep: { set: '${ { status: $e.status, instance: $e.instance, type: $e.type } }' } } ] }
  - missing:
      try: [ { sub: { run: { workflow: { namespace: test, name: absent, version: '1.0.0' } } } } ]
      catch: { as: e, do: [ { keep: { set: '${ . + { missing: $e.status } }' } } ] }"#,
        ),
    )
    .await;
    f.rt.start("p", "parent", json!({})).await.unwrap();
    let out = completed(settle(&f.rt, "p").await);
    assert_eq!(out, json!({"status": 418, "instance": "/do/1/guarded/try/0/sub", "type": "urn:t:bad", "missing": 400}));
    let slow = f.rt.list(Some("slow"), "").await.unwrap();
    assert_eq!(slow.len(), 1);
    assert!(slow[0].ended_ms.is_none(), "an independent run is not cancelled with its parent");
}

#[tokio::test]
async fn reserved_child_functions_still_join_later() {
    let f = fixture();
    save(&f.rt, "child", &doc("child", "do: [ { r: { set: { answer: 42 } } } ]")).await;
    save(
        &f.rt,
        "parent",
        &doc(
            "parent",
            "do:\n  - start: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }",
        ),
    ).await;
    f.rt.start("p", "parent", json!({})).await.unwrap();
    assert_eq!(completed(settle(&f.rt, "p").await)[0]["output"], json!({"answer": 42}));
}

// ── events ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn emitted_events_reach_listens_and_the_sink_once() {
    struct Sink(Mutex<Vec<Value>>);
    #[async_trait]
    impl EventSink for Sink {
        async fn emit(&self, event: &Value) -> Result<(), FunctionError> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }
    let sink = Arc::new(Sink(Mutex::default()));
    let f = fixture();
    let rt = f.rt.with_event_sink(sink.clone());
    save(
        &rt,
        "listener",
        &doc(
            "listener",
            "do: [ { l: { listen: { to: { any: [ { with: { type: reading, data: '${ .v > 1 }' } } ], until: '${ length >= 2 }' } } } } ]",
        ),
    ).await;
    save(
        &rt,
        "emitter",
        &doc(
            "emitter",
            "do:\n  - loop:\n      for: { in: [ 1, 2, 3 ] }\n      do: [ { e: { emit: { event: { with: { type: reading, source: 'https://sensor', data: { v: '${ $item }' } } } } } } ]",
        ),
    ).await;
    rt.start("l", "listener", json!({})).await.unwrap();
    assert!(matches!(rt.drive("l", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    rt.start("e", "emitter", json!({})).await.unwrap();
    // A `for` outputs its last pass: the last emitted envelope.
    assert_eq!(completed(settle(&rt, "e").await)["data"], json!({"v": 3}));
    let emitted = sink.0.lock().unwrap().clone();
    for e in &emitted {
        for k in ["id", "specversion", "time", "source", "type", "data"] {
            assert!(e.get(k).is_some(), "{k} in {e}");
        }
    }
    // v = 1 is dropped by the filter; 2 and 3 complete the listen.
    assert_eq!(completed(settle(&rt, "l").await), json!([{"v": 2}, {"v": 3}]));
    assert_eq!(emitted.len(), 3);
}

#[tokio::test]
async fn all_with_correlation_waits_for_matching_events() {
    let f = fixture();
    save(
        &f.rt,
        "room",
        &doc(
            "room",
            r#"do:
  - readings:
      listen:
        to:
          all:
            - { with: { type: temperature }, correlate: { room: { from: .roomid, expect: '${ .room }' } } }
            - { with: { type: humidity }, correlate: { room: { from: .roomid } } }
        read: envelope
      output: { as: '${ map(.data) }' }"#,
        ),
    )
    .await;
    f.rt.start("r", "room", json!({"room": "kitchen"})).await.unwrap();
    f.rt.drive("r", &CancellationToken::new()).await.unwrap();
    let mut other = event("1", "temperature", json!(30));
    other["roomid"] = json!("hall");
    let mut temp = event("2", "temperature", json!(21));
    temp["roomid"] = json!("kitchen");
    let mut hum = event("3", "humidity", json!(40));
    hum["roomid"] = json!("kitchen");
    for e in [other, temp.clone(), temp, hum] {
        f.rt.publish(e).await.unwrap();
    }
    assert_eq!(completed(settle(&f.rt, "r").await), json!([21, 40]));
}

#[tokio::test]
async fn foreach_survives_a_restart_without_repeating_an_item_or_an_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let (time, writes) = (Arc::new(Time::default()), Arc::new(AtomicUsize::new(0)));
    let open = |writes: &Arc<AtomicUsize>| {
        fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone(), writes.clone(), Arc::default())
    };
    let f = open(&writes);
    save(
        &f.rt,
        "stream",
        &doc(
            "stream",
            r#"do:
  - each:
      listen: { to: { any: [ { with: { type: order } } ], until: { one: { with: { type: closed } } } } }
      foreach:
        item: order
        at: i
        do: [ { book: { call: write, with: { n: '${ $order.n }', at: '${ $i }' } } } ]
        output: { as: '${ .n }' }
        export: { as: '${ $context + { last: . } }' }
  - result: { set: '${ { items: ., last: $context.last } }' }"#,
        ),
    )
    .await;
    f.rt.start("s", "stream", json!({})).await.unwrap();
    f.rt.drive("s", &CancellationToken::new()).await.unwrap();
    f.rt.publish(event("1", "order", json!({"n": 1}))).await.unwrap();
    f.rt.drive("s", &CancellationToken::new()).await.unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    drop(f);
    let f = open(&writes);
    // The same event again is not consumed twice.
    assert!(f.rt.publish(event("1", "order", json!({"n": 1}))).await.unwrap().is_empty());
    f.rt.publish(event("2", "order", json!({"n": 2}))).await.unwrap();
    f.rt.publish(event("3", "closed", json!({}))).await.unwrap();
    assert_eq!(completed(settle(&f.rt, "s").await), json!({"items": [1, 2], "last": 2}));
    assert_eq!(writes.load(Ordering::SeqCst), 2);
}

// ── extensions and attempt deadlines ───────────────────────────────────────

#[tokio::test]
async fn extensions_wrap_matching_tasks_and_exit_bypasses_them() {
    let f = fixture();
    save(
        &f.rt,
        "ext",
        &doc(
            "ext",
            r#"use:
  extensions:
    - outer:
        extend: all
        before: [ { log: { call: write, with: { before: '${ $task.reference }' } } } ]
    - mock:
        extend: call
        when: '$task.with.mock == true'
        before: [ { fake: { set: { mocked: true }, then: exit } } ]
do:
  - real: { call: read, with: { mock: false, v: 1 } }
  - mocked: { call: write, with: { mock: true } }"#,
        ),
    )
    .await;
    f.rt.start("x", "ext", json!({})).await.unwrap();
    assert_eq!(completed(settle(&f.rt, "x").await), json!({"mocked": true}));
    let seen = f.seen.lock().unwrap().clone();
    // The logging extension ran around both tasks (and not around its own
    // task); the mocked write never reached the function.
    assert_eq!(seen, vec![json!({"before": "/do/0/real"}), json!({"before": "/do/1/mocked"})]);
}

#[tokio::test]
async fn an_attempt_deadline_is_a_retried_timeout() {
    let f = fixture();
    save(
        &f.rt,
        "attempts",
        &doc(
            "attempts",
            r#"do:
  - guarded:
      try: [ { slow: { wait: PT1H } } ]
      catch:
        as: e
        retry: { limit: { attempt: { count: 2, duration: PT1S } } }
        do: [ { r: { set: '${ { status: $e.status } }' } } ]"#,
        ),
    )
    .await;
    f.rt.start("a", "attempts", json!({})).await.unwrap();
    for t in [0, 1_000, 2_000, 3_000] {
        f.time.set(t);
        if settle(&f.rt, "a").await.is_some() {
            break;
        }
    }
    assert_eq!(completed(f.rt.get("a").await.unwrap().unwrap().outcome), json!({"status": 408}));
}

// ── processes, secrets, resources ──────────────────────────────────────────

struct Shell(AtomicUsize);
#[async_trait::async_trait]
impl ProcessRunner for Shell {
    fn runs(&self, kind: &str) -> bool {
        kind == "shell"
    }
    async fn run(&self, _: &str, definition: Value, _: FunctionContext) -> Result<Value, FunctionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "code": 3, "stdout": definition["command"], "stderr": "warn" }))
    }
}

#[tokio::test]
async fn processes_need_a_runner_and_return_what_is_asked() {
    let f = fixture();
    let body = |r: &str| {
        doc("p", &format!("do: [ {{ s: {{ run: {{ shell: {{ command: '${{ .cmd }}' }}, return: {r} }} }} }} ]"))
    };
    assert!(f.rt.save("p", &body("all")).await.unwrap_err().to_string().contains("process runner"));
    let shell = Arc::new(Shell(AtomicUsize::new(0)));
    let rt = f.rt.with_process_runner(shell.clone());
    for (r, expected) in [
        ("all", json!({"code": 3, "stdout": "ls", "stderr": "warn"})),
        ("stdout", json!("ls")),
        ("stderr", json!("warn")),
        ("code", json!(3)),
        ("none", Value::Null),
    ] {
        save(&rt, "p", &body(r)).await;
        rt.start(r, "p", json!({"cmd": "ls"})).await.unwrap();
        assert_eq!(completed(settle(&rt, r).await), expected, "{r}");
    }
    assert_eq!(shell.0.load(Ordering::SeqCst), 5);
}

struct Vault;
impl SecretResolver for Vault {
    fn resolve(&self, name: &str) -> anyhow::Result<Option<Value>> {
        Ok((name == "api").then(|| json!({ "token": "tok-9f2c51e0d7" })))
    }
}

#[tokio::test]
async fn secret_values_never_reach_records_spans_errors_or_outcomes() {
    use opentelemetry::trace::TracerProvider;
    const SECRET: &str = "tok-9f2c51e0d7";
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(durable::sqlite::SqliteBackend::open(dir.path().join("s.sqlite")).unwrap());
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider =
        opentelemetry_sdk::trace::SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let received = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let mut registry = Registry::new();
    registry
        .register(Function::new("call_api", json!({}), json!({}), move |a: Value, _: FunctionContext| {
            r.lock().unwrap().push(a.clone());
            async move {
                Err(WorkflowError::communication(401, format!("rejected header {}", a["auth"].as_str().unwrap_or(""))))
            }
        }))
        .unwrap();
    let rt = Runtime::new(
        Arc::new(durable::TransactionalStore::new(backend.clone())),
        Arc::new(registry),
        Config::default(),
        Arc::new(Time::default()),
        opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("test"))),
    )
    .unwrap()
    .with_secrets(Arc::new(Vault));
    save(
        &rt,
        "secret",
        &doc(
            "secret",
            r#"use: { secrets: [ api ] }
do:
  - call:
      try:
        - send:
            input: { from: '${ . + { auth: ("Bearer " + $secrets.api.token) } }' }
            call: call_api
            with: { auth: '${ .auth }' }
      catch: { as: e, do: [ { keep: { set: '${ { error: $e.detail, auth: .auth } }' } } ] }
  - fail: { raise: { error: { type: 'urn:t:leak', status: 500, detail: '${ .error }' } } }"#,
        ),
    )
    .await;
    rt.start("s", "secret", json!({})).await.unwrap();
    let status = rt.drive("s", &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(received.lock().unwrap()[0]["auth"], format!("Bearer {SECRET}"), "the function gets the value");
    let Status::Faulted(e) = &status else { panic!("{status:?}") };
    assert!(e.detail.as_deref().is_some_and(|d| d.contains("rejected header")), "{e:?}");
    // Every persisted row, the outcome and every span: no secret value.
    let rows = serde_json::to_string(&backend.snapshot().await.unwrap().rows.values().collect::<Vec<_>>()).unwrap();
    assert!(rows.contains("rejected header"), "the records are there");
    assert!(!rows.contains(SECRET), "a secret was persisted");
    assert!(!format!("{status:?}").contains(SECRET), "a secret is in the outcome");
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert!(!spans.is_empty());
    for span in spans {
        assert!(!format!("{:?} {:?} {:?}", span.attributes, span.status, span.events).contains(SECRET), "{span:?}");
    }
    // Validation: $secrets only in input.from, only declared names.
    let bad =
        doc("bad", "use: { secrets: [ api ] }\ndo: [ { a: { call: call_api, with: { t: '${ $secrets.api }' } } } ]");
    assert!(rt.save("bad", &bad).await.unwrap_err().to_string().contains("input.from"));
}

struct Schemas(Mutex<BTreeMap<String, Value>>);
impl ResourceResolver for Schemas {
    fn resolve(&self, uri: &str) -> anyhow::Result<Value> {
        self.0.lock().unwrap().get(uri).cloned().ok_or_else(|| anyhow::anyhow!("no {uri}"))
    }
}

#[tokio::test]
async fn external_schemas_resolve_through_the_host_and_are_pinned() {
    let f = fixture();
    let body = doc(
        "typed",
        "input: { schema: { format: json, resource: { endpoint: 'https://schemas.test/n.json' } } }\ndo: [ { a: { set: { ok: true } } } ]",
    );
    assert!(f.rt.save("typed", &body).await.unwrap_err().to_string().contains("resource resolver"));
    let schemas = Arc::new(Schemas(Mutex::new(BTreeMap::from([(
        "https://schemas.test/n.json".to_string(),
        json!({"type": "object", "required": ["n"]}),
    )]))));
    let rt = f.rt.with_resources(schemas.clone());
    save(&rt, "typed", &body).await;
    rt.start("ok", "typed", json!({"n": 1})).await.unwrap();
    rt.start("bad", "typed", json!({})).await.unwrap();
    // A schema that changes after admission does not change admitted runs.
    schemas.0.lock().unwrap().insert("https://schemas.test/n.json".into(), json!({"type": "string"}));
    assert_eq!(completed(settle(&rt, "ok").await), json!({"ok": true}));
    assert!(matches!(settle(&rt, "bad").await, Some(Status::Faulted(e)) if e.is("validation")));
}

// ── schedules ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_cron_and_after_start_each_slot_once() {
    let f = fixture();
    save(&f.rt, "every", &doc("every", "schedule: { every: PT1M }\ndo: [ { a: { set: { ok: true } } } ]")).await;
    save(&f.rt, "cron", &doc("cron", "schedule: { cron: '*/5 * * * *' }\ndo: [ { a: { set: { ok: true } } } ]")).await;
    save(&f.rt, "after", &doc("after", "schedule: { after: PT10S }\ndo: [ { a: { set: { ok: true } } } ]")).await;
    // Activation at 0 arms the first `after` run at once and the others at
    // their next slots.
    assert_eq!(f.rt.fire_due().await.unwrap(), vec!["schedule:after:after:1:0000000001".to_string()]);
    f.time.set(60_000);
    assert_eq!(f.rt.fire_due().await.unwrap(), vec!["schedule:every:every:60000".to_string()]);
    // A second scheduler at the same time starts nothing more.
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    // Down until 10 minutes: each missed schedule starts once, late. (The
    // previous `every` run ends first: an unfinished one would skip the slot.)
    f.rt.drive("schedule:every:every:60000", &CancellationToken::new()).await.unwrap();
    f.time.set(600_000);
    let mut started = f.rt.fire_due().await.unwrap();
    started.sort();
    assert_eq!(started, vec!["schedule:cron:cron:300000".to_string(), "schedule:every:every:120000".into()]);
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    // The next `after` run waits for the previous one to end, plus the delay.
    settle(&f.rt, "schedule:after:after:1:0000000001").await;
    f.time.set(609_999);
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    f.time.set(610_000);
    assert_eq!(f.rt.fire_due().await.unwrap(), vec!["schedule:after:after:1:0000000002".to_string()]);
    // A version without the schedule removes its trigger.
    save(&f.rt, "every", &doc("every", "do: [ { a: { set: { ok: true } } } ]")).await;
    f.time.set(10_000_000);
    assert!(!f.rt.fire_due().await.unwrap().iter().any(|id| id.starts_with("schedule:every:")));
}

#[tokio::test]
async fn schedule_on_starts_a_run_per_match() {
    let f = fixture();
    save(
        &f.rt,
        "alarm",
        &doc(
            "alarm",
            "schedule: { on: { one: { with: { type: heartbeat.low, data: '${ .bpm < 50 }' } } } }\ndo: [ { a: { set: { patient: '${ .[0].patient }' } } } ]",
        ).replace("version: '1.0.0' }", "version: '1.0.0', metadata: { overlap: allow } }"),
    ).await;
    let mut started = Vec::new();
    for (id, bpm, patient) in [("1", 40, "ann"), ("2", 70, "bob"), ("3", 45, "cid"), ("1", 40, "ann")] {
        started
            .extend(f.rt.publish(event(id, "heartbeat.low", json!({"bpm": bpm, "patient": patient}))).await.unwrap());
    }
    assert_eq!(started.len(), 2, "one run per match, none for a republished event: {started:?}");
    while f.rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    let mut outputs: Vec<Value> =
        f.rt.list(Some("alarm"), "")
            .await
            .unwrap()
            .into_iter()
            .map(|r| completed(r.outcome)["patient"].clone())
            .collect();
    outputs.sort_by_key(|v| v.to_string());
    assert_eq!(outputs, vec![json!("ann"), json!("cid")]);
    // A version without the schedule stops reacting.
    save(&f.rt, "alarm", &doc("alarm", "do: [ { a: { set: { ok: true } } } ]")).await;
    assert!(f.rt.publish(event("4", "heartbeat.low", json!({"bpm": 1}))).await.unwrap().is_empty());
}

// Activation epochs: a version without a schedule removes the triggers in
// its activation's commit. The breaker reports and never rolls back: a trip
// stops new runs and the schedule in its own commit, is visible in the runs
// API and is handed on as a CloudEvent (to the event sink and to an `on`
// schedule); only an activation, of the version the host picks, clears it.
#[tokio::test]
async fn a_trip_reports_and_only_an_activation_clears_it() {
    struct Sink {
        seen: Mutex<Vec<Value>>,
        failures: AtomicUsize,
    }
    #[async_trait]
    impl EventSink for Sink {
        async fn emit(&self, event: &Value) -> Result<(), FunctionError> {
            self.seen.lock().unwrap().push(event.clone());
            if self.failures.load(Ordering::SeqCst) > 0 {
                self.failures.fetch_sub(1, Ordering::SeqCst);
                return Err(FunctionError::communication(503, "sink unavailable"));
            }
            Ok(())
        }
    }
    let sink = Arc::new(Sink { seen: Mutex::default(), failures: AtomicUsize::new(1) });
    let f = fixture();
    let rt = f.rt.with_event_sink(sink.clone());
    let ok = "schedule: { every: PT1M }\ndo: [ { a: { set: { ok: true } } } ]";
    let boom = "schedule: { every: PT1M }\ndo: [ { boom: { raise: { error: { type: 'urn:t:boom', status: 500 } } } } ]";
    save(&rt, "flaky", &doc("flaky", ok)).await;
    save(&rt, "flaky", &doc("flaky", "do: [ { a: { set: { ok: true } } } ]")).await;
    assert!(rt.triggers("flaky").await.unwrap().is_empty(), "removed with the activation");
    save(&rt, "flaky", &doc("flaky", boom)).await;
    assert_eq!(rt.triggers("flaky").await.unwrap().len(), 1);
    // The host's own reaction, a workflow started once per trip.
    let watch = "schedule: { on: { one: { with: { type: ouro.definition.tripped } } } }\ndo: [ { a: { set: { tripped: '${ .[0] }' } } } ]";
    save(&rt, "watch", &doc("watch", watch)).await;
    for n in 0..3 {
        let id = format!("boom{n}");
        rt.start(&id, "flaky", json!({})).await.unwrap();
        assert!(matches!(settle(&rt, &id).await, Some(Status::Faulted(_))));
    }
    let d = rt.definitions("flaky").await.unwrap();
    assert_eq!((d.active, d.epoch), (Some(3), 3), "no rollback and no new epoch");
    let trip = d.tripped.clone().expect("visible in the runs API");
    assert_eq!((trip.version, trip.epoch, trip.faults, trip.threshold), (3, 3, 3, 3));
    assert_eq!((trip.run.as_str(), trip.reason.type_.as_str()), ("boom2", "urn:t:boom"));
    assert!(rt.triggers("flaky").await.unwrap().is_empty(), "its schedule stops");
    assert!(rt.start("refused", "flaky", json!({})).await.unwrap_err().to_string().contains("tripped"));
    f.time.set(10_000_000);
    assert!(!rt.fire_due().await.unwrap().iter().any(|id| id.contains("flaky")));
    // A failing sink leaves the notice, retried after its own backoff (1 s
    // after the first failure) with the same identity, which starts the
    // host's workflow once.
    let stop = CancellationToken::new();
    assert_eq!(rt.notify(&stop).await.unwrap(), 0);
    assert_eq!(rt.notify(&stop).await.unwrap(), 0, "backing off");
    f.time.set(10_000_999);
    assert_eq!(rt.notify(&stop).await.unwrap(), 0, "still backing off");
    assert_eq!(sink.seen.lock().unwrap().len(), 1, "not offered to the sink while backing off");
    f.time.set(10_001_000);
    assert_eq!(rt.notify(&stop).await.unwrap(), 1);
    assert_eq!(rt.notify(&stop).await.unwrap(), 0, "acknowledged");
    let seen = sink.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], trip.event("flaky"));
    assert_eq!(seen[0]["type"], durable::TRIPPED);
    let watched = rt.list(Some("watch"), "").await.unwrap();
    assert_eq!(watched.len(), 1, "one run per trip: {watched:?}");
    let id = watched[0].admission.id.clone();
    let output = completed(settle(&rt, &id).await);
    assert_eq!(output["tripped"]["run"], "boom2");
    assert_eq!(output["tripped"]["faults"], 3);
    // The host decides: here it activates the earlier version, in a new epoch.
    rt.activate("flaky", 1).await.unwrap();
    let d = rt.definitions("flaky").await.unwrap();
    assert!(d.tripped.is_none() && d.epoch == 4 && d.active == Some(1), "a new activation clears it");
    assert_eq!(rt.triggers("flaky").await.unwrap().len(), 1);
    rt.start("accepted", "flaky", json!({})).await.unwrap();
    assert_eq!(completed(settle(&rt, "accepted").await), json!({"ok": true}));
}

// A tripped definition admits no child either: `run: workflow` answers with
// a typed error, never "no workflow" and never a retry. A parent may catch
// it; uncaught, it faults the parent, which counts toward the parent's own
// breaker as an unhandled error.
#[tokio::test]
async fn children_of_a_tripped_definition_are_refused_with_a_typed_error() {
    let f = fixture();
    let rt = &f.rt;
    save(rt, "broken", &doc("broken", "do: [ { boom: { raise: { error: { type: 'urn:t:boom', status: 500 } } } } ]"))
        .await;
    for n in 0..3 {
        let id = format!("broken{n}");
        rt.start(&id, "broken", json!({})).await.unwrap();
        settle(rt, &id).await;
    }
    let epoch = rt.definitions("broken").await.unwrap().tripped.expect("tripped").epoch;
    let refused = rt.start("direct", "broken", json!({})).await.unwrap_err();
    let refused = refused.downcast_ref::<durable::DefinitionTripped>().expect("typed");
    assert_eq!((refused.name.as_str(), refused.epoch), ("broken", epoch));
    save(
        rt,
        "careful",
        &doc(
            "careful",
            r#"do:
  - owned:
      try: [ { sub: { run: { workflow: { namespace: test, name: broken, version: latest } } } } ]
      catch:
        errors: { with: { type: 'urn:ouro:errors:definition.tripped' } }
        as: e
        do: [ { keep: { set: '${ { type: $e.type, status: $e.status, detail: $e.detail } }' } } ]
  - independent:
      try: [ { fire: { run: { workflow: { namespace: test, name: broken, version: '1.0.0' }, await: false } } } ]
      catch:
        errors: { with: { type: 'urn:ouro:errors:definition.tripped' } }
        do: [ { keep: { set: '${ . + { independent: "refused" } }' } } ]"#,
        ),
    )
    .await;
    rt.start("careful", "careful", json!({})).await.unwrap();
    let out = completed(settle(rt, "careful").await);
    assert_eq!(out["type"], durable::TRIPPED_ERROR);
    assert_eq!(out["status"], 409);
    let detail = out["detail"].as_str().unwrap();
    assert!(detail.contains("broken") && detail.contains(&format!("epoch {epoch}")), "{detail}");
    assert_eq!(out["independent"], "refused");
    assert_eq!(rt.list(Some("broken"), "").await.unwrap().len(), 3, "no child was admitted");
    save(
        rt,
        "careless",
        &doc("careless", "do: [ { sub: { run: { workflow: { namespace: test, name: broken, version: latest } } } } ]"),
    )
    .await;
    rt.start("careless", "careless", json!({})).await.unwrap();
    match settle(rt, "careless").await {
        Some(Status::Faulted(e)) => assert_eq!(e.type_, durable::TRIPPED_ERROR),
        other => panic!("expected the parent to fault, got {other:?}"),
    }
    let parent = rt.definitions("careless").await.unwrap();
    assert_eq!((parent.consecutive_faults, parent.tripped.is_none()), (1, true), "an unhandled error of the parent");
    // The host's activation clears the trip; children are admitted again.
    rt.activate("broken", 1).await.unwrap();
    rt.start("careless-again", "careless", json!({})).await.unwrap();
    match settle(rt, "careless-again").await {
        Some(Status::Faulted(e)) => assert_eq!(e.type_, "urn:t:boom", "the child ran and faulted"),
        other => panic!("expected the child's own fault, got {other:?}"),
    }
}

#[tokio::test]
async fn catchup_none_skips_missed_slots_with_a_record() {
    let f = fixture();
    let body = "schedule: { every: PT1M }\ndo: [ { a: { set: { ok: true } } } ]";
    let yaml = doc("strict", body).replace("version: '1.0.0' }", "version: '1.0.0', metadata: { catchup: none } }");
    save(&f.rt, "strict", &yaml).await;
    f.time.set(60_000);
    assert_eq!(f.rt.fire_due().await.unwrap(), vec!["schedule:strict:every:60000".to_string()]);
    settle(&f.rt, "schedule:strict:every:60000").await;
    // Down until 10 minutes: the missed slot is skipped, recorded, not run.
    f.time.set(600_000);
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    let (_, trigger) = f.rt.triggers("strict").await.unwrap().pop().unwrap();
    assert_eq!(trigger.state["skipped"], json!([{ "slot": 120_000, "reason": "missed" }]));
    assert_eq!(trigger.at_ms, Some(660_000));
    f.time.set(660_000);
    assert_eq!(f.rt.fire_due().await.unwrap(), vec!["schedule:strict:every:660000".to_string()]);
    let bad = doc("bad", "do: [ { a: { set: { ok: true } } } ]")
        .replace("version: '1.0.0' }", "version: '1.0.0', metadata: { catchup: none } }");
    assert!(f.rt.save("bad", &bad).await.unwrap_err().to_string().contains("catchup"));
}

// The mailbox: an event addressed to a run waits for its listen (which reads
// the mailbox first) until its lifetime ends; then it leaves a record.
#[tokio::test]
async fn the_mailbox_keeps_addressed_events_until_a_listen_or_expiry() {
    let policy = durable::Policy { mailbox_ms: 5_000, ..durable::Policy::default() };
    let store = Arc::new(
        durable::TransactionalStore::new(Arc::new(durable::memory::MemoryBackend::default())).with_policy(policy),
    );
    let f = fixture_on(store, Arc::default(), Arc::default(), Arc::default());
    let body = "do:\n  - pause: { wait: PT1S }\n  - answer: { listen: { to: { one: { with: { type: answer } } } } }";
    save(&f.rt, "late", &doc("late", body)).await;
    save(&f.rt, "later", &doc("later", &body.replace("PT1S", "PT10S"))).await;
    let answer = |id: &str, data: Value| json!({"id": id, "source": "/test", "type": "answer", "data": data});
    for (run, name) in [("listens", "late"), ("too-late", "later")] {
        f.rt.start(run, name, json!({})).await.unwrap();
        // Before the run has run at all, let alone listened.
        assert_eq!(f.rt.deliver(answer(run, json!(42)), Some(run)).await.unwrap(), Some(run.to_string()));
    }
    assert_eq!(f.rt.deliver(answer("listens", json!(42)), Some("listens")).await.unwrap(), Some("listens".into()));
    assert!(f.rt.deliver(answer("listens", json!(0)), Some("listens")).await.is_err(), "same identity, other payload");
    for run in ["listens", "too-late"] {
        assert_eq!(settle(&f.rt, run).await, None, "pausing");
    }
    f.time.set(1_000);
    assert_eq!(completed(settle(&f.rt, "listens").await), json!([42]));
    // The other one listens only after the event's lifetime.
    f.time.set(10_000);
    assert!(f.rt.records("too-late").await.unwrap().keys().all(|k| !k.starts_with("mailbox#")));
    assert_eq!(settle(&f.rt, "too-late").await, None, "still listening");
    assert_eq!(f.rt.get("too-late").await.unwrap().unwrap().state, durable::RunState::Waiting);
    let expired = f.rt.records("too-late").await.unwrap();
    assert!(expired.iter().any(|(k, r)| k == "mailbox#expired[0]"
        && matches!(r, owf::durable::Record::Expired { event } if event["id"] == "too-late")));
    // A run that never listens again: the sweep expires it, observably.
    f.rt.start("idle", "late", json!({})).await.unwrap();
    f.rt.deliver(answer("idle", json!(1)), Some("idle")).await.unwrap();
    f.time.set(15_000);
    assert_eq!(f.rt.sweep().await.unwrap().expired, 1);
}

// ── crashes at every commit ────────────────────────────────────────────────

/// A memory backend whose `n`th commit fails, before or after it lands.
struct Crashing {
    inner: durable::memory::MemoryBackend,
    commits: AtomicUsize,
    fail_at: usize,
    after: bool,
}
#[async_trait::async_trait]
impl AtomicBackend for Crashing {
    async fn begin(&self) -> anyhow::Result<Box<dyn durable::Transaction + '_>> {
        Ok(Box::new(CrashingTx { inner: self.inner.begin().await?, backend: self }))
    }
    async fn snapshot(&self) -> anyhow::Result<durable::Snapshot> {
        self.inner.snapshot().await
    }
}
struct CrashingTx<'a> {
    inner: Box<dyn durable::Transaction + 'a>,
    backend: &'a Crashing,
}
#[async_trait::async_trait]
impl durable::Transaction for CrashingTx<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.inner.get(kind, id).await
    }
    async fn scan(
        &mut self,
        kind: &str,
        start: &str,
        end: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<durable::Rows> {
        self.inner.scan(kind, start, end, limit).await
    }
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        self.inner.put(kind, id, value).await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.inner.delete(kind, id).await
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let b = self.backend;
        let n = b.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if n == b.fail_at && !b.after {
            anyhow::bail!("injected crash before commit {n}");
        }
        self.inner.commit().await?;
        if n == b.fail_at && b.after {
            anyhow::bail!("injected crash after commit {n}");
        }
        Ok(())
    }
}

const PARENT: &str = r#"use:
  extensions:
    - audit: { extend: run, before: [ { note: { call: read, with: { at: '${ $task.reference }' } } } ] }
do:
  - child: { run: { workflow: { namespace: test, name: child, version: latest, input: { n: 1 } } } }
  - announce: { emit: { event: { with: { type: started, source: 'https://parent', data: '${ . }' } } } }
  - orders:
      listen: { to: { any: [ { with: { type: order } } ], until: '${ length >= 2 }' } }
      foreach: { item: o, do: [ { book: { call: write, with: { order: '${ $o.n }' } } } ], output: { as: '${ .order }' } }
  - done: { set: { orders: '${ . }' } }"#;

/// Runs the parent to its end with a crash at commit `fail_at`, recovering
/// like a restarted worker would; publishes two orders once it listens.
async fn crash_run(fail_at: usize, after: bool) -> (Option<Status>, Vec<Value>, usize, usize, usize) {
    let backend = Arc::new(Crashing {
        inner: durable::memory::MemoryBackend::default(),
        commits: AtomicUsize::new(0),
        fail_at,
        after,
    });
    let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
    let f = fixture_on(store.clone(), Arc::default(), Arc::default(), Arc::default());
    let setup = async |rt: &Runtime| -> anyhow::Result<()> {
        for (name, body) in
            [("child", "do: [ { w: { call: write, with: { child: '${ .n }' } } } ]"), ("parent", PARENT)]
        {
            let d = rt.save(name, &doc(name, body)).await?;
            rt.activate(name, d.version).await?;
        }
        rt.start("p", "parent", json!({})).await?;
        Ok(())
    };
    while setup(&f.rt).await.is_err() {}
    let mut published = 0;
    for step in 1..200 {
        if let Some(status) = store.get("p").await.ok().flatten().and_then(|r| r.outcome) {
            // Reads commit too: one may meet the injected crash; read again.
            let snapshot = loop {
                if let Ok(Some(s)) = store.inspect("p").await {
                    break s;
                }
            };
            let consumed = snapshot.events.iter().filter(|e| e.consumed_key.is_some() && e.type_ == "order").count();
            let children = loop {
                if let Ok(runs) = store.list(Some("child"), "").await {
                    break runs.len();
                }
            };
            let commits = backend.commits.load(Ordering::SeqCst);
            return (Some(status), f.seen.lock().unwrap().clone(), consumed, children, commits);
        }
        let _ = f.rt.recover_once(&CancellationToken::new()).await;
        let listening = store.inspect("p").await.ok().flatten().is_some_and(|s| !s.subscriptions.is_empty());
        while listening && published < 2 {
            let n = published + 1;
            if f.rt.publish(event(&n.to_string(), "order", json!({ "n": n }))).await.is_err() {
                break;
            }
            published += 1;
        }
        // Leases of a crashed worker expire; interruptions back off.
        f.time.set(step * (durable::LEASE_MS + 600_001));
    }
    (None, f.seen.lock().unwrap().clone(), 0, 0, 0)
}

#[tokio::test]
async fn crashes_at_every_commit_never_repeat_a_child_an_event_or_a_write() {
    let (status, writes, consumed, children, commits) = crash_run(usize::MAX, false).await;
    let expected = Status::Completed(json!({ "orders": [1, 2] }));
    assert_eq!(status.as_ref(), Some(&expected));
    assert_eq!(writes, vec![json!({"child": 1}), json!({"order": 1}), json!({"order": 2})]);
    assert_eq!((consumed, children), (2, 1));
    let mut completed = 0;
    for after in [false, true] {
        for fail_at in 1..=commits {
            let (status, writes, consumed, children, _) = crash_run(fail_at, after).await;
            let label = format!("crash at commit {fail_at}, after={after}: {status:?} {writes:?}");
            // An at-most-once write whose answer was lost is an unknown
            // outcome, never a second dispatch; otherwise the run completes.
            match &status {
                Some(s) if *s == expected => {
                    completed += 1;
                    assert_eq!(writes.len(), 3, "{label}");
                    assert_eq!((consumed, children), (2, 1), "{label}");
                }
                Some(Status::Faulted(e)) if e.is("outcome_unknown") => {}
                _ => panic!("{label}"),
            }
            let mut unique = writes.clone();
            unique.dedup();
            assert_eq!(unique.len(), writes.len(), "a write was sent twice: {label}");
            assert!(children <= 1, "{label}");
        }
    }
    // Most crash points recover to the same result; the rest lose a write's
    // answer and say so.
    assert!(completed * 4 > commits * 2 * 3, "{completed} of {} crash points completed", commits * 2);
}

// ── runtime expression arguments, dry runs, event-triggered `all` ──────────

#[tokio::test]
async fn runtime_arguments_have_the_shapes_the_dsl_defines() {
    let store = Arc::new(durable::memory::memory());
    let config = Config { metadata: json!({ "tenant": "t1" }), ..Config::default() };
    let rt = Runtime::new(
        store,
        Arc::new(registry(Arc::default(), Arc::default())),
        config,
        Arc::new(Time::default()),
        tracer(),
    )
    .unwrap()
    .with_secrets(Arc::new(Vault));
    save(
        &rt,
        "args",
        &doc(
            "args",
            r#"use: { secrets: [ api ] }
input: { from: '${ { raw: ., secret: ($secrets.api.token | length) } }' }
do:
  - probe:
      input: { from: '${ . + { from: $workflow.input } }' }
      set:
        runtime: '${ $runtime }'
        workflow: '${ $workflow | { id, input, definition: (.definition.document.name), startedAt } }'
        task: '${ $task | { name, reference, input, definition: (.definition | keys), startedAt } }'
        input: '${ $input }'
        context: '${ $context }'
      output: { as: '${ . + { raw: $task.output.input.from } }' }
      export: { as: '${ { output: $output.task.name } }' }
  - last: { set: '${ . + { exported: $context } }' }"#,
        ),
    )
    .await;
    rt.start("r", "args", json!({ "n": 1 })).await.unwrap();
    let out = completed(settle(&rt, "r").await);
    assert_eq!(out["runtime"]["name"], "ouro-runtime");
    assert!(out["runtime"]["version"].is_string());
    assert_eq!(out["runtime"]["metadata"], json!({ "tenant": "t1" }));
    assert_eq!(out["runtime"].as_object().unwrap().len(), 3, "no bindings leak into $runtime");
    assert_eq!(out["workflow"]["id"], "r");
    assert_eq!(out["workflow"]["input"], json!({ "n": 1 }), "$workflow.input is the raw input");
    assert_eq!(out["workflow"]["definition"], "args");
    for d in [&out["workflow"]["startedAt"], &out["task"]["startedAt"]] {
        assert!(
            d["iso8601"].is_string() && d["epoch"]["seconds"].is_i64() && d["epoch"]["milliseconds"].is_i64(),
            "{d}"
        );
    }
    assert_eq!(out["task"]["name"], "probe");
    assert_eq!(out["task"]["reference"], "/do/0/probe");
    assert_eq!(out["task"]["input"], json!({ "raw": { "n": 1 }, "secret": 14 }), "$task.input is the raw task input");
    assert_eq!(out["task"]["definition"], json!(["export", "input", "output", "set"]));
    assert_eq!(out["input"]["from"], json!({ "n": 1 }), "$input is the transformed input");
    assert_eq!(out["context"], json!({ "raw": { "n": 1 }, "secret": 14 }));
    assert_eq!(out["raw"], json!({ "n": 1 }), "$task.output is the raw output");
    assert_eq!(out["exported"], json!({ "output": "probe" }), "$output in export.as");
    let bad = doc("bad", "do: [ { a: { set: { x: '${ $authorization }' } } } ]");
    assert!(rt.save("bad", &bad).await.unwrap_err().to_string().contains("$authorization"));
}

#[tokio::test]
async fn dry_runs_record_emits_and_run_children_from_the_catalog() {
    let f = fixture();
    save(&f.rt, "child", &doc("child", "do: [ { w: { call: write, with: { from: child } } } ]")).await;
    let definition = doc(
        "parent",
        "do:\n  - sub: { run: { workflow: { namespace: test, name: child, version: latest } } }\n  - say: { emit: { event: { with: { type: said, source: 'https://p', data: '${ . }' } } } }",
    );
    let dry =
        f.rt.dry_run(
            &definition,
            json!({}),
            mock(
                |function: &str, _: &Value| function == "write",
                |_: &str, a: &Value, _: &FunctionContext| Ok(a.clone()),
            ),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    let functions: Vec<&str> = dry.mocked.iter().map(|w| w.function.as_str()).collect();
    assert_eq!(functions, ["write", "emit"]);
    assert_eq!(f.seen.lock().unwrap().len(), 0, "nothing reached the real write");
    assert!(f.rt.list(None, "").await.unwrap().is_empty(), "nothing was admitted for real");
}

// The host's mock rule covers process runs too: a dry run dispatches no
// `run:<kind>` the rule marks, and runs the others.
#[tokio::test]
async fn dry_runs_dispatch_no_process_the_rule_marks() {
    let shell = Arc::new(Shell(AtomicUsize::new(0)));
    let rt = fixture().rt.with_process_runner(shell.clone());
    let definition = doc("p", "do: [ { s: { run: { shell: { command: '${ .cmd }' }, return: all } } } ]");
    let marked = |cmd: &'static str| {
        mock(
            move |function: &str, a: &Value| function == "run:shell" && a["command"] == cmd,
            |_: &str, _: &Value, _: &FunctionContext| Ok(json!({ "code": 0, "stdout": "mocked", "stderr": "" })),
        )
    };
    let stop = CancellationToken::new();
    let dry = rt.dry_run(&definition, json!({"cmd": "rm"}), marked("rm"), &stop).await.unwrap();
    assert_eq!(completed(Some(dry.status)), json!({ "code": 0, "stdout": "mocked", "stderr": "" }));
    assert_eq!(dry.mocked.len(), 1);
    assert_eq!(shell.0.load(Ordering::SeqCst), 0, "a marked process never ran");
    let dry = rt.dry_run(&definition, json!({"cmd": "ls"}), marked("rm"), &stop).await.unwrap();
    assert_eq!(completed(Some(dry.status)), json!({ "code": 3, "stdout": "ls", "stderr": "warn" }));
    assert!(dry.mocked.is_empty());
    assert_eq!(shell.0.load(Ordering::SeqCst), 1, "an unmarked process ran for real");
}

#[tokio::test]
async fn schedule_on_all_starts_once_every_filter_has_matched() {
    let f = fixture();
    save(
        &f.rt,
        "pair",
        &doc(
            "pair",
            "schedule:\n  on: { all: [ { with: { type: a } }, { with: { type: b } } ] }\n  read: envelope\ndo: [ { s: { set: '${ map(.type) }' } } ]",
        ),
    ).await;
    assert!(f.rt.publish(event("1", "a", json!(1))).await.unwrap().is_empty(), "one of two is not enough");
    assert!(f.rt.publish(event("2", "a", json!(2))).await.unwrap().is_empty(), "a filled filter drops a second match");
    let started = f.rt.publish(event("3", "b", json!(3))).await.unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(completed(settle(&f.rt, &started[0]).await), json!(["a", "b"]));
    assert!(f.rt.publish(event("4", "b", json!(4))).await.unwrap().is_empty(), "the next pair starts afresh");
}

// ── the jump budget (issue #8) ─────────────────────────────────────────────

/// Three jumps, then the budget ends the run with the jumping task's output.
const BUDGETED: &str = r#"do:
  - tick: { call: read, with: { n: '${ .n + 1 }' } }
  - again:
      switch:
        - stop: { when: '${ .n < 0 }', then: exit }
        - more: { then: tick }"#;

fn budgeted() -> String {
    format!(
        "document: {{ dsl: '1.0.4', namespace: test, name: loop, version: '1.0.0', metadata: {{ limits: {{ max_jumps: 3, on_exceeded: end }} }} }}\n{BUDGETED}\n"
    )
}

fn decisions(records: &BTreeMap<String, owf::durable::Record>) -> Vec<(String, bool, u64)> {
    records
        .iter()
        .filter_map(|(k, r)| match r {
            owf::durable::Record::Budget { granted, spent, .. } => Some((k.clone(), *granted, *spent)),
            _ => None,
        })
        .collect()
}

/// The budgeted loop to its end with a crash at commit `fail_at`.
async fn budget_crash(fail_at: usize, after: bool) -> (Option<Status>, Vec<(String, bool, u64)>, usize) {
    let backend = Arc::new(Crashing {
        inner: durable::memory::MemoryBackend::default(),
        commits: AtomicUsize::new(0),
        fail_at,
        after,
    });
    let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
    let f = fixture_on(store.clone(), Arc::default(), Arc::default(), Arc::default());
    let setup = async |rt: &Runtime| -> anyhow::Result<()> {
        let d = rt.save("loop", &budgeted()).await?;
        rt.activate("loop", d.version).await?;
        rt.start("l", "loop", json!({ "n": 0 })).await?;
        Ok(())
    };
    while setup(&f.rt).await.is_err() {}
    for step in 1..200 {
        if let Some(status) = store.get("l").await.ok().flatten().and_then(|r| r.outcome) {
            let records = loop {
                if let Ok(Some(s)) = store.inspect("l").await {
                    break s.records;
                }
            };
            return (Some(status), decisions(&records), backend.commits.load(Ordering::SeqCst));
        }
        let _ = f.rt.recover_once(&CancellationToken::new()).await;
        f.time.set(step * (durable::LEASE_MS + 600_001));
    }
    (None, Vec::new(), 0)
}

#[tokio::test]
async fn the_jump_count_survives_crashes_at_every_commit() {
    let (status, baseline, commits) = budget_crash(usize::MAX, false).await;
    let expected = Status::Completed(json!({ "n": 4 }));
    assert_eq!(status.as_ref(), Some(&expected));
    let keys = ["/do/1/again#jump", "/do/1/again[1]#jump", "/do/1/again[2]#jump", "/do/1/again[3]#jump"];
    assert_eq!(
        baseline,
        keys.iter()
            .zip([(true, 1), (true, 2), (true, 3), (false, 3)])
            .map(|(k, (g, s))| (k.to_string(), g, s))
            .collect::<Vec<_>>()
    );
    for after in [false, true] {
        for fail_at in 1..=commits {
            let (status, recovered, _) = budget_crash(fail_at, after).await;
            // A restart neither resets the count nor spends a unit twice.
            assert_eq!(status.as_ref(), Some(&expected), "crash at commit {fail_at}, after={after}");
            assert_eq!(recovered, baseline, "crash at commit {fail_at}, after={after}");
        }
    }
}

#[tokio::test]
async fn a_restarted_process_resumes_the_count_and_replay_repeats_the_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let f = fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone(), Arc::default(), Arc::default());
    // The loop pauses after every jump, so a process can stop between them.
    let paused = budgeted().replace("- tick: { call: read", "- pause: { wait: PT1S }\n  - tick: { call: read");
    let paused = paused.replace("then: tick", "then: pause");
    save(&f.rt, "loop", &paused).await;
    f.rt.start("l", "loop", json!({ "n": 0 })).await.unwrap();
    for at in [0, 1_000] {
        time.set(at);
        assert!(matches!(f.rt.drive("l", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    }
    assert_eq!(decisions(&f.rt.records("l").await.unwrap()).len(), 1);
    drop(f);
    let f = fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone(), Arc::default(), Arc::default());
    for at in 2..6 {
        time.set(at * 1_000);
        f.rt.drive("l", &CancellationToken::new()).await.unwrap();
    }
    let status = f.rt.get("l").await.unwrap().and_then(|r| r.outcome);
    assert_eq!(status, Some(Status::Completed(json!({ "n": 4 }))));
    let made = decisions(&f.rt.records("l").await.unwrap());
    assert_eq!(
        made.iter().map(|(_, g, s)| (*g, *s)).collect::<Vec<_>>(),
        [(true, 1), (true, 2), (true, 3), (false, 3)]
    );
    // Replay restores every decision, the refusal included.
    let replay = f.rt.replay("l", &CancellationToken::new()).await.unwrap();
    assert_eq!(replay.status, Status::Completed(json!({ "n": 4 })));
}

#[tokio::test]
async fn children_spend_their_roots_budget_and_nesting_is_capped() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let mut config = Config::default();
    config.limits.max_jumps = 6;
    config.limits.max_depth = 3;
    let rt = Runtime::new(
        store.clone(),
        Arc::new(registry(Arc::default(), Arc::default())),
        config,
        Arc::new(Time::default()),
        tracer(),
    )
    .unwrap();
    // A child jumps twice; the parent starts a fresh child on every pass.
    let child = "do:\n  - step: { set: { k: '${ (.k // 0) + 1 }' } }\n  - again: { switch: [ { stop: { when: '${ .k >= 3 }', then: exit } }, { more: { then: step } } ] }";
    save(&rt, "child", &doc("child", child)).await;
    let parent = "do:\n  - spawn: { run: { workflow: { namespace: test, name: child, version: latest, input: {} } }, output: { as: '${ $input }' } }\n  - count: { set: { i: '${ .i + 1 }' } }\n  - loop: { switch: [ { stop: { when: '${ .i >= 100 }', then: exit } }, { more: { then: spawn } } ] }";
    save(&rt, "parent", &doc("parent", parent)).await;
    rt.start("p", "parent", json!({ "i": 0 })).await.unwrap();
    let Some(Status::Faulted(e)) = settle(&rt, "p").await else { panic!("the tree did not run out of budget") };
    // Two passes spend 2 + 1 jumps each; the third child is refused.
    assert!(e.is("budget.exhausted"), "{e:?}");
    assert_eq!(e.instance.as_deref(), Some("/do/0/spawn[2]"));
    let mut granted = 0;
    let runs = rt.list(None, "").await.unwrap();
    for r in &runs {
        let made = decisions(&rt.records(&r.admission.id).await.unwrap());
        granted += made.iter().filter(|(_, g, _)| *g).count();
        assert!(made.iter().all(|(_, _, spent)| *spent <= 6));
        if r.admission.name == "child" {
            assert_eq!(r.root.as_deref(), Some("p"));
            assert_eq!(r.admission.runtime["limits"], json!({ "max_jumps": 6, "depth": 1 }));
        }
    }
    assert_eq!((granted, runs.len()), (6, 4));
    // Replayed alone, the refused child is refused again: replay restores the
    // tree's decision, not a count of its own.
    let refused = runs.iter().find(|r| r.admission.name == "child" && matches!(r.outcome, Some(Status::Faulted(_))));
    let replay = rt.replay(&refused.unwrap().admission.id, &CancellationToken::new()).await.unwrap();
    assert!(matches!(&replay.status, Status::Faulted(e) if e.is("budget.exhausted")), "{:?}", replay.status);
    // Self-recursion through `run: workflow` stops at the nesting depth.
    save(
        &rt,
        "rec",
        &doc("rec", "do:\n  - deeper: { run: { workflow: { namespace: test, name: rec, version: latest } } }"),
    )
    .await;
    rt.start("r", "rec", json!({})).await.unwrap();
    let Some(Status::Faulted(e)) = settle(&rt, "r").await else { panic!("recursion did not stop") };
    assert!(e.is("budget.exhausted"), "{e:?}");
    assert_eq!(rt.list(Some("rec"), "").await.unwrap().len(), 4, "the root and three nested children");
}

#[tokio::test]
async fn definitions_only_tighten_the_budget_and_cannot_loop_without_a_way_out() {
    let f = fixture();
    let limited = |n: u64| {
        format!(
            "document: {{ dsl: '1.0.4', namespace: test, name: l, version: '1.0.0', metadata: {{ limits: {{ max_jumps: {n} }} }} }}\n{BUDGETED}\n"
        )
    };
    assert!(f.rt.validate(&limited(100)).is_ok());
    let e = f.rt.save("l", &limited(101)).await.unwrap_err().to_string();
    assert!(e.contains("ceiling of 100"), "{e}");
    let e =
        f.rt.save("c", &doc("c", "do:\n  - a: { call: read }\n  - b: { call: read, then: a }"))
            .await
            .unwrap_err()
            .to_string();
    assert!(e.contains("jump in a cycle"), "{e}");
}

#[tokio::test]
async fn a_run_ended_by_its_budget_says_so_on_its_span() {
    use opentelemetry::trace::TracerProvider;
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider =
        opentelemetry_sdk::trace::SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry(Arc::default(), Arc::default())),
        Config::default(),
        Arc::new(Time::default()),
        opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("budget"))),
    )
    .unwrap();
    save(&rt, "loop", &budgeted()).await;
    save(&rt, "plain", &doc("plain", "do: [ { a: { set: { x: 1 } } } ]")).await;
    for (id, name) in [("l", "loop"), ("p", "plain")] {
        rt.start(id, name, json!({ "n": 0 })).await.unwrap();
        settle(&rt, id).await;
    }
    let flagged = |definition: &str| {
        exporter.get_finished_spans().unwrap().iter().any(|s| {
            s.name == format!("workflow {definition}")
                && s.attributes.iter().any(|kv| kv.key.as_str() == "ouro.limit_exceeded" && kv.value.as_str() == "true")
        })
    };
    assert!(flagged("loop"));
    assert!(!flagged("plain"));
}

/// A memory backend whose `n`th commit fails (before or after it lands),
/// noting the commit that saved a run's first refusal.
struct Claiming {
    inner: durable::memory::MemoryBackend,
    commits: AtomicUsize,
    claimed: AtomicUsize,
    fail_at: usize,
    after: bool,
}
#[async_trait::async_trait]
impl AtomicBackend for Claiming {
    async fn begin(&self) -> anyhow::Result<Box<dyn durable::Transaction + '_>> {
        Ok(Box::new(ClaimingTx { inner: self.inner.begin().await?, backend: self, claims: false }))
    }
    async fn snapshot(&self) -> anyhow::Result<durable::Snapshot> {
        self.inner.snapshot().await
    }
}
struct ClaimingTx<'a> {
    inner: Box<dyn durable::Transaction + 'a>,
    backend: &'a Claiming,
    claims: bool,
}
#[async_trait::async_trait]
impl durable::Transaction for ClaimingTx<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.inner.get(kind, id).await
    }
    async fn scan(
        &mut self,
        kind: &str,
        start: &str,
        end: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<durable::Rows> {
        self.inner.scan(kind, start, end, limit).await
    }
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        self.claims |= kind.starts_with("record:") && id == owf::durable::EXCEEDED;
        self.inner.put(kind, id, value).await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.inner.delete(kind, id).await
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let b = self.backend;
        let n = b.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if n == b.fail_at && !b.after {
            anyhow::bail!("injected crash before commit {n}");
        }
        self.inner.commit().await?;
        if self.claims {
            b.claimed.store(n, Ordering::SeqCst);
        }
        if n == b.fail_at && b.after {
            anyhow::bail!("injected crash after commit {n}");
        }
        Ok(())
    }
}

/// Two fork branches loop on one budget; `tick` yields, so they take turns
/// and both are refused. The handler reports which branch acted.
const RACING: &str = r#"document:
  dsl: '1.0.4'
  namespace: test
  name: race
  version: '1.0.0'
  metadata: { limits: { max_jumps: 3, on_exceeded: { call: report, with: { by: '${ .b }' } } } }
do:
  - both:
      fork:
        branches:
          - a:
              do:
                - tick: { call: tick, with: { b: a } }
                - again: { switch: [ { stop: { when: '${ .b == "none" }', then: exit } }, { more: { then: tick } } ] }
          - b:
              do:
                - tick: { call: tick, with: { b: b } }
                - again: { switch: [ { stop: { when: '${ .b == "none" }', then: exit } }, { more: { then: tick } } ] }
"#;

/// The racing run to its end with a crash at commit `fail_at`: its status,
/// the task paths `report` was called at, its refusals, the commits made and
/// the commit that saved the first refusal.
async fn race_crash(fail_at: usize, after: bool) -> (Option<Status>, Vec<String>, usize, usize, usize) {
    let backend = Arc::new(Claiming {
        inner: durable::memory::MemoryBackend::default(),
        commits: AtomicUsize::new(0),
        claimed: AtomicUsize::new(0),
        fail_at,
        after,
    });
    let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
    let reports = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry
        .register(Function::new("tick", json!({}), json!({}), |a: Value, _: FunctionContext| async move {
            tokio::task::yield_now().await;
            Ok(a)
        }))
        .unwrap();
    let seen = reports.clone();
    registry
        .register(Function::new("report", json!({}), json!({}), move |a: Value, c: FunctionContext| {
            seen.lock().unwrap().push(c.task_path.clone());
            async move {
                // Long enough for the other branch to be refused meanwhile.
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                Ok(json!({ "reported": a["by"] }))
            }
        }))
        .unwrap();
    let time = Arc::new(Time::default());
    let rt = Runtime::new(store.clone(), Arc::new(registry), Config::default(), time.clone(), tracer()).unwrap();
    let setup = async |rt: &Runtime| -> anyhow::Result<()> {
        let d = rt.save("race", RACING).await?;
        rt.activate("race", d.version).await?;
        rt.start("r", "race", json!({})).await?;
        Ok(())
    };
    while setup(&rt).await.is_err() {}
    for step in 1..200 {
        if let Some(status) = store.get("r").await.ok().flatten().and_then(|r| r.outcome) {
            let records = loop {
                if let Ok(Some(s)) = store.inspect("r").await {
                    break s.records;
                }
            };
            let refused = decisions(&records).iter().filter(|(_, granted, _)| !granted).count();
            let reports = reports.lock().unwrap().clone();
            if fail_at == usize::MAX {
                // Replay takes no turns (recorded calls answer at once), yet
                // the recorded first refusal acts again.
                let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
                assert_eq!(Some(&replay.status), Some(&status));
            }
            return (
                Some(status),
                reports,
                refused,
                backend.commits.load(Ordering::SeqCst),
                backend.claimed.load(Ordering::SeqCst),
            );
        }
        let _ = rt.recover_once(&CancellationToken::new()).await;
        time.set(step * (durable::LEASE_MS + 600_001));
    }
    (None, Vec::new(), 0, 0, 0)
}

#[tokio::test]
async fn branches_refused_together_follow_the_first_refusal_across_crashes() {
    let (status, baseline_reports, refused, commits, claimed) = race_crash(usize::MAX, false).await;
    let reports = baseline_reports;
    let Some(Status::Completed(baseline)) = status else { panic!("{status:?}") };
    // Both branches were refused; one handler ran, for the first refusal.
    assert_eq!(refused, 2, "both branches were refused");
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(claimed > 0 && claimed < commits);
    let either = [json!({ "reported": "a" }), json!({ "reported": "b" })];
    assert!(either.contains(&baseline), "{baseline}");
    let mut decided = 0;
    for after in [false, true] {
        for fail_at in 1..=commits {
            let (status, calls, _, _, _) = race_crash(fail_at, after).await;
            let label = format!("crash at commit {fail_at}, after={after}: {status:?} {calls:?}");
            let Some(Status::Completed(output)) = status else { panic!("{label}") };
            // One handler: every call (a lost answer resends the same call)
            // has one identity.
            let mut paths = calls.clone();
            paths.dedup();
            assert_eq!(paths.len(), 1, "{label}");
            assert!(either.contains(&output), "{label}");
            // Once the first refusal is saved, recovery follows it: the same
            // branch acts and the run ends with the same output.
            if fail_at > claimed || (fail_at == claimed && after) {
                decided += 1;
                assert_eq!(output, baseline, "{label}");
                assert_eq!(paths, reports, "{label}");
            }
        }
    }
    assert!(decided > 2, "crash points after the first refusal: {decided}");
}

#[tokio::test]
async fn schedule_overlap_policies_apply_to_time_and_event_slots() {
    for policy in ["skip", "allow", "queue", "replace"] {
        for kind in ["every", "cron", "on"] {
            let f = fixture();
            let schedule = match kind {
                "every" => "every: PT1M",
                "cron" => "cron: '* * * * *'",
                _ => "on: { one: { with: { type: tick } } }",
            };
            let yaml = doc("overlap", &format!("schedule: {{ {schedule} }}\ndo: [ {{ pause: {{ wait: PT1H }} }} ]"))
                .replace("version: '1.0.0' }", &format!("version: '1.0.0', metadata: {{ overlap: {policy} }} }}"));
            save(&f.rt, "overlap", &yaml).await;
            let fire = async |n: i64| {
                f.time.set(n * 60_000);
                if kind == "on" {
                    f.rt.publish(json!({"source":"test", "id":format!("{n}"), "type":"tick", "data":n})).await.unwrap()
                } else {
                    f.rt.fire_due().await.unwrap()
                }
            };
            let first = fire(1).await.pop().unwrap();
            assert_eq!(settle(&f.rt, &first).await, None, "waiting counts as overlap");
            let second = fire(2).await;
            let third = fire(3).await;
            match policy {
                "skip" => {
                    assert!(second.is_empty() && third.is_empty());
                    assert_eq!(f.rt.list(Some("overlap"), "").await.unwrap().len(), 1);
                }
                "allow" => {
                    assert_eq!((second.len(), third.len()), (1, 1));
                    assert!(f.rt.get(&first).await.unwrap().unwrap().ended_ms.is_none());
                }
                "queue" => {
                    assert!(second.is_empty() && third.is_empty());
                    assert_eq!(f.rt.queued("overlap").await.unwrap().len(), 1);
                    f.rt.cancel(&first).await.unwrap();
                    assert!(f.rt.queued("overlap").await.unwrap().is_empty());
                    let runs = f.rt.list(Some("overlap"), "").await.unwrap();
                    assert_eq!(runs.len(), 2);
                    let promoted = runs.iter().find(|r| r.admission.id != first).unwrap();
                    assert_eq!(promoted.admitted_ms, 180_000);
                    if kind == "on" {
                        assert_eq!(promoted.admission.input, json!([2]));
                    } else {
                        assert!(promoted.admission.id.ends_with(":120000"));
                    }
                }
                _ => {
                    assert_eq!((second.len(), third.len()), (1, 1));
                    assert_eq!(f.rt.get(&first).await.unwrap().unwrap().outcome, Some(Status::Cancelled));
                    assert_eq!(f.rt.get(&second[0]).await.unwrap().unwrap().outcome, Some(Status::Cancelled));
                }
            }
            // Reoffering the same event or ticking the same slot never admits
            // a second run, even after a policy's skip or replacement.
            let count = f.rt.list(Some("overlap"), "").await.unwrap().len();
            fire(3).await;
            assert_eq!(f.rt.list(Some("overlap"), "").await.unwrap().len(), count);
            let (_, t) = f.rt.triggers("overlap").await.unwrap().pop().unwrap();
            if policy == "skip" || policy == "queue" {
                assert!(t.state["skips"].as_u64().unwrap() >= 1);
            }
        }
    }
}

#[tokio::test]
async fn schedule_allow_stops_at_the_host_ceiling_across_trigger_kinds() {
    let f = fixture();
    let yaml = doc(
        "bounded",
        "schedule: { every: PT1M, on: { one: { with: { type: tick } } } }\ndo: [ { pause: { wait: PT1H } } ]",
    )
    .replace("version: '1.0.0' }", "version: '1.0.0', metadata: { overlap: allow } }");
    save(&f.rt, "bounded", &yaml).await;
    for n in 1..=10 {
        f.time.set(n * 60_000);
        assert_eq!(f.rt.fire_due().await.unwrap().len(), 1);
    }
    f.time.set(660_000);
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    assert!(f.rt.publish(json!({"source":"test", "id":"ceiling", "type":"tick"})).await.unwrap().is_empty());
    assert_eq!(f.rt.list(Some("bounded"), "").await.unwrap().len(), 10);
    for (_, t) in f.rt.triggers("bounded").await.unwrap() {
        assert_eq!(t.state["skipped"][0]["overlap"], "ceiling");
    }
    f.rt.cancel("schedule:bounded:every:60000").await.unwrap();
    assert_eq!(f.rt.publish(json!({"source":"test", "id":"free", "type":"tick"})).await.unwrap().len(), 1);
}

#[tokio::test]
async fn after_overlap_and_ceiling_rearm_without_stranding_the_trigger() {
    for policy in ["skip", "allow", "queue", "replace"] {
        let f = fixture();
        let yaml = doc("after-overlap", "schedule: { after: PT1M }\ndo: [ { pause: { wait: PT1H } } ]")
            .replace("version: '1.0.0' }", &format!("version: '1.0.0', metadata: {{ overlap: {policy} }} }}"));
        save(&f.rt, "after-overlap", &yaml).await;
        let first = f.rt.fire_due().await.unwrap().pop().unwrap();
        settle(&f.rt, &first).await;
        // Reactivation arms a fresh after trigger while the older run lives.
        f.rt.activate("after-overlap", 1).await.unwrap();
        let second = f.rt.fire_due().await.unwrap();
        match policy {
            "skip" => {
                assert!(second.is_empty());
                assert_eq!(f.rt.triggers("after-overlap").await.unwrap()[0].1.at_ms, Some(60_000));
            }
            "queue" => {
                assert!(second.is_empty());
                let queued = f.rt.queued("after-overlap").await.unwrap().pop().unwrap();
                f.rt.cancel(&first).await.unwrap();
                assert!(f.rt.get(&queued.id).await.unwrap().is_some());
                f.rt.cancel(&queued.id).await.unwrap();
                assert_eq!(f.rt.triggers("after-overlap").await.unwrap()[0].1.at_ms, Some(60_000));
            }
            _ => assert_eq!(second.len(), 1),
        }
    }
    let f = fixture();
    let yaml = doc("after-ceiling", "schedule: { after: PT1M }\ndo: [ { pause: { wait: PT1H } } ]");
    save(&f.rt, "after-ceiling", &yaml).await;
    for n in 0..10 {
        f.rt.start(&format!("manual{n}"), "after-ceiling", json!({})).await.unwrap();
    }
    assert!(f.rt.fire_due().await.unwrap().is_empty());
    assert_eq!(f.rt.triggers("after-ceiling").await.unwrap()[0].1.at_ms, Some(60_000));
    f.rt.cancel("manual0").await.unwrap();
    f.time.set(60_000);
    assert_eq!(f.rt.fire_due().await.unwrap().len(), 1);
}

#[tokio::test]
async fn replacing_a_slot_signals_its_running_handler() {
    let time = Arc::new(Time::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    registry
        .register(Function::new("block", json!({}), json!({}), {
            let entered = entered.clone();
            let cancelled = cancelled.clone();
            move |_: Value, ctx: FunctionContext| {
                let entered = entered.clone();
                let cancelled = cancelled.clone();
                async move {
                    entered.notify_one();
                    ctx.cancel.cancelled().await;
                    cancelled.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({}))
                }
            }
        }))
        .unwrap();
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry),
        Config::default(),
        time.clone(),
        tracer(),
    )
    .unwrap();
    let yaml = doc("replace-handler", "schedule: { every: PT1M }\ndo: [ { block: { call: block } } ]")
        .replace("version: '1.0.0' }", "version: '1.0.0', metadata: { overlap: replace } }");
    save(&rt, "replace-handler", &yaml).await;
    time.set(60_000);
    let first = rt.fire_due().await.unwrap().pop().unwrap();
    let stop = CancellationToken::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(rt.drive(&first, &stop), async {
            entered.notified().await;
            time.set(120_000);
            assert_eq!(rt.fire_due().await.unwrap().len(), 1);
        })
    })
    .await
    .unwrap()
    .0
    .ok();
    assert_eq!(rt.get(&first).await.unwrap().unwrap().outcome, Some(Status::Cancelled));
    // The interpreter may drop an in-flight handler after signaling it; the
    // durable cancellation and prompt return are the required observations.
}

#[tokio::test]
async fn schedule_ceiling_is_supplied_by_the_host() {
    let time = Arc::new(Time::default());
    let config = Config { max_concurrent_runs: 1, ..Config::default() };
    let rt =
        Runtime::new(Arc::new(durable::memory::memory()), Arc::new(Registry::new()), config, time.clone(), tracer())
            .unwrap();
    let yaml = doc("host-ceiling", "schedule: { every: PT1M }\ndo: [ { pause: { wait: PT1H } } ]")
        .replace("version: '1.0.0' }", "version: '1.0.0', metadata: { overlap: allow } }");
    save(&rt, "host-ceiling", &yaml).await;
    time.set(60_000);
    assert_eq!(rt.fire_due().await.unwrap().len(), 1);
    time.set(120_000);
    assert!(rt.fire_due().await.unwrap().is_empty());
    assert_eq!(rt.triggers("host-ceiling").await.unwrap()[0].1.state["skipped"][0]["overlap"], "ceiling");
    assert!(
        Runtime::new(
            Arc::new(durable::memory::memory()),
            Arc::new(Registry::new()),
            Config { max_concurrent_runs: 0, ..Config::default() },
            time,
            tracer()
        )
        .is_err()
    );
}

#[tokio::test]
async fn refused_slots_follow_the_selected_overlap_policy() {
    for policy in ["skip", "allow", "queue", "replace"] {
        let f = fixture();
        let yaml = doc(
            "refused-overlap",
            "schedule: { every: PT1M }\ndo: [ { pause: { wait: PT1H } }, { write: { call: write } } ]",
        )
        .replace("version: '1.0.0' }", &format!("version: '1.0.0', metadata: {{ overlap: {policy} }} }}"));
        save(&f.rt, "refused-overlap", &yaml).await;
        f.time.set(60_000);
        let first = f.rt.fire_due().await.unwrap().pop().unwrap();
        assert_eq!(settle(&f.rt, &first).await, None);
        f.rt.set_registry(Arc::new(Registry::new()));
        f.time.set(120_000);
        assert!(f.rt.fire_due().await.unwrap().is_empty());
        let second = "schedule:refused-overlap:every:120000";
        if policy == "skip" {
            assert!(f.rt.get(second).await.unwrap().is_none());
            assert_eq!(f.rt.definitions("refused-overlap").await.unwrap().consecutive_faults, 0);
            continue;
        }
        if policy == "queue" {
            assert_eq!(f.rt.queued("refused-overlap").await.unwrap()[0].id, second);
            assert!(f.rt.get(second).await.unwrap().is_none());
            f.rt.cancel(&first).await.unwrap();
            assert!(f.rt.queued("refused-overlap").await.unwrap().is_empty());
        }
        assert!(matches!(f.rt.get(second).await.unwrap().unwrap().outcome, Some(Status::Faulted(_))));
        assert_eq!(f.rt.definitions("refused-overlap").await.unwrap().consecutive_faults, 1);
        if policy == "replace" {
            assert_eq!(f.rt.get(&first).await.unwrap().unwrap().outcome, Some(Status::Cancelled));
        }
    }
}
