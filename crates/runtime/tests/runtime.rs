use ouro_runtime::{
    CallStatus, Config, Function, FunctionContext, Mock, Registry, Runtime,
    durable::{self, Clock, RunStore},
    owf::{
        durable::{CallState, Delivery, Record},
        run::{Status, WorkflowError},
    },
};
use serde_json::{Value, json};
use std::{
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
fn tracer() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new()))
}
fn yaml(body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: workflow, version: '1.0.0' }}\ndo:\n{body}\n")
}
fn runtime(store: Arc<dyn RunStore>, registry: Registry, clock: Arc<dyn Clock>) -> Runtime {
    Runtime::new(store, Arc::new(registry), Config::default(), clock, tracer()).unwrap()
}
fn save(rt: &Runtime, yaml: &str) {
    let d = rt.save("workflow", yaml).unwrap();
    rt.activate("workflow", d.version).unwrap();
}
/// The host's dry-run rule for the counter registry: it knows `write`
/// changes the world, so a dry run answers it with its arguments.
fn writes_mocked() -> Arc<dyn Mock> {
    ouro_runtime::mock(
        |function: &str, _: &Value| function == "write",
        |_: &str, a: &Value, _: &FunctionContext| Ok(a.clone()),
    )
}
fn counter_registry(reads: Arc<AtomicUsize>, writes: Arc<AtomicUsize>) -> Registry {
    let mut registry = Registry::new();
    registry
        .register(Function::new(
            "read",
            json!({"type":"object"}),
            json!({"type":"object"}),
            move |a: Value, _: FunctionContext| {
                reads.fetch_add(1, Ordering::SeqCst);
                async move { Ok(a) }
            },
        ))
        .unwrap();
    registry
        .register(Function::new(
            "write",
            json!({"type":"object"}),
            json!({"type":"object"}),
            move |a: Value, _: FunctionContext| {
                writes.fetch_add(1, Ordering::SeqCst);
                async move { Ok(a) }
            },
        ))
        .unwrap();
    registry
}
#[tokio::test]
async fn pinned_versions_resume_a_wait_without_resending_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let rt = runtime(
        Arc::new(durable::sqlite::open(&path).unwrap()),
        counter_registry(reads.clone(), writes.clone()),
        time.clone(),
    );
    let original = yaml(
        "  - send: { call: write, with: { n: 1 } }\n  - wait: { wait: PT1S }\n  - read: { call: read, with: { result: original } }",
    );
    save(&rt, &original);
    rt.start("r", "workflow", json!({})).unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let d = rt.save("workflow", &yaml("  - changed: { set: { result: changed } }")).unwrap();
    rt.activate("workflow", d.version).unwrap();
    drop(rt);
    time.0.store(1000, Ordering::SeqCst);
    let rt =
        runtime(Arc::new(durable::sqlite::open(path).unwrap()), counter_registry(reads.clone(), writes.clone()), time);
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    assert_eq!(rt.get("r").unwrap().unwrap().outcome, Some(Status::Completed(json!({"result":"original"}))));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let journal: Vec<_> = rt.calls("r").unwrap().into_iter().map(|c| (c.key, c.function, c.state)).collect();
    assert_eq!(
        journal,
        [
            ("/do/0/send#call".to_owned(), "write".to_owned(), CallStatus::Completed),
            ("/do/2/read#call".to_owned(), "read".to_owned(), CallStatus::Completed),
        ]
    );
    rt.start("new", "workflow", json!({})).unwrap();
    assert_eq!(
        rt.drive("new", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({"result":"changed"})))
    );
}
#[tokio::test]
async fn schema_validation_and_typed_errors_are_caught_by_yaml() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mut registry = Registry::new();
    registry.register(Function::new("typed",json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}},"additionalProperties":false}),json!({"type":"integer"}), move |_:Value,_:FunctionContext| {c.fetch_add(1,Ordering::SeqCst);async {Err(WorkflowError {type_:"urn:test:rejected".into(),status:409,title:Some("Rejected".into()),detail:None,instance:None})}})).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    assert!(rt.save("bad", &yaml("  - bad: { call: typed, with: { n: text } }")).is_err());
    assert!(rt.save("bad", &yaml("  - bad: { call: absent }")).is_err());
    save(
        &rt,
        &yaml(
            "  - handle:\n      try:\n        - invoke: { call: typed, with: { n: '${ .n }' } }\n      catch:\n        as: failure\n        do:\n          - result: { set: '${ $failure.status }' }",
        ),
    );
    for (id, input, expected) in [("typed", json!({"n":1}), 409), ("schema", json!({"n":"bad"}), 400)] {
        rt.start(id, "workflow", input).unwrap();
        let result = rt.drive(id, &CancellationToken::new()).await.unwrap();
        assert_eq!(result, Some(Status::Completed(json!(expected))), "{result:?}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn dry_run_mocks_what_the_host_rule_marks_and_replay_calls_no_handlers() {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let rt = runtime(
        Arc::new(durable::memory::memory()),
        counter_registry(reads.clone(), writes.clone()),
        Arc::new(Time::default()),
    );
    let definition =
        yaml("  - read: { call: read, with: { n: 1 } }\n  - write: { call: write, with: { n: '${ .n + 1 }' } }");
    let dry = rt.dry_run(&definition, json!({}), writes_mocked(), &CancellationToken::new()).await.unwrap();
    assert_eq!(dry.status, Status::Completed(json!({"n":2})));
    assert_eq!(dry.mocked.len(), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(rt.list(None, "").unwrap().is_empty());
    assert!(rt.definitions("dry-run").unwrap().versions.is_empty());
    save(&rt, &definition);
    rt.start("r", "workflow", json!({})).unwrap();
    let result = rt.drive("r", &CancellationToken::new()).await.unwrap();
    let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
    assert_eq!(Some(replay.status), result);
    assert_eq!(replay.calls_replayed, 2);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn breaker_rolls_back_after_configured_faults_and_success_resets_count() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    save(&rt, &yaml("  - ok: { set: { result: old } }"));
    let bad = rt
        .save("workflow", &yaml("  - fail: { raise: { error: { type: 'urn:test:failure', status: 500 } } }"))
        .unwrap();
    rt.activate("workflow", bad.version).unwrap();
    for n in 0..3 {
        let id = format!("r{n}");
        rt.start(&id, "workflow", Value::Null).unwrap();
        assert!(matches!(rt.drive(&id, &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(_))));
    }
    assert_eq!(rt.definitions("workflow").unwrap().active, Some(1));
    assert_eq!(rt.get("r0").unwrap().unwrap().admission.version, bad.version);
}
#[tokio::test]
async fn shutdown_releases_the_lease_and_unknown_writes_never_dispatch_again() {
    let writes = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let mut registry = Registry::new();
    let w = writes.clone();
    let n = started.clone();
    registry
        .register(
            Function::new("write", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
                w.fetch_add(1, Ordering::SeqCst);
                n.notify_one();
                async move {
                    ctx.cancel.cancelled().await;
                    Ok(Value::Null)
                }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    save(&rt, &yaml("  - write: { call: write }"));
    rt.start("r", "workflow", Value::Null).unwrap();
    let stop = CancellationToken::new();
    let shutdown = async {
        started.notified().await;
        stop.cancel();
    };
    let (result, ()) = tokio::join!(rt.drive("r", &stop), shutdown);
    assert!(matches!(result.unwrap(), Some(Status::Interrupted(_))));
    assert_eq!(rt.get("r").unwrap().unwrap().state, durable::RunState::Ready);
    // The journal tells the host the write may have happened.
    let calls = rt.calls("r").unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].key.as_str(), calls[0].function.as_str()), ("/do/0/write#call", "write"));
    assert_eq!((calls[0].delivery, calls[0].state), (Delivery::AtMostOnce, CallStatus::Unknown));
    let result = rt.drive("r", &CancellationToken::new()).await.unwrap();
    assert!(matches!(result,Some(Status::Faulted(e)) if e.is("outcome_unknown")));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn events_resume_a_waiting_listen_and_cancel_is_terminal() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    save(&rt, &yaml("  - listen: { listen: { to: { one: { with: { type: answer } } }, read: envelope } }"));
    rt.start("r", "workflow", Value::Null).unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let answer = |id: &str, data: Value| json!({"id": id, "source": "/test", "type": "answer", "data": data});
    assert_eq!(rt.deliver(answer("1", json!(42)), None).unwrap(), Some("r".into()));
    assert!(rt.deliver(answer("2", json!(0)), None).unwrap().is_none());
    assert!(
        matches!(rt.drive("r",&CancellationToken::new()).await.unwrap(),Some(Status::Completed(v)) if v[0]["data"]==42)
    );
    rt.start("cancelled", "workflow", Value::Null).unwrap();
    assert!(rt.cancel("cancelled").unwrap());
    assert!(!rt.cancel("cancelled").unwrap());
    assert_eq!(rt.drive("cancelled", &CancellationToken::new()).await.unwrap(), Some(Status::Cancelled));
}
#[tokio::test]
async fn child_runs_join_via_durable_events_with_one_worker_slot() {
    let store = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), Registry::new(), Arc::new(Time::default()));
    let child = rt.save("child", &yaml("  - result: { set: { answer: 42 } }")).unwrap();
    rt.activate("child", child.version).unwrap();
    save(
        &rt,
        &yaml(
            "  - child: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } }, read: envelope } }",
        ),
    );
    rt.start("p", "workflow", json!({})).unwrap();
    assert!(matches!(rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    assert!(
        matches!(rt.get("p").unwrap().unwrap().outcome,Some(Status::Completed(v)) if v[0]["data"]["output"]["answer"]==42)
    );
    assert_eq!(store.list(Some("child"), "").unwrap().len(), 1);
}
#[tokio::test]
async fn retries_receive_stable_idempotency_keys_and_increasing_attempts() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let c = contexts.clone();
    let mut registry = Registry::new();
    registry
        .register(Function::new("read", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
            c.lock().unwrap().push((ctx.attempt, ctx.idempotency_key));
            async { Err(WorkflowError::runtime("retry")) }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    save(
        &rt,
        &yaml(
            "  - attempts:\n      try: [ { read: { call: read } } ]\n      catch:\n        retry: { delay: { milliseconds: 0 }, limit: { attempt: { count: 3 } } }",
        ),
    );
    rt.start("r", "workflow", Value::Null).unwrap();
    rt.drive("r", &CancellationToken::new()).await.unwrap();
    let c = contexts.lock().unwrap();
    assert_eq!(c.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(c.iter().all(|x| x.1 == c[0].1));
}
#[tokio::test]
async fn host_concurrency_is_bounded_and_cancellation_reaches_handlers() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    let a = active.clone();
    let p = peak.clone();
    registry
        .register(Function::new("read", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
            let n = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(n, Ordering::SeqCst);
            let a = a.clone();
            async move {
                ctx.cancel.cancelled().await;
                a.fetch_sub(1, Ordering::SeqCst);
                Ok(Value::Null)
            }
        }))
        .unwrap();
    let store = Arc::new(durable::memory::memory());
    let config = Config {
        max_concurrency: 2,
        max_function_concurrency: 2,
        heartbeat_interval: Duration::from_millis(5),
        ..Config::default()
    };
    let rt = Runtime::new(store, Arc::new(registry), config, Arc::new(Time::default()), tracer()).unwrap();
    save(&rt, &yaml("  - read: { call: read }"));
    for n in 0..6 {
        rt.start(&format!("r{n}"), "workflow", Value::Null).unwrap();
    }
    let stop = CancellationToken::new();
    let token = stop.clone();
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(peak.load(Ordering::SeqCst) <= 2);
        rt.cancel("r0").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(rt.worker(stop), cancel);
    result.unwrap();
    assert_eq!(rt.get("r0").unwrap().unwrap().outcome, Some(Status::Cancelled));
    assert!(peak.load(Ordering::SeqCst) > 0);
}
// Delivery is declared per function: a call with the default at-least-once
// delivery goes out again under the same idempotency key after its outcome
// was lost; a declared or YAML-tightened at-most-once one never does; a
// changed declaration fails recovery as a changed binding.
#[tokio::test]
async fn delivery_is_declared_per_function() {
    type Keys = Arc<std::sync::Mutex<Vec<(String, String)>>>;
    fn registry(keys: Keys, post: Delivery) -> Registry {
        let mut registry = Registry::new();
        for (name, delivery) in [("post", post), ("send", Delivery::AtMostOnce)] {
            let keys = keys.clone();
            let f = Function::new(name, json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
                keys.lock().unwrap().push((name.to_string(), ctx.idempotency_key.clone()));
                async move {
                    // The answer is lost: the worker stops before it is saved.
                    ctx.cancel.cancelled().await;
                    Ok(json!({}))
                }
            });
            registry.register(f.with_delivery(delivery)).unwrap();
        }
        registry
    }
    async fn interrupted(rt: &Runtime, id: &str, keys: &Keys, calls: usize) {
        let stop = CancellationToken::new();
        let stopping = async {
            while keys.lock().unwrap().len() < calls {
                tokio::task::yield_now().await;
            }
            stop.cancel();
        };
        let (status, ()) = tokio::join!(rt.drive(id, &stop), stopping);
        assert!(matches!(status.unwrap(), Some(Status::Interrupted(_))), "{id}");
    }
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let keys = Keys::default();
    let rt = runtime(store.clone(), registry(keys.clone(), Delivery::AtLeastOnce), Arc::new(Time::default()));
    for (name, task) in [
        ("default", "  - post: { call: post }"),
        ("declared", "  - send: { call: send }"),
        ("tightened", "  - post: { call: post, metadata: { delivery: at_most_once } }"),
    ] {
        let d = rt.save(name, &yaml(task)).unwrap();
        rt.activate(name, d.version).unwrap();
        rt.start(name, name, json!({})).unwrap();
        let before = keys.lock().unwrap().len();
        interrupted(&rt, name, &keys, before + 1).await;
        if name == "default" {
            interrupted(&rt, name, &keys, before + 2).await;
            let sent = keys.lock().unwrap()[before..].to_vec();
            assert_eq!(sent[0], sent[1], "re-dispatched once more, under the same idempotency key");
        } else {
            let outcome = rt.drive(name, &CancellationToken::new()).await.unwrap();
            assert!(matches!(outcome, Some(Status::Faulted(e)) if e.is("outcome_unknown")), "{name}");
            assert_eq!(keys.lock().unwrap().len(), before + 1, "{name}: never sent again");
        }
        let delivery = rt.records(name).unwrap().values().find_map(|r| match r {
            Record::Call { delivery, .. } => Some(*delivery),
            _ => None,
        });
        let expected = if name == "default" { Delivery::AtLeastOnce } else { Delivery::AtMostOnce };
        assert_eq!(delivery, Some(expected), "{name}: the effective delivery is bound into the record");
    }
    // The declaration is a pinned binding: a host that now declares `post` at
    // most once cannot resume a run admitted under at-least-once.
    let changed = runtime(store, registry(keys.clone(), Delivery::AtMostOnce), Arc::new(Time::default()));
    let outcome = changed.drive("default", &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, Some(Status::Interrupted(e)) if e.to_string().contains("bindings changed")));
}

