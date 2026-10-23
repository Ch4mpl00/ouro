// The DSL beyond the original subset, end to end through the runtime: child
// workflows, published and emitted events, listen strategies and foreach,
// extensions, attempt deadlines, processes, secrets, external schemas and
// schedules. Durability cases restart the runtime on the same SQLite file.
use async_trait::async_trait;
use ouro_runtime::{
    Config, Effect, EventSink, Function, FunctionContext, FunctionError, ProcessRunner, Registry, ResourceResolver,
    Runtime, SecretResolver,
    durable::{self, AtomicBackend, Clock, RunStore},
    owf::run::{Status, WorkflowError},
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

/// Functions every test may call: `write` counts and echoes, `read` echoes.
fn registry(writes: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<Value>>>) -> Registry {
    let mut registry = Registry::new();
    let w = writes.clone();
    let s = seen.clone();
    registry
        .register(Function::new("write", json!({}), json!({}), Effect::Write, move |a: Value, _: FunctionContext| {
            w.fetch_add(1, Ordering::SeqCst);
            s.lock().unwrap().push(a.clone());
            async move { Ok(a) }
        }))
        .unwrap();
    registry
        .register(Function::new(
            "read",
            json!({}),
            json!({}),
            Effect::Read,
            |a: Value, _: FunctionContext| async move { Ok(a) },
        ))
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

fn save(rt: &Runtime, name: &str, yaml: &str) {
    let d = rt.save(name, yaml).unwrap_or_else(|e| panic!("{e}"));
    rt.activate(name, d.version).unwrap();
}

/// Drives everything due until `id` ends or nothing is left to do.
async fn settle(rt: &Runtime, id: &str) -> Option<Status> {
    for _ in 0..50 {
        if let Some(status) = rt.get(id).unwrap().and_then(|r| r.outcome) {
            return Some(status);
        }
        if rt.recover_once(&CancellationToken::new()).await.unwrap() == 0 {
            break;
        }
    }
    rt.get(id).unwrap().and_then(|r| r.outcome)
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
    );
    save(
        &f.rt,
        "parent",
        &doc(
            "parent",
            "do:\n  - sub: { run: { workflow: { namespace: test, name: child, version: '1.0.0', input: { n: '${ .n }' } } } }\n  - after: { set: { got: '${ .doubled }' } }",
        ),
    );
    f.rt.start("p", "parent", json!({"n": 21})).unwrap();
    assert!(matches!(f.rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let child = f.rt.list(Some("child"), "").unwrap().pop().unwrap().admission.id;
    // The child writes, then waits; the parent waits for it.
    assert!(matches!(f.rt.drive(&child, &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    drop(f);
    // A new process on the same database: the child finishes, the parent
    // takes its outcome; nothing runs twice.
    time.set(1_000);
    let f = fixture_on(Arc::new(durable::sqlite::open(&path).unwrap()), time, writes.clone(), Arc::default());
    assert_eq!(completed(settle(&f.rt, "p").await), json!({"got": 42}));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.rt.list(Some("child"), "").unwrap().len(), 1);
    // Replaying a finished parent joins the same child.
    f.rt.start("p", "parent", json!({"n": 21})).unwrap();
    assert_eq!(completed(settle(&f.rt, "p").await), json!({"got": 42}));
}

#[tokio::test]
async fn a_faulted_child_is_caught_and_await_false_outlives_the_parent() {
    let f = fixture();
    save(
        &f.rt,
        "bad",
        &doc("bad", "do: [ { r: { raise: { error: { type: 'urn:t:bad', status: 418, title: Bad } } } } ]"),
    );
    save(&f.rt, "slow", &doc("slow", "do: [ { w: { wait: PT1H } }, { done: { set: { ok: true } } } ]"));
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
    );
    f.rt.start("p", "parent", json!({})).unwrap();
    let out = completed(settle(&f.rt, "p").await);
    assert_eq!(out, json!({"status": 418, "instance": "/do/1/guarded/try/0/sub", "type": "urn:t:bad", "missing": 400}));
    let slow = f.rt.list(Some("slow"), "").unwrap();
    assert_eq!(slow.len(), 1);
    assert!(slow[0].ended_ms.is_none(), "an independent run is not cancelled with its parent");
}

#[tokio::test]
async fn reserved_child_functions_still_join_later() {
    let f = fixture();
    save(&f.rt, "child", &doc("child", "do: [ { r: { set: { answer: 42 } } } ]"));
    save(
        &f.rt,
        "parent",
        &doc(
            "parent",
            "do:\n  - start: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }",
        ),
    );
    f.rt.start("p", "parent", json!({})).unwrap();
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
    );
    save(
        &rt,
        "emitter",
        &doc(
            "emitter",
            "do:\n  - loop:\n      for: { in: [ 1, 2, 3 ] }\n      do: [ { e: { emit: { event: { with: { type: reading, source: 'https://sensor', data: { v: '${ $item }' } } } } } } ]",
        ),
    );
    rt.start("l", "listener", json!({})).unwrap();
    assert!(matches!(rt.drive("l", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    rt.start("e", "emitter", json!({})).unwrap();
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
    );
    f.rt.start("r", "room", json!({"room": "kitchen"})).unwrap();
    f.rt.drive("r", &CancellationToken::new()).await.unwrap();
    let mut other = event("1", "temperature", json!(30));
    other["roomid"] = json!("hall");
    let mut temp = event("2", "temperature", json!(21));
    temp["roomid"] = json!("kitchen");
    let mut hum = event("3", "humidity", json!(40));
    hum["roomid"] = json!("kitchen");
    for e in [other, temp.clone(), temp, hum] {
        f.rt.publish(e).unwrap();
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
    );
    f.rt.start("s", "stream", json!({})).unwrap();
    f.rt.drive("s", &CancellationToken::new()).await.unwrap();
    f.rt.publish(event("1", "order", json!({"n": 1}))).unwrap();
    f.rt.drive("s", &CancellationToken::new()).await.unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    drop(f);
    let f = open(&writes);
    // The same event again is not consumed twice.
    assert!(f.rt.publish(event("1", "order", json!({"n": 1}))).unwrap().is_empty());
    f.rt.publish(event("2", "order", json!({"n": 2}))).unwrap();
    f.rt.publish(event("3", "closed", json!({}))).unwrap();
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
    );
    f.rt.start("x", "ext", json!({})).unwrap();
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
    );
    f.rt.start("a", "attempts", json!({})).unwrap();
    for t in [0, 1_000, 2_000, 3_000] {
        f.time.set(t);
        if settle(&f.rt, "a").await.is_some() {
            break;
        }
    }
    assert_eq!(completed(f.rt.get("a").unwrap().unwrap().outcome), json!({"status": 408}));
}

// ── processes, secrets, resources ──────────────────────────────────────────

struct Shell(AtomicUsize);
#[async_trait]
impl ProcessRunner for Shell {
    fn effect(&self, kind: &str) -> Option<Effect> {
        (kind == "shell").then_some(Effect::Write)
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
    assert!(f.rt.save("p", &body("all")).unwrap_err().to_string().contains("process runner"));
    let shell = Arc::new(Shell(AtomicUsize::new(0)));
    let rt = f.rt.with_process_runner(shell.clone());
    for (r, expected) in [
        ("all", json!({"code": 3, "stdout": "ls", "stderr": "warn"})),
        ("stdout", json!("ls")),
        ("stderr", json!("warn")),
        ("code", json!(3)),
        ("none", Value::Null),
    ] {
        save(&rt, "p", &body(r));
        rt.start(r, "p", json!({"cmd": "ls"})).unwrap();
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
        .register(Function::new(
            "call_api",
            json!({}),
            json!({}),
            Effect::Write,
            move |a: Value, _: FunctionContext| {
                r.lock().unwrap().push(a.clone());
                async move {
                    Err(WorkflowError::communication(
                        401,
                        format!("rejected header {}", a["auth"].as_str().unwrap_or("")),
                    ))
                }
            },
        ))
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
    );
    rt.start("s", "secret", json!({})).unwrap();
    let status = rt.drive("s", &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(received.lock().unwrap()[0]["auth"], format!("Bearer {SECRET}"), "the function gets the value");
    let Status::Faulted(e) = &status else { panic!("{status:?}") };
    assert!(e.detail.as_deref().is_some_and(|d| d.contains("rejected header")), "{e:?}");
    // Every persisted row, the outcome and every span: no secret value.
    let rows = serde_json::to_string(&backend.snapshot().unwrap().rows.values().collect::<Vec<_>>()).unwrap();
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
    assert!(rt.save("bad", &bad).unwrap_err().to_string().contains("input.from"));
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
    assert!(f.rt.save("typed", &body).unwrap_err().to_string().contains("resource resolver"));
    let schemas = Arc::new(Schemas(Mutex::new(BTreeMap::from([(
        "https://schemas.test/n.json".to_string(),
        json!({"type": "object", "required": ["n"]}),
    )]))));
    let rt = f.rt.with_resources(schemas.clone());
    save(&rt, "typed", &body);
    rt.start("ok", "typed", json!({"n": 1})).unwrap();
    rt.start("bad", "typed", json!({})).unwrap();
    // A schema that changes after admission does not change admitted runs.
    schemas.0.lock().unwrap().insert("https://schemas.test/n.json".into(), json!({"type": "string"}));
    assert_eq!(completed(settle(&rt, "ok").await), json!({"ok": true}));
    assert!(matches!(settle(&rt, "bad").await, Some(Status::Faulted(e)) if e.is("validation")));
}

// ── schedules ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_cron_and_after_start_each_slot_once() {
    let f = fixture();
    save(&f.rt, "every", &doc("every", "schedule: { every: PT1M }\ndo: [ { a: { set: { ok: true } } } ]"));
    save(&f.rt, "cron", &doc("cron", "schedule: { cron: '*/5 * * * *' }\ndo: [ { a: { set: { ok: true } } } ]"));
    save(&f.rt, "after", &doc("after", "schedule: { after: PT10S }\ndo: [ { a: { set: { ok: true } } } ]"));
    let started = f.rt.schedule(0, 60_000).unwrap();
    assert!(started.contains(&"schedule:every:every:60000".to_string()), "{started:?}");
    assert!(started.iter().any(|id| id.starts_with("schedule:after:after:")));
    assert!(!started.iter().any(|id| id.starts_with("schedule:cron:")), "no cron slot in the first minute");
    // Two schedulers over the same window start nothing more.
    assert!(f.rt.schedule(0, 60_000).unwrap().is_empty());
    let started = f.rt.schedule(60_000, 300_000).unwrap();
    assert!(started.contains(&"schedule:cron:cron:300000".to_string()), "{started:?}");
    assert!(started.contains(&"schedule:every:every:300000".to_string()));
    // The next `after` run waits for the previous one to end, plus the delay.
    assert!(!started.iter().any(|id| id.starts_with("schedule:after:")));
    f.time.set(300_000);
    settle(&f.rt, "schedule:after:after:0000000001").await;
    assert!(f.rt.schedule(300_000, 305_000).unwrap().is_empty());
    let started = f.rt.schedule(305_000, 310_000).unwrap();
    assert_eq!(started, vec!["schedule:after:after:0000000002".to_string()]);
}

#[tokio::test]
async fn schedule_on_starts_a_run_per_match_through_trigger_runs() {
    let f = fixture();
    save(
        &f.rt,
        "alarm",
        &doc(
            "alarm",
            "schedule: { on: { one: { with: { type: heartbeat.low, data: '${ .bpm < 50 }' } } } }\ndo: [ { a: { set: { patient: '${ .[0].patient }' } } } ]",
        ),
    );
    for (id, bpm, patient) in [("1", 40, "ann"), ("2", 70, "bob"), ("3", 45, "cid")] {
        f.rt.publish(event(id, "heartbeat.low", json!({"bpm": bpm, "patient": patient}))).unwrap();
    }
    let mut outputs = Vec::new();
    for _ in 0..10 {
        f.rt.schedule(0, 1).unwrap();
        while f.rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
        outputs =
            f.rt.list(Some("alarm"), "")
                .unwrap()
                .into_iter()
                .filter_map(|r| match r.outcome {
                    Some(Status::Completed(v)) => Some(v["patient"].clone()),
                    _ => None,
                })
                .collect();
        if outputs.len() == 2 {
            break;
        }
    }
    outputs.sort_by_key(|v| v.to_string());
    assert_eq!(outputs, vec![json!("ann"), json!("cid")]);
    // A deactivated schedule stops listening.
    save(&f.rt, "alarm", &doc("alarm", "do: [ { a: { set: { ok: true } } } ]"));
    f.rt.schedule(1, 2).unwrap();
    assert!(f.rt.list(Some("alarm#on"), "").unwrap().iter().all(|r| r.ended_ms.is_some()));
}

// ── crashes at every commit ────────────────────────────────────────────────

/// A memory backend whose `n`th commit fails, before or after it lands.
struct Crashing {
    inner: durable::memory::MemoryBackend,
    commits: AtomicUsize,
    fail_at: usize,
    after: bool,
}
impl AtomicBackend for Crashing {
    fn snapshot(&self) -> anyhow::Result<durable::Snapshot> {
        self.inner.snapshot()
    }
    fn compare_exchange(&self, before: &durable::Snapshot, rows: &durable::Rows) -> anyhow::Result<bool> {
        let n = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_at && !self.after {
            anyhow::bail!("injected crash before commit {n}");
        }
        let done = self.inner.compare_exchange(before, rows)?;
        if n == self.fail_at && self.after {
            anyhow::bail!("injected crash after commit {n}");
        }
        Ok(done)
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
    let setup = |rt: &Runtime| -> anyhow::Result<()> {
        for (name, body) in
            [("child", "do: [ { w: { call: write, with: { child: '${ .n }' } } } ]"), ("parent", PARENT)]
        {
            let d = rt.save(name, &doc(name, body))?;
            rt.activate(name, d.version)?;
        }
        rt.start("p", "parent", json!({}))?;
        Ok(())
    };
    while setup(&f.rt).is_err() {}
    let mut published = 0;
    for step in 1..200 {
        if let Some(status) = store.get("p").ok().flatten().and_then(|r| r.outcome) {
            let consumed = store
                .inspect("p")
                .unwrap()
                .unwrap()
                .events
                .iter()
                .filter(|e| e.consumed_key.is_some() && e.type_ == "order")
                .count();
            let children = store.list(Some("child"), "").unwrap().len();
            let commits = backend.commits.load(Ordering::SeqCst);
            return (Some(status), f.seen.lock().unwrap().clone(), consumed, children, commits);
        }
        let _ = f.rt.recover_once(&CancellationToken::new()).await;
        let listening = store.inspect("p").ok().flatten().is_some_and(|s| !s.subscriptions.is_empty());
        while listening && published < 2 {
            let n = published + 1;
            if f.rt.publish(event(&n.to_string(), "order", json!({ "n": n }))).is_err() {
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
    );
    rt.start("r", "args", json!({ "n": 1 })).unwrap();
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
    assert!(rt.save("bad", &bad).unwrap_err().to_string().contains("$authorization"));
}

#[tokio::test]
async fn dry_runs_record_emits_and_run_children_from_the_catalog() {
    let f = fixture();
    save(&f.rt, "child", &doc("child", "do: [ { w: { call: write, with: { from: child } } } ]"));
    let definition = doc(
        "parent",
        "do:\n  - sub: { run: { workflow: { namespace: test, name: child, version: latest } } }\n  - say: { emit: { event: { with: { type: said, source: 'https://p', data: '${ . }' } } } }",
    );
    let dry =
        f.rt.dry_run(
            &definition,
            json!({}),
            Arc::new(|_: &str, a: &Value, _: &FunctionContext| Ok(a.clone())),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    let functions: Vec<&str> = dry.writes.iter().map(|w| w.function.as_str()).collect();
    assert_eq!(functions, ["write", "emit"]);
    assert_eq!(f.seen.lock().unwrap().len(), 0, "nothing reached the real write");
    assert!(f.rt.list(None, "").unwrap().is_empty(), "nothing was admitted for real");
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
    );
    f.rt.publish(event("1", "a", json!(1))).unwrap();
    f.rt.schedule(0, 1).unwrap();
    while f.rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    assert!(f.rt.list(Some("pair"), "").unwrap().is_empty(), "one of two is not enough");
    f.rt.publish(event("2", "b", json!(2))).unwrap();
    for _ in 0..3 {
        while f.rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
        f.rt.schedule(1, 2).unwrap();
    }
    while f.rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    let runs = f.rt.list(Some("pair"), "").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, Some(Status::Completed(json!(["a", "b"]))));
}