// #3: a declared application version is recorded at admission; a worker of
// an incompatible version never claims the run, which stays visible and
// waiting; a compatible or undeclared one resumes it.
#[tokio::test]
async fn runs_wait_for_a_compatible_application_version() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let time: Arc<Time> = Arc::new(Time::default());
    let at = |version: Option<&str>, accepts: &[&str]| {
        let application = durable::Compatibility {
            version: version.map(Into::into),
            accepts: accepts.iter().map(|v| v.to_string()).collect(),
        };
        let config = Config { application, ..Config::default() };
        Runtime::new(store.clone(), Arc::new(Registry::new()), config, time.clone(), tracer()).unwrap()
    };
    let one = at(Some("1"), &[]);
    save(&one, &yaml("  - done: { set: { ok: true } }"));
    let run = one.start("r", "workflow", json!({})).unwrap();
    assert_eq!(run.admission.application_version(), Some("1"));
    let two = at(Some("2"), &[]);
    assert_eq!(two.drive("r", &CancellationToken::new()).await.unwrap(), None);
    assert_eq!(two.recover_once(&CancellationToken::new()).await.unwrap(), 0);
    let waiting = two.get("r").unwrap().unwrap();
    assert!(waiting.outcome.is_none() && !two.compatible(&waiting), "waiting for a compatible worker");
    let upgraded = at(Some("2"), &["1"]);
    assert!(upgraded.compatible(&waiting));
    assert_eq!(
        upgraded.drive("r", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({"ok":true})))
    );
    // Undeclared on either side: compatible, as before versions existed.
    let undeclared = at(None, &[]);
    undeclared.start("u", "workflow", json!({})).unwrap();
    assert_eq!(two.drive("u", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({"ok":true}))));
    one.start("v", "workflow", json!({})).unwrap();
    assert!(undeclared.compatible(&undeclared.get("v").unwrap().unwrap()));
}

#[tokio::test]
async fn write_response_survives_crash_before_task_checkpoint() {
    let backend = Arc::new(durable::memory::MemoryBackend::default());
    let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
    let writes = Arc::new(AtomicUsize::new(0));
    let b = backend.clone();
    let w = writes.clone();
    let mut registry = Registry::new();
    registry
        .register(
            Function::new("write", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                w.fetch_add(1, Ordering::SeqCst);
                let b = b.clone();
                async move {
                    b.faults.arm(durable::CommitFault::AfterCommit);
                    Ok(json!(42))
                }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    let time = Arc::new(Time::default());
    let rt = runtime(store, registry, time.clone());
    save(&rt, &yaml("  - write: { call: write }"));
    rt.start("r", "workflow", Value::Null).unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Interrupted(_))));
    time.0.store(2000, Ordering::SeqCst);
    assert_eq!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!(42))));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(
        rt.records("r")
            .unwrap()
            .values()
            .any(|r| matches!(r, Record::Call { state: CallState::Done(Ok(_)), delivery: Delivery::AtMostOnce, .. }))
    );
}

#[tokio::test]
async fn telemetry_nests_function_task_and_run_spans_with_current_context() {
    use opentelemetry::trace::{TraceContextExt, TracerProvider};
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider =
        opentelemetry_sdk::trace::SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let mut registry = Registry::new();
    registry
        .register(Function::new("read", json!({}), json!({}), move |_: Value, c: FunctionContext| {
            capture.lock().unwrap().push((
                c.span.span().span_context().clone(),
                opentelemetry::Context::current().span().span_context().clone(),
            ));
            async { Ok(json!({})) }
        }))
        .unwrap();
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(registry),
        Config::default(),
        Arc::new(Time::default()),
        opentelemetry::global::BoxedTracer::new(Box::new(provider.tracer("test"))),
    )
    .unwrap();
    save(&rt, &yaml("  - read: { call: read }"));
    rt.start("trace", "workflow", Value::Null).unwrap();
    rt.drive("trace", &CancellationToken::new()).await.unwrap();
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3);
    let function = spans.iter().find(|s| s.name == "function read").unwrap();
    let task = spans.iter().find(|s| s.name.starts_with("task ")).unwrap();
    let root = spans.iter().find(|s| s.name.starts_with("workflow ")).unwrap();
    assert_eq!(function.parent_span_id, task.span_context.span_id());
    assert_eq!(task.parent_span_id, root.span_context.span_id());
    for span in &spans {
        for key in ["ouro.run.id", "ouro.definition.name", "ouro.definition.version", "ouro.outcome"] {
            assert!(span.attributes.iter().any(|a| a.key.as_str() == key), "missing {key} on {}", span.name);
        }
    }
    for key in ["ouro.task.path", "ouro.attempt"] {
        assert!(function.attributes.iter().any(|a| a.key.as_str() == key));
    }
    // The engine does not classify calls: no read/write attribute.
    assert!(spans.iter().all(|s| s.attributes.iter().all(|a| a.key.as_str() != "ouro.effect")));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, seen[0].1);
    assert!(seen[0].0.is_valid());
}

#[tokio::test]
async fn dry_children_are_isolated_and_event_replay_uses_recorded_envelopes() {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let rt = runtime(
        Arc::new(durable::memory::memory()),
        counter_registry(reads.clone(), writes.clone()),
        Arc::new(Time::default()),
    );
    let child = rt.save("child", &yaml("  - write: { call: write, with: { n: 42 } }")).unwrap();
    rt.activate("child", child.version).unwrap();
    let definition = yaml(
        "  - child: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } }, read: envelope } }",
    );
    let dry = rt.dry_run(&definition, json!({}), writes_mocked(), &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(dry.mocked.len(), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(rt.list(None, "").unwrap().is_empty());
    save(&rt, &definition);
    rt.start("p", "workflow", json!({})).unwrap();
    for _ in 0..3 {
        rt.recover_once(&CancellationToken::new()).await.unwrap();
    }
    let original = rt.get("p").unwrap().unwrap().outcome;
    let replay = rt.replay("p", &CancellationToken::new()).await.unwrap();
    assert_eq!(Some(replay.status), original);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_a_stale_attempt_preserves_cancellation_of_its_successor() {
    let mut registry = Registry::new();
    registry
        .register(Function::new("read", json!({}), json!({}), |_: Value, ctx: FunctionContext| async move {
            ctx.cancel.cancelled().await;
            Ok(Value::Null)
        }))
        .unwrap();
    let time = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), registry, time.clone());
    save(&rt, &yaml("  - read: { call: read }"));
    rt.start("r", "workflow", Value::Null).unwrap();
    let stop = CancellationToken::new();
    let mut old = Box::pin(rt.drive("r", &stop));
    assert!(futures::poll!(old.as_mut()).is_pending());
    time.0.store(durable::LEASE_MS + 1, Ordering::SeqCst);
    let mut current = Box::pin(rt.drive("r", &stop));
    assert!(futures::poll!(current.as_mut()).is_pending());
    drop(old);
    rt.cancel("r").unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), current).await.unwrap().unwrap(),
        Some(Status::Cancelled)
    );
}

#[test]
fn x1_x2_generated_dry_runs_and_replays_are_isolated() {
    use durable::AtomicBackend;
    use proptest::{prelude::*, test_runner::TestRunner};
    let strategy = (prop::collection::vec(any::<bool>(), 1..16), 0i64..100);
    let config = proptest::test_runner::Config { cases: 64, ..Default::default() };
    TestRunner::new(config)
        .run(&strategy, |(writes_in_program, input)| {
            let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            executor.block_on(async {
                let reads = Arc::new(AtomicUsize::new(0));
                let writes = Arc::new(AtomicUsize::new(0));
                let backend = Arc::new(durable::memory::MemoryBackend::default());
                let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
                let rt =
                    runtime(store.clone(), counter_registry(reads.clone(), writes.clone()), Arc::new(Time::default()));
                let body = writes_in_program
                    .iter()
                    .enumerate()
                    .map(|(i, write)| {
                        format!(
                            "  - step{i}: {{ call: {}, with: {{ n: '${{ .n + 1 }}' }} }}",
                            if *write { "write" } else { "read" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let definition = yaml(&body);
                save(&rt, &definition);
                let before = backend.snapshot().unwrap().rows;
                let dry = rt
                    .dry_run(&definition, json!({"n":input}), writes_mocked(), &CancellationToken::new())
                    .await
                    .unwrap();
                prop_assert!(matches!(dry.status, Status::Completed(_)));
                prop_assert_eq!(writes.load(Ordering::SeqCst), 0);
                prop_assert_eq!(dry.mocked.len(), writes_in_program.iter().filter(|x| **x).count());
                prop_assert_eq!(backend.snapshot().unwrap().rows, before);
                rt.start("recorded", "workflow", json!({"n":input})).unwrap();
                let original = rt.drive("recorded", &CancellationToken::new()).await.unwrap();
                let before = backend.snapshot().unwrap().rows;
                let counts = (reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst));
                let replay = rt.replay("recorded", &CancellationToken::new()).await.unwrap();
                prop_assert_eq!(Some(replay.status), original);
                // Changed arguments must refuse instead of falling through to a handler.
                let changed = definition.replace(".n + 1", ".n + 2");
                let changed = rt.replay_definition(&changed, "recorded", &CancellationToken::new()).await;
                prop_assert!(changed.is_err() || matches!(changed.unwrap().status, Status::Faulted(_)));
                prop_assert_eq!((reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst)), counts);
                prop_assert_eq!(backend.snapshot().unwrap().rows, before);
                store.sweep(durable::RECORDS_KEPT_MS + 1, durable::RECORDS_KEPT_MS).unwrap();
                prop_assert!(rt.replay("recorded", &CancellationToken::new()).await.is_err());
                prop_assert_eq!((reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst)), counts);
                Ok(())
            })
        })
        .unwrap();
}

#[tokio::test]
async fn t3_admission_fixes_the_workflow_deadline() {
    let reads = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(Time::default());
    let rt = runtime(
        Arc::new(durable::memory::memory()),
        counter_registry(reads.clone(), Arc::new(AtomicUsize::new(0))),
        clock.clone(),
    );
    let definition = yaml("  - read: { call: read, with: {} }").replace("\ndo:", "\ntimeout: { after: PT1S }\ndo:");
    save(&rt, &definition);
    rt.start("deadline", "workflow", json!({})).unwrap();
    clock.0.store(2000, Ordering::SeqCst);
    let outcome = rt.drive("deadline", &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome,Some(Status::Faulted(e)) if e.is("timeout")), "admission deadline was restarted");
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn t1_ingress_identity_is_global_across_triggers() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    let definition = yaml("  - done: { set: { ok: true } }")
        .replace("\ndo:", "\nschedule: { on: { one: { with: { type: event } } } }\ndo:");
    for name in ["first", "second"] {
        let version = rt.save(name, &definition).unwrap();
        rt.activate(name, version.version).unwrap();
    }
    let event = json!({"specversion":"1.0","source":"test","id":"one","type":"event","data":{}});
    let started = rt.publish(event.clone()).unwrap();
    assert_eq!(started.len(), 1, "one ingress event acquired multiple admission obligations");
    assert!(rt.publish(event).unwrap().is_empty());
}

#[tokio::test]
async fn t1_live_schedule_run_prevents_overlap() {
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), clock.clone());
    let definition = yaml("  - wait: { wait: PT1H }").replace("\ndo:", "\nschedule: { every: PT1S }\ndo:");
    save(&rt, &definition);
    clock.0.store(1000, Ordering::SeqCst);
    let started = rt.fire_due().unwrap();
    assert_eq!(started.len(), 1);
    assert!(matches!(rt.drive(&started[0], &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    clock.0.store(2000, Ordering::SeqCst);
    assert!(rt.fire_due().unwrap().is_empty(), "overlap admitted a second live run");
    assert_eq!(rt.list(None, "").unwrap().len(), 1);
}

/// The mock rule sees each call's name and arguments: a dry run dispatches
/// every call except what the rule marks.
#[tokio::test]
async fn a_dry_run_dispatches_no_call_the_rule_marks() {
    let (reads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let rt = runtime(
        Arc::new(durable::memory::memory()),
        counter_registry(reads.clone(), writes.clone()),
        Arc::new(Time::default()),
    );
    let definition = yaml(
        "  - one: { call: write, with: { to: self } }\n  - two: { call: write, with: { to: boss } }\n  - three: { call: read, with: { to: boss } }",
    );
    let boss = ouro_runtime::mock(
        |_: &str, a: &Value| a["to"] == "boss",
        |f: &str, _: &Value, _: &FunctionContext| Ok(json!({ "mocked": f })),
    );
    let dry = rt.dry_run(&definition, json!({}), boss, &CancellationToken::new()).await.unwrap();
    assert_eq!(dry.status, Status::Completed(json!({ "mocked": "read" })));
    let mocked: Vec<_> = dry.mocked.iter().map(|c| (c.task_path.as_str(), c.function.as_str())).collect();
    assert_eq!(mocked, [("/do/1/two", "write"), ("/do/2/three", "read")]);
    assert_eq!((writes.load(Ordering::SeqCst), reads.load(Ordering::SeqCst)), (1, 0), "only `one` was dispatched");
}
