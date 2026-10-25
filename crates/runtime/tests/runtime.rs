use ouro_runtime::{
    CALL_UNKNOWN, CallStatus, Config, Function, FunctionContext, MissingFunction, Mock, Registry, Runtime,
    durable::{self, Clock, RunStore},
    ows::{
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
async fn save(rt: &Runtime, yaml: &str) {
    let d = rt.save("workflow", yaml).await.unwrap();
    rt.activate("workflow", d.version).await.unwrap();
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
    save(&rt, &original).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let d = rt.save("workflow", &yaml("  - changed: { set: { result: changed } }")).await.unwrap();
    rt.activate("workflow", d.version).await.unwrap();
    drop(rt);
    time.0.store(1000, Ordering::SeqCst);
    let rt =
        runtime(Arc::new(durable::sqlite::open(path).unwrap()), counter_registry(reads.clone(), writes.clone()), time);
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    assert_eq!(rt.get("r").await.unwrap().unwrap().outcome, Some(Status::Completed(json!({"result":"original"}))));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let journal: Vec<_> = rt.calls("r").await.unwrap().into_iter().map(|c| (c.key, c.function, c.state)).collect();
    assert_eq!(
        journal,
        [
            ("/do/0/send#call".to_owned(), "write".to_owned(), CallStatus::Completed),
            ("/do/2/read#call".to_owned(), "read".to_owned(), CallStatus::Completed),
        ]
    );
    rt.start("new", "workflow", json!({})).await.unwrap();
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
    registry.register(Function::new("typed",json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}},"additionalProperties":false}),json!({"type":"integer"}), move |_:Value,_:FunctionContext| {c.fetch_add(1,Ordering::SeqCst);async {Err::<Value, _>(WorkflowError {type_:"urn:test:rejected".into(),status:409,title:Some("Rejected".into()),detail:None,instance:None})}})).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    assert!(rt.save("bad", &yaml("  - bad: { call: typed, with: { n: text } }")).await.is_err());
    assert!(rt.save("bad", &yaml("  - bad: { call: absent }")).await.is_err());
    save(
        &rt,
        &yaml(
            "  - handle:\n      try:\n        - invoke: { call: typed, with: { n: '${ .n }' } }\n      catch:\n        as: failure\n        do:\n          - result: { set: '${ $failure.status }' }",
        ),
    ).await;
    for (id, input, expected) in [("typed", json!({"n":1}), 409), ("schema", json!({"n":"bad"}), 400)] {
        rt.start(id, "workflow", input).await.unwrap();
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
    assert!(rt.list(None, "").await.unwrap().is_empty());
    assert!(rt.definitions("dry-run").await.unwrap().versions.is_empty());
    save(&rt, &definition).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    let result = rt.drive("r", &CancellationToken::new()).await.unwrap();
    let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
    assert_eq!(Some(replay.status), result);
    assert_eq!(replay.calls_replayed, 2);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn breaker_trips_after_configured_faults_and_success_resets_count() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    save(&rt, &yaml("  - ok: { set: { result: old } }")).await;
    let flaky = "  - fail: { if: '${ .fail }', raise: { error: { type: 'urn:test:failure', status: 500 } } }";
    let bad = rt.save("workflow", &yaml(flaky)).await.unwrap();
    rt.activate("workflow", bad.version).await.unwrap();
    let stop = CancellationToken::new();
    let run = async |id: &'static str, fail: bool| {
        rt.start(id, "workflow", json!({ "fail": fail })).await.unwrap();
        rt.drive(id, &stop).await
    };
    let faults = async || rt.definitions("workflow").await.unwrap().consecutive_faults;
    for id in ["f0", "f1"] {
        assert!(matches!(run(id, true).await.unwrap(), Some(Status::Faulted(_))));
    }
    assert_eq!(faults().await, 2);
    assert!(matches!(run("s0", false).await.unwrap(), Some(Status::Completed(_))));
    assert_eq!(faults().await, 0, "a success resets the count");
    // Admitted before the trip; it finishes after it.
    rt.start("late", "workflow", json!({ "fail": true })).await.unwrap();
    for id in ["f2", "f3", "f4"] {
        assert!(matches!(run(id, true).await.unwrap(), Some(Status::Faulted(_))));
    }
    let d = rt.definitions("workflow").await.unwrap();
    assert_eq!(d.active, Some(bad.version), "the breaker never picks another version");
    let trip = d.tripped.expect("tripped at the configured threshold");
    assert_eq!((trip.faults, trip.threshold, trip.run.as_str()), (3, 3, "f4"));
    assert_eq!(rt.get("f0").await.unwrap().unwrap().admission.version, bad.version);
    assert!(matches!(rt.drive("late", &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(_))));
    assert_eq!(faults().await, 3, "a tripped breaker counts nothing more");
    assert_eq!(rt.definitions("workflow").await.unwrap().tripped.unwrap().run, "f4");
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
    save(&rt, &yaml("  - write: { call: write }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    let stop = CancellationToken::new();
    let shutdown = async {
        started.notified().await;
        stop.cancel();
    };
    let (result, ()) = tokio::join!(rt.drive("r", &stop), shutdown);
    assert!(matches!(result.unwrap(), Some(Status::Interrupted(_))));
    assert_eq!(rt.get("r").await.unwrap().unwrap().state, durable::RunState::Ready);
    // The journal tells the host the write may have happened.
    let calls = rt.calls("r").await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].key.as_str(), calls[0].function.as_str()), ("/do/0/write#call", "write"));
    assert_eq!((calls[0].delivery, calls[0].state), (Delivery::AtMostOnce, CallStatus::Unknown));
    let result = rt.drive("r", &CancellationToken::new()).await.unwrap();
    assert!(matches!(result,Some(Status::Faulted(e)) if e.type_ == CALL_UNKNOWN));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn events_resume_a_waiting_listen_and_cancel_is_terminal() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    save(&rt, &yaml("  - listen: { listen: { to: { one: { with: { type: answer } } }, read: envelope } }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let answer = |id: &str, data: Value| json!({"id": id, "source": "/test", "type": "answer", "data": data});
    assert_eq!(rt.deliver(answer("1", json!(42)), None).await.unwrap(), Some("r".into()));
    assert!(rt.deliver(answer("2", json!(0)), None).await.unwrap().is_none());
    assert!(
        matches!(rt.drive("r",&CancellationToken::new()).await.unwrap(),Some(Status::Completed(v)) if v[0]["data"]==42)
    );
    rt.start("cancelled", "workflow", Value::Null).await.unwrap();
    assert!(rt.cancel("cancelled").await.unwrap());
    assert!(!rt.cancel("cancelled").await.unwrap());
    assert_eq!(rt.drive("cancelled", &CancellationToken::new()).await.unwrap(), Some(Status::Cancelled));
}
#[tokio::test]
async fn child_runs_join_via_durable_events_with_one_worker_slot() {
    let store = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), Registry::new(), Arc::new(Time::default()));
    let child = rt.save("child", &yaml("  - result: { set: { answer: 42 } }")).await.unwrap();
    rt.activate("child", child.version).await.unwrap();
    save(
        &rt,
        &yaml(
            "  - child: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } }, read: envelope } }",
        ),
    ).await;
    rt.start("p", "workflow", json!({})).await.unwrap();
    assert!(matches!(rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    assert!(
        matches!(rt.get("p").await.unwrap().unwrap().outcome,Some(Status::Completed(v)) if v[0]["data"]["output"]["answer"]==42)
    );
    assert_eq!(store.list(Some("child"), "").await.unwrap().len(), 1);
}
#[tokio::test]
async fn retries_receive_stable_idempotency_keys_and_increasing_attempts() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let c = contexts.clone();
    let mut registry = Registry::new();
    registry
        .register(Function::new("read", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
            c.lock().unwrap().push((ctx.attempt, ctx.idempotency_key));
            async { Err::<Value, _>(WorkflowError::runtime("retry")) }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    save(
        &rt,
        &yaml(
            "  - attempts:\n      try: [ { read: { call: read } } ]\n      catch:\n        retry: { delay: { milliseconds: 0 }, limit: { attempt: { count: 3 } } }",
        ),
    ).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
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
    save(&rt, &yaml("  - read: { call: read }")).await;
    for n in 0..6 {
        rt.start(&format!("r{n}"), "workflow", Value::Null).await.unwrap();
    }
    let stop = CancellationToken::new();
    let token = stop.clone();
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(peak.load(Ordering::SeqCst) <= 2);
        rt.cancel("r0").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(rt.worker(stop), cancel);
    result.unwrap();
    assert_eq!(rt.get("r0").await.unwrap().unwrap().outcome, Some(Status::Cancelled));
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
        ("tightened", "  - post: { call: post, metadata: { ouro: { delivery: at_most_once } } }"),
    ] {
        let d = rt.save(name, &yaml(task)).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
        rt.start(name, name, json!({})).await.unwrap();
        let before = keys.lock().unwrap().len();
        interrupted(&rt, name, &keys, before + 1).await;
        if name == "default" {
            interrupted(&rt, name, &keys, before + 2).await;
            let sent = keys.lock().unwrap()[before..].to_vec();
            assert_eq!(sent[0], sent[1], "re-dispatched once more, under the same idempotency key");
        } else {
            let outcome = rt.drive(name, &CancellationToken::new()).await.unwrap();
            assert!(matches!(outcome, Some(Status::Faulted(e)) if e.type_ == CALL_UNKNOWN), "{name}");
            assert_eq!(keys.lock().unwrap().len(), before + 1, "{name}: never sent again");
        }
        let delivery = rt.records(name).await.unwrap().values().find_map(|r| match r {
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
    save(&one, &yaml("  - done: { set: { ok: true } }")).await;
    let run = one.start("r", "workflow", json!({})).await.unwrap();
    assert_eq!(run.admission.application_version(), Some("1"));
    let two = at(Some("2"), &[]);
    assert_eq!(two.drive("r", &CancellationToken::new()).await.unwrap(), None);
    assert_eq!(two.recover_once(&CancellationToken::new()).await.unwrap(), 0);
    let waiting = two.get("r").await.unwrap().unwrap();
    assert!(waiting.outcome.is_none() && !two.compatible(&waiting), "waiting for a compatible worker");
    let upgraded = at(Some("2"), &["1"]);
    assert!(upgraded.compatible(&waiting));
    assert_eq!(
        upgraded.drive("r", &CancellationToken::new()).await.unwrap(),
        Some(Status::Completed(json!({"ok":true})))
    );
    // Undeclared on either side: compatible, as before versions existed.
    let undeclared = at(None, &[]);
    undeclared.start("u", "workflow", json!({})).await.unwrap();
    assert_eq!(two.drive("u", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({"ok":true}))));
    one.start("v", "workflow", json!({})).await.unwrap();
    assert!(undeclared.compatible(&undeclared.get("v").await.unwrap().unwrap()));
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
    save(&rt, &yaml("  - write: { call: write }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Interrupted(_))));
    time.0.store(2000, Ordering::SeqCst);
    assert_eq!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!(42))));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(
        rt.records("r")
            .await
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
    save(&rt, &yaml("  - read: { call: read }")).await;
    rt.start("trace", "workflow", Value::Null).await.unwrap();
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
    let child = rt.save("child", &yaml("  - write: { call: write, with: { n: 42 } }")).await.unwrap();
    rt.activate("child", child.version).await.unwrap();
    let definition = yaml(
        "  - child: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } }, read: envelope } }",
    );
    let dry = rt.dry_run(&definition, json!({}), writes_mocked(), &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(dry.mocked.len(), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(rt.list(None, "").await.unwrap().is_empty());
    save(&rt, &definition).await;
    rt.start("p", "workflow", json!({})).await.unwrap();
    for _ in 0..3 {
        rt.recover_once(&CancellationToken::new()).await.unwrap();
    }
    let original = rt.get("p").await.unwrap().unwrap().outcome;
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
    save(&rt, &yaml("  - read: { call: read }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    let stop = CancellationToken::new();
    let mut old = Box::pin(rt.drive("r", &stop));
    assert!(futures::poll!(old.as_mut()).is_pending());
    time.0.store(durable::LEASE_MS + 1, Ordering::SeqCst);
    let mut current = Box::pin(rt.drive("r", &stop));
    assert!(futures::poll!(current.as_mut()).is_pending());
    drop(old);
    rt.cancel("r").await.unwrap();
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
                save(&rt, &definition).await;
                let before = backend.snapshot().await.unwrap().rows;
                let dry = rt
                    .dry_run(&definition, json!({"n":input}), writes_mocked(), &CancellationToken::new())
                    .await
                    .unwrap();
                prop_assert!(matches!(dry.status, Status::Completed(_)));
                prop_assert_eq!(writes.load(Ordering::SeqCst), 0);
                prop_assert_eq!(dry.mocked.len(), writes_in_program.iter().filter(|x| **x).count());
                prop_assert_eq!(backend.snapshot().await.unwrap().rows, before);
                rt.start("recorded", "workflow", json!({"n":input})).await.unwrap();
                let original = rt.drive("recorded", &CancellationToken::new()).await.unwrap();
                let before = backend.snapshot().await.unwrap().rows;
                let counts = (reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst));
                let replay = rt.replay("recorded", &CancellationToken::new()).await.unwrap();
                prop_assert_eq!(Some(replay.status), original);
                // Changed arguments must refuse instead of falling through to a handler.
                let changed = definition.replace(".n + 1", ".n + 2");
                let changed = rt.replay_definition(&changed, "recorded", &CancellationToken::new()).await;
                prop_assert!(changed.is_err() || matches!(changed.unwrap().status, Status::Faulted(_)));
                prop_assert_eq!((reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst)), counts);
                prop_assert_eq!(backend.snapshot().await.unwrap().rows, before);
                store.sweep(durable::RECORDS_KEPT_MS + 1, durable::RECORDS_KEPT_MS).await.unwrap();
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
    save(&rt, &definition).await;
    rt.start("deadline", "workflow", json!({})).await.unwrap();
    clock.0.store(2000, Ordering::SeqCst);
    let outcome = rt.drive("deadline", &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome,Some(Status::Faulted(e)) if e.is("timeout")), "admission deadline was restarted");
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn t1_ingress_identity_is_global_across_triggers() {
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Arc::new(Time::default()));
    let definition = yaml("  - done: { set: { ok: true } }")
        .replace("\ndo:", "\nschedule: { on: { one: { with: { type: event } } } }\ndo:");
    for name in ["first", "second"] {
        let version = rt.save(name, &definition).await.unwrap();
        rt.activate(name, version.version).await.unwrap();
    }
    let event = json!({"specversion":"1.0","source":"test","id":"one","type":"event","data":{}});
    let started = rt.publish(event.clone()).await.unwrap();
    assert_eq!(started.len(), 1, "one ingress event acquired multiple admission obligations");
    assert!(rt.publish(event).await.unwrap().is_empty());
}

#[tokio::test]
async fn t1_live_schedule_run_prevents_overlap() {
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), clock.clone());
    let definition = yaml("  - wait: { wait: PT1H }").replace("\ndo:", "\nschedule: { every: PT1S }\ndo:");
    save(&rt, &definition).await;
    clock.0.store(1000, Ordering::SeqCst);
    let started = rt.fire_due().await.unwrap();
    assert_eq!(started.len(), 1);
    assert!(matches!(rt.drive(&started[0], &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    clock.0.store(2000, Ordering::SeqCst);
    assert!(rt.fire_due().await.unwrap().is_empty(), "overlap admitted a second live run");
    assert_eq!(rt.list(None, "").await.unwrap().len(), 1);
}

// ── the start check: every function a run can call is registered ──────────

fn missing(e: &anyhow::Error) -> &MissingFunction {
    e.downcast_ref::<MissingFunction>().unwrap_or_else(|| panic!("not a missing function: {e:#}"))
}

/// A run admitted under one registry, resumed by a host whose release no
/// longer has `read`: new admissions fail with the typed error, and the
/// resumed run fails before its next step, never halfway through it.
#[tokio::test]
async fn a_function_removed_after_activation_stops_admission_and_the_resumed_run_before_its_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let (reads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let rt = runtime(
        Arc::new(durable::sqlite::open(&path).unwrap()),
        counter_registry(reads.clone(), writes.clone()),
        time.clone(),
    );
    save(
        &rt,
        &yaml("  - send: { call: write, with: {} }\n  - pause: { wait: PT1S }\n  - fetch: { call: read, with: {} }"),
    )
    .await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    drop(rt);
    // The next release registers `write` only.
    let mut release = Registry::new();
    let w = writes.clone();
    release
        .register(Function::new(
            "write",
            json!({"type":"object"}),
            json!({"type":"object"}),
            move |a: Value, _: FunctionContext| {
                w.fetch_add(1, Ordering::SeqCst);
                async move { Ok(a) }
            },
        ))
        .unwrap();
    time.0.store(1000, Ordering::SeqCst);
    let rt = runtime(Arc::new(durable::sqlite::open(&path).unwrap()), release, time);
    let e = rt.start("new", "workflow", json!({})).await.unwrap_err();
    assert_eq!(missing(&e), &MissingFunction { function: "read".into(), path: vec![("workflow".into(), 1)] });
    assert!(rt.get("new").await.unwrap().is_none(), "nothing was admitted");
    let outcome = rt.drive("r", &CancellationToken::new()).await.unwrap();
    let Some(Status::Faulted(error)) = outcome else { panic!("{outcome:?}") };
    assert!(error.is("configuration"), "{error:?}");
    assert!(error.detail.as_deref().is_some_and(|d| d.contains("`read`")), "{error:?}");
    assert_eq!(rt.get("r").await.unwrap().unwrap().outcome, Some(Status::Faulted(error)));
    assert_eq!((writes.load(Ordering::SeqCst), reads.load(Ordering::SeqCst)), (1, 0));
    // The journal shows where the run stopped: one completed call.
    let journal: Vec<_> = rt.calls("r").await.unwrap().into_iter().map(|c| (c.function, c.state)).collect();
    assert_eq!(journal, [("write".to_owned(), CallStatus::Completed)]);
}

/// The check is cached per definition version and registry generation:
/// replacing the registry invalidates it, in either direction.
#[tokio::test]
async fn replacing_the_registry_invalidates_the_start_check() {
    let (reads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let full = Arc::new(counter_registry(reads.clone(), writes.clone()));
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        full.clone(),
        Config::default(),
        Arc::new(Time::default()),
        tracer(),
    )
    .unwrap();
    save(&rt, &yaml("  - fetch: { call: read, with: {} }")).await;
    rt.start("before", "workflow", json!({})).await.unwrap();
    rt.set_registry(Arc::new(Registry::new()));
    assert_eq!(missing(&rt.start("without", "workflow", json!({})).await.unwrap_err()).function, "read");
    let outcome = rt.drive("before", &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, Some(Status::Faulted(ref e)) if e.is("configuration")), "{outcome:?}");
    rt.set_registry(full);
    rt.start("after", "workflow", json!({})).await.unwrap();
    assert_eq!(rt.drive("after", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({}))));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

/// A function missing in a child definition (`run: workflow`, through any
/// depth) fails the parent's admission and names the way to it; the child
/// `latest` names is resolved on every check, not cached.
#[tokio::test]
async fn a_missing_function_in_a_child_definition_fails_the_parent_before_it_starts() {
    let (reads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let full = Arc::new(counter_registry(reads.clone(), writes.clone()));
    let rt =
        Runtime::new(Arc::new(durable::memory::memory()), full, Config::default(), Arc::new(Time::default()), tracer())
            .unwrap();
    let child = async |name: &str, body: &str| {
        let d = rt.save(name, &yaml(body).replace("name: workflow", &format!("name: {name}"))).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    };
    child("leaf", "  - fetch: { call: read, with: {} }").await;
    child("middle", "  - leaf: { run: { workflow: { namespace: test, name: leaf, version: latest } } }").await;
    // A child that runs itself: the check visits it once.
    child(
        "workflow",
        "  - first: { set: { n: 1 } }\n  - middle: { run: { workflow: { namespace: test, name: middle, version: latest } } }\n  - again: { if: '${ false }', run: { workflow: { namespace: test, name: workflow, version: latest } } }",
    ).await;
    rt.start("ok", "workflow", json!({})).await.unwrap();
    let mut without_read = Registry::new();
    without_read
        .register(Function::new("write", json!({}), json!({}), |a: Value, _: FunctionContext| async move { Ok(a) }))
        .unwrap();
    rt.set_registry(Arc::new(without_read));
    let e = rt.start("parent", "workflow", json!({})).await.unwrap_err();
    assert_eq!(
        missing(&e),
        &MissingFunction {
            function: "read".into(),
            path: vec![("workflow".into(), 1), ("middle".into(), 1), ("leaf".into(), 1)],
        }
    );
    assert!(e.to_string().contains("called by workflow version 1 > middle version 1 > leaf version 1"), "{e}");
    // The parent admitted before the change fails before its first step.
    let outcome = rt.drive("ok", &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, Some(Status::Faulted(ref e)) if e.is("configuration")), "{outcome:?}");
    assert!(rt.records("ok").await.unwrap().values().all(|r| !matches!(r, Record::Task { .. })), "no step ran");
    // A new `leaf` version without the call: the parent resolves it anew.
    child("leaf", "  - done: { set: { leaf: true } }").await;
    rt.start("later", "workflow", json!({})).await.unwrap();
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
// The worker hands a trip's notice on by itself, polled beside the runs.
#[tokio::test]
async fn worker_hands_a_breaker_trip_to_the_event_sink() {
    struct Sink(tokio::sync::mpsc::UnboundedSender<Value>);
    #[async_trait::async_trait]
    impl ouro_runtime::EventSink for Sink {
        async fn emit(&self, event: &Value) -> Result<(), ouro_runtime::FunctionError> {
            self.0.send(event.clone()).ok();
            Ok(())
        }
    }
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let config = Config { breaker_threshold: 1, poll_interval: Duration::from_millis(5), ..Config::default() };
    let store = Arc::new(durable::memory::memory());
    let rt = Runtime::new(store, Arc::new(Registry::new()), config, Arc::new(Time::default()), tracer())
        .unwrap()
        .with_event_sink(Arc::new(Sink(sent)));
    save(&rt, &yaml("  - fail: { raise: { error: { type: 'urn:test:failure', status: 500 } } }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    let stop = CancellationToken::new();
    let watch = async {
        let event = tokio::time::timeout(Duration::from_secs(10), received.recv()).await;
        stop.cancel();
        event
    };
    let (result, event) = tokio::join!(rt.worker(stop.clone()), watch);
    result.unwrap();
    let event = event.expect("the worker handed the notice on").unwrap();
    assert_eq!(event["type"], durable::TRIPPED);
    assert_eq!((event["data"]["run"].as_str(), event["data"]["faults"].as_u64()), (Some("r"), Some(1)));
    assert_eq!(rt.notify(&CancellationToken::new()).await.unwrap(), 0, "acknowledged by the worker");
}

// ── a trigger's refused admission is a per-definition outcome ──────────────

/// `healthy` and `broken` fire every second; the release the worker runs no
/// longer registers `read`, which `broken` calls. The worker keeps firing
/// `healthy`; each `broken` slot is consumed with a run that ended faulted
/// at admission, which counts toward its breaker; the third trips it, its
/// schedule stops and the trip's notice reaches the sink.
#[tokio::test]
async fn a_scheduled_definition_the_host_cannot_admit_trips_without_stopping_the_others() {
    struct Sink(tokio::sync::mpsc::UnboundedSender<Value>);
    #[async_trait::async_trait]
    impl ouro_runtime::EventSink for Sink {
        async fn emit(&self, event: &Value) -> Result<(), ouro_runtime::FunctionError> {
            self.0.send(event.clone()).ok();
            Ok(())
        }
    }
    let (reads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let time = Arc::new(Time::default());
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let config = Config { poll_interval: Duration::from_millis(5), ..Config::default() };
    let rt = Runtime::new(
        Arc::new(durable::memory::memory()),
        Arc::new(counter_registry(reads.clone(), writes)),
        config,
        time.clone(),
        tracer(),
    )
    .unwrap()
    .with_event_sink(Arc::new(Sink(sent)));
    let every = async |name: &str, body: &str| {
        let definition = yaml(body)
            .replace("name: workflow", &format!("name: {name}"))
            .replace("\ndo:", "\nschedule: { every: PT1S }\ndo:");
        let d = rt.save(name, &definition).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    };
    every("healthy", "  - ok: { set: { ok: true } }").await;
    every("broken", "  - fetch: { call: read, with: {} }").await;
    rt.set_registry(Arc::new(Registry::new()));
    let stop = CancellationToken::new();
    // One second per few ticks, until the trip is handed on, and three
    // seconds more.
    let drive = async {
        let mut trip = None;
        for second in 1..=20 {
            time.0.store(second * 1000, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            if trip.is_none() {
                trip = received.try_recv().ok().map(|event| (second, event));
            } else if trip.as_ref().is_some_and(|(at, _)| second >= at + 3) {
                break;
            }
        }
        stop.cancel();
        trip
    };
    let (result, trip) = tokio::join!(rt.worker(stop.clone()), drive);
    result.expect("a refused admission does not stop the worker");
    let (tripped_at, event) = trip.expect("the trip's notice reached the sink");
    assert_eq!((event["type"].as_str(), event["subject"].as_str()), (Some(durable::TRIPPED), Some("broken")));
    let d = rt.definitions("broken").await.unwrap();
    let trip = d.tripped.expect("tripped at the threshold");
    assert_eq!((trip.faults, trip.threshold), (3, 3));
    assert!(trip.reason.is("configuration"), "{:?}", trip.reason);
    assert_eq!(event["data"]["run"].as_str(), Some(trip.run.as_str()));
    assert!(rt.triggers("broken").await.unwrap().is_empty(), "its schedule stops");
    // Exactly the three refused slots, each a run that ended faulted at its
    // admission, with nothing executed.
    let broken = rt.list(Some("broken"), "").await.unwrap();
    assert_eq!(broken.len(), 3, "{broken:?}");
    for run in &broken {
        assert!(run.admission.id.starts_with("schedule:broken:every:"), "{}", run.admission.id);
        let Some(Status::Faulted(error)) = &run.outcome else { panic!("{run:?}") };
        assert!(
            error.is("configuration") && error.detail.as_deref().is_some_and(|d| d.contains("`read`")),
            "{error:?}"
        );
        assert_eq!(run.ended_ms, Some(run.admitted_ms), "ended in its admission's commit");
        assert!(rt.records(&run.admission.id).await.unwrap().is_empty());
    }
    assert_eq!(trip.run, broken.last().unwrap().admission.id);
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    // `healthy` kept firing, before and after the trip.
    let healthy = rt.list(Some("healthy"), "").await.unwrap();
    let last = |runs: &[durable::SavedRun]| runs.iter().map(|r| r.admitted_ms).max().unwrap_or_default();
    assert!(last(&healthy) >= (tripped_at + 2) * 1000, "{healthy:?}");
    assert!(healthy.iter().all(|r| !matches!(r.outcome, Some(Status::Faulted(_)))), "{healthy:?}");
    assert_eq!(rt.definitions("healthy").await.unwrap().consecutive_faults, 0);
}

/// The refused slot and its run commit together: a process that dies after
/// that commit and before its next tick leaves the slot consumed once, never
/// refired or lost. `after` has no run to wait for, so it is due one delay
/// after the refusal.
#[tokio::test]
async fn a_refused_slot_is_recorded_once_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let open = |registry: Registry| runtime(Arc::new(durable::sqlite::open(&path).unwrap()), registry, time.clone());
    let rt = open(counter_registry(Arc::default(), Arc::default()));
    for (name, schedule) in [("broken", "every: PT1S"), ("later", "after: PT10S")] {
        let definition = yaml("  - fetch: { call: read, with: {} }")
            .replace("name: workflow", &format!("name: {name}"))
            .replace("\ndo:", &format!("\nschedule: {{ {schedule} }}\ndo:"));
        let d = rt.save(name, &definition).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    }
    drop(rt);
    // The next release has no `read`.
    time.0.store(1000, Ordering::SeqCst);
    let rt = open(Registry::new());
    assert!(rt.fire_due().await.unwrap().is_empty(), "nothing admitted to run");
    let failed = async |rt: &Runtime, id: &str| {
        let run = rt.get(id).await.unwrap().unwrap_or_else(|| panic!("{id} not recorded"));
        matches!(run.outcome, Some(Status::Faulted(e)) if e.is("configuration"))
    };
    assert!(failed(&rt, "schedule:broken:every:1000").await);
    assert!(failed(&rt, "schedule:later:after:1:0000000001").await);
    let next = async |rt: &Runtime, name: &str| rt.triggers(name).await.unwrap()[0].1.at_ms;
    assert_eq!((next(&rt, "broken").await, next(&rt, "later").await), (Some(2000), Some(11_000)));
    drop(rt);
    // Restarted before the next tick, at the same instant: nothing again.
    let rt = open(Registry::new());
    assert!(rt.fire_due().await.unwrap().is_empty());
    let count = async |rt: &Runtime, name: &str| {
        (rt.list(Some(name), "").await.unwrap().len(), rt.definitions(name).await.unwrap().consecutive_faults)
    };
    assert_eq!((count(&rt, "broken").await, count(&rt, "later").await), ((1, 1), (1, 1)));
    time.0.store(2000, Ordering::SeqCst);
    rt.fire_due().await.unwrap();
    assert_eq!((count(&rt, "broken").await, count(&rt, "later").await), ((2, 2), (1, 1)));
    // Down until 11 s: `broken`'s missed slots run once, late (`catchup:
    // latest`), its third fault trips it; `later` refuses its second run.
    time.0.store(11_000, Ordering::SeqCst);
    rt.fire_due().await.unwrap();
    assert!(failed(&rt, "schedule:broken:every:3000").await && failed(&rt, "schedule:later:after:1:0000000002").await);
    assert_eq!((count(&rt, "broken").await, count(&rt, "later").await), ((3, 3), (2, 2)));
    let trip = rt.definitions("broken").await.unwrap().tripped.expect("tripped");
    assert_eq!(trip.run, "schedule:broken:every:3000");
    assert!(rt.triggers("broken").await.unwrap().is_empty());
    assert_eq!(next(&rt, "later").await, Some(21_000));
    // Another release brings `read` back; only an activation resumes `broken`.
    rt.set_registry(Arc::new(counter_registry(Arc::default(), Arc::default())));
    time.0.store(21_000, Ordering::SeqCst);
    assert_eq!(rt.fire_due().await.unwrap(), vec!["schedule:later:after:1:0000000003".to_string()]);
    rt.activate("broken", 1).await.unwrap();
    time.0.store(22_000, Ordering::SeqCst);
    assert_eq!(rt.fire_due().await.unwrap(), vec!["schedule:broken:every:22000".to_string()]);
}

/// A definition that no longer validates against the host (here a host whose
/// jump ceiling dropped below the one the definition declares) is refused
/// like a missing function, with a validation fault; so is the run an event
/// schedule (`on`) would start, whose event is consumed once.
#[tokio::test]
async fn invalid_definitions_and_event_schedules_are_refused_per_definition() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let time = Arc::new(Time::default());
    let rt = runtime(store.clone(), counter_registry(Arc::default(), Arc::default()), time.clone());
    let save_as = async |name: &str, document: &str| {
        let d = rt.save(name, document).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    };
    save_as(
        "bounded",
        "document: { dsl: '1.0.4', namespace: test, name: bounded, version: '1.0.0', metadata: { ouro: { limits: { max_jumps: 30 } } } }\nschedule: { every: PT1S }\ndo:\n  - ok: { set: { ok: true } }\n",
    ).await;
    save_as(
        "orders",
        &yaml("  - fetch: { call: read, with: {} }")
            .replace("name: workflow", "name: orders")
            .replace("\ndo:", "\nschedule: { on: { one: { with: { type: order } } } }\ndo:"),
    )
    .await;
    let mut config = Config::default();
    config.limits.max_jumps = 10;
    let lowered = Runtime::new(store, Arc::new(Registry::new()), config, time.clone(), tracer()).unwrap();
    time.0.store(1000, Ordering::SeqCst);
    assert!(lowered.fire_due().await.unwrap().is_empty());
    let run = lowered.get("schedule:bounded:every:1000").await.unwrap().expect("recorded");
    assert!(matches!(&run.outcome, Some(Status::Faulted(e)) if e.is("validation")), "{run:?}");
    assert_eq!(lowered.definitions("bounded").await.unwrap().consecutive_faults, 1);
    assert_eq!(lowered.triggers("bounded").await.unwrap()[0].1.at_ms, Some(2000));
    // `start` still answers its caller with the error and records nothing.
    let e = lowered.start("direct", "bounded", json!({})).await.unwrap_err();
    assert!(e.is::<ouro_runtime::InvalidDefinition>(), "{e:#}");
    assert!(lowered.get("direct").await.unwrap().is_none());
    let order = json!({ "id": "1", "source": "/shop", "type": "order", "data": { "n": 1 } });
    assert!(lowered.publish(order.clone()).await.unwrap().is_empty());
    let orders = lowered.list(Some("orders"), "").await.unwrap();
    assert_eq!(orders.len(), 1);
    assert!(matches!(&orders[0].outcome, Some(Status::Faulted(e)) if e.is("configuration")), "{orders:?}");
    assert_eq!(lowered.definitions("orders").await.unwrap().consecutive_faults, 1);
    assert!(lowered.publish(order).await.unwrap().is_empty());
    assert_eq!(lowered.list(Some("orders"), "").await.unwrap().len(), 1, "one event starts at most one run");
}

/// A schema resolver that fails is an interruption of the host, not a fault
/// of the definition: the slot is neither consumed nor counted toward the
/// breaker; its trigger keeps it and retries after a capped backoff, stored
/// with the trigger, while the other schedules go on. Once the resolver
/// answers, the kept slot runs.
#[tokio::test]
async fn a_failing_schema_resolver_retries_the_slot_without_consuming_or_counting_it() {
    struct Flaky(std::sync::atomic::AtomicBool);
    impl ouro_runtime::ResourceResolver for Flaky {
        fn resolve(&self, uri: &str) -> anyhow::Result<Value> {
            anyhow::ensure!(self.0.load(Ordering::SeqCst), "{uri} unreachable");
            Ok(json!({ "type": "object" }))
        }
    }
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let time = Arc::new(Time::default());
    let resolver = Arc::new(Flaky(std::sync::atomic::AtomicBool::new(true)));
    let open = || runtime(store.clone(), Registry::new(), time.clone()).with_resources(resolver.clone());
    let rt = open();
    let typed = "input: { schema: { format: json, resource: { endpoint: 'https://schemas.test/in.json' } } }\n";
    for (name, schedule, extra) in [
        ("typed", "every: PT1S", typed),
        ("healthy", "every: PT1S", ""),
        ("orders", "on: { one: { with: { type: order } } }", typed),
    ] {
        let definition = yaml("  - ok: { set: { ok: true } }")
            .replace("name: workflow", &format!("name: {name}"))
            .replace("\ndo:", &format!("\nschedule: {{ {schedule} }}\n{extra}do:"));
        let d = rt.save(name, &definition).await.unwrap();
        rt.activate(name, d.version).await.unwrap();
    }
    resolver.0.store(false, Ordering::SeqCst);
    let stop = CancellationToken::new();
    let tick = async |rt: &Runtime, at: i64| {
        time.0.store(at, Ordering::SeqCst);
        rt.fire_due().await.expect("an interrupted admission does not stop the scheduler")
    };
    let retry = async |rt: &Runtime| {
        let (_, t) = rt.triggers("typed").await.unwrap().remove(0);
        (t.at_ms, t.state["retry"]["slot"].as_i64(), t.state["retry"]["failures"].as_u64())
    };
    // Due at 1, 2, 4 s: 1 s doubling, the slot kept, nothing recorded or counted.
    for (at, next, failures) in [(1000, 2000, 1), (2000, 4000, 2), (4000, 8000, 3)] {
        for id in tick(&rt, at).await {
            assert!(id.starts_with("schedule:healthy:"), "{id}");
            rt.drive(&id, &stop).await.unwrap();
        }
        assert_eq!(retry(&rt).await, (Some(next), Some(1000), Some(failures)));
    }
    assert!(rt.list(Some("typed"), "").await.unwrap().is_empty(), "the slot is not consumed");
    let d = rt.definitions("typed").await.unwrap();
    assert_eq!((d.consecutive_faults, d.tripped), (0, None), "past the threshold of 3, still not counted");
    assert_eq!(rt.list(Some("healthy"), "").await.unwrap().len(), 3, "the others went on");
    // An `on` schedule: the event is not taken, the publisher hears why.
    let order = json!({ "id": "1", "source": "/shop", "type": "order" });
    let e = rt.publish(order.clone()).await.unwrap_err();
    assert!(e.is::<ouro_runtime::ResourceUnavailable>(), "{e:#}");
    assert!(rt.list(Some("orders"), "").await.unwrap().is_empty());
    assert_eq!(rt.definitions("orders").await.unwrap().consecutive_faults, 0);
    // The backoff is stored with the trigger: a restart does not shorten it.
    drop(rt);
    let rt = open();
    tick(&rt, 5000).await;
    assert_eq!(retry(&rt).await.0, Some(8000));
    // The resolver answers again: the kept slot runs, late, once.
    resolver.0.store(true, Ordering::SeqCst);
    assert!(tick(&rt, 8000).await.contains(&"schedule:typed:every:1000".to_string()));
    assert_eq!(retry(&rt).await, (Some(9000), None, None));
    assert_eq!(
        rt.drive("schedule:typed:every:1000", &stop).await.unwrap(),
        Some(Status::Completed(json!({ "ok": true })))
    );
    assert_eq!(rt.publish(order).await.unwrap().len(), 1, "republished, the event starts its run");
    // The worker's hand-off of a trip notice to such an `on` schedule keeps
    // the notice, after its backoff, until the resolver answers.
    let watch = yaml("  - ok: { set: { ok: true } }").replace("name: workflow", "name: watch").replace(
        "\ndo:",
        &format!("\nschedule: {{ on: {{ one: {{ with: {{ type: ouro.definition.tripped }} }} }} }}\n{typed}do:"),
    );
    let d = rt.save("watch", &watch).await.unwrap();
    rt.activate("watch", d.version).await.unwrap();
    let boom = yaml("  - boom: { raise: { error: { type: 'urn:test:boom', status: 500 } } }")
        .replace("name: workflow", "name: boom");
    let d = rt.save("boom", &boom).await.unwrap();
    rt.activate("boom", d.version).await.unwrap();
    for n in 0..3 {
        rt.start(&format!("boom{n}"), "boom", json!({})).await.unwrap();
        rt.drive(&format!("boom{n}"), &stop).await.unwrap();
    }
    assert!(rt.definitions("boom").await.unwrap().tripped.is_some());
    resolver.0.store(false, Ordering::SeqCst);
    assert_eq!(rt.notify(&stop).await.unwrap(), 0, "kept");
    assert!(rt.list(Some("watch"), "").await.unwrap().is_empty());
    resolver.0.store(true, Ordering::SeqCst);
    assert_eq!(rt.notify(&stop).await.unwrap(), 0, "still backing off");
    time.0.store(9000, Ordering::SeqCst);
    assert_eq!(rt.notify(&stop).await.unwrap(), 1);
    assert_eq!(rt.list(Some("watch"), "").await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn independent_store_cancel_fires_in_flight_token_within_one_heartbeat() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel.sqlite");
    let store = Arc::new(durable::sqlite::open(&path).unwrap());
    let entered = Arc::new(tokio::sync::Notify::new());
    let token = Arc::new(Mutex::new(None));
    let mut registry = Registry::new();
    let observed = token.clone();
    let ready = entered.clone();
    registry
        .register(Function::new("hold", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
            *observed.lock().unwrap() = Some(ctx.cancel.clone());
            ready.notify_one();
            async move { std::future::pending::<Result<Value, _>>().await }
        }))
        .unwrap();
    let heartbeat = Duration::from_secs(1);
    let rt = Runtime::new(
        store,
        Arc::new(registry),
        Config { heartbeat_interval: heartbeat, ..Config::default() },
        Arc::new(Time::default()),
        tracer(),
    )
    .unwrap();
    save(&rt, &yaml("  - hold: { call: hold }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    let remote =
        ouro_runtime::control::Control::new(Arc::new(durable::sqlite::open(path).unwrap()), Arc::new(Time::default()));
    let stop = CancellationToken::new();
    let drive = rt.drive("r", &stop);
    let cancel = async {
        entered.notified().await;
        let token = token.lock().unwrap().clone().unwrap();
        assert!(!token.is_cancelled());
        remote.cancel(&operator("remote-cancel"), "r").await.unwrap();
        let started = tokio::time::Instant::now();
        tokio::time::timeout(heartbeat + Duration::from_millis(1), token.cancelled()).await.unwrap();
        assert!(started.elapsed() <= heartbeat);
    };
    let (result, ()) = tokio::join!(drive, cancel);
    assert_eq!(result.unwrap(), Some(Status::Cancelled));
    assert_eq!(rt.calls("r").await.unwrap()[0].state, CallStatus::Pending);
}

fn operator(id: &str) -> ouro_runtime::control::Request {
    ouro_runtime::control::Request { id: id.into(), actor: "operator".into(), reason: "review".into() }
}

#[tokio::test]
async fn control_lists_a_one_shot_with_its_instant_until_it_fires() {
    use ouro_runtime::control::{Control, ScheduleKind};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("definitions.sqlite");
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::sqlite::open(&path).unwrap()), Registry::new(), clock.clone());
    let definition = yaml("  - done: { set: { ok: true } }")
        .replace("version: '1.0.0'", "version: '1.0.0', metadata: { ouro: { at: '1970-01-01T00:00:05Z' } }");
    save(&rt, &definition).await;
    let control = Control::new(Arc::new(durable::sqlite::open(&path).unwrap()), clock.clone());
    let due = |summary: ouro_runtime::control::DefinitionSummary| {
        summary.schedules.iter().map(|s| (s.kind, s.next_due_ms)).collect::<Vec<_>>()
    };
    assert_eq!(due(control.definition("workflow").await.unwrap().unwrap()), [(ScheduleKind::At, Some(5000))]);
    clock.0.store(5000, Ordering::SeqCst);
    assert_eq!(rt.fire_due().await.unwrap(), ["schedule:workflow:at:5000"]);
    assert_eq!(due(control.definition("workflow").await.unwrap().unwrap()), [(ScheduleKind::At, None)]);
}

#[tokio::test]
async fn store_only_control_lists_definition_schedules_and_suspended_slots() {
    use ouro_runtime::control::{Control, ControlError, ScheduleCatchup, ScheduleKind};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("definitions.sqlite");
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::sqlite::open(&path).unwrap()), Registry::new(), clock.clone());
    let definition = yaml("  - wait: { wait: PT1H }")
        .replace("version: '1.0.0'", "version: '1.0.0', metadata: { ouro: { overlap: queue } }")
        .replace("\ndo:", "\nschedule: { every: PT1S, after: PT2S, on: { one: { with: { type: start } } } }\ndo:");
    save(&rt, &definition).await;
    let control = Control::new(Arc::new(durable::sqlite::open(&path).unwrap()), clock.clone());
    assert!(control.definition("missing").await.unwrap().is_none());
    let summary = control.definition("workflow").await.unwrap().unwrap();
    assert_eq!((summary.active, summary.epoch), (Some(1), 1));
    assert_eq!(summary.versions[0].version, 1);
    // Activated at the runtime's clock, unattributed.
    let first = &summary.versions[0];
    assert_eq!((first.activated_ms, first.activated_epoch, first.activated_by.as_deref()), (Some(0), Some(1), None));
    assert_eq!(
        summary.schedules.iter().map(|s| s.kind).collect::<Vec<_>>(),
        [ScheduleKind::Every, ScheduleKind::After, ScheduleKind::On]
    );
    assert_eq!(summary.schedules.iter().map(|s| s.next_due_ms).collect::<Vec<_>>(), [Some(1000), Some(0), None]);
    assert!(
        summary.schedules.iter().all(|s| s.overlap == durable::Overlap::Queue && s.catchup == ScheduleCatchup::Latest)
    );
    assert_eq!(control.definitions(None, 1).await.unwrap().items, [summary]);
    // The after slot starts immediately. Every then queues behind it at 1000.
    assert_eq!(rt.fire_due().await.unwrap().len(), 1);
    clock.0.store(1000, Ordering::SeqCst);
    assert_eq!(rt.fire_due().await.unwrap().len(), 1);
    clock.0.store(2000, Ordering::SeqCst);
    rt.fire_due().await.unwrap();
    let before_pause = control.definition("workflow").await.unwrap().unwrap();
    assert_eq!(before_pause.schedules[0].queued.as_ref().unwrap().run, "schedule:workflow:every:2000");
    control.pause(&operator("pause-definitions"), "workflow", "inspect schedule").await.unwrap();
    let paused = control.definition("workflow").await.unwrap().unwrap();
    assert_eq!(paused.schedules, before_pause.schedules);
    assert_eq!(paused.tripped.as_ref().unwrap().manual.as_ref().unwrap().note, "inspect schedule");
    assert_eq!(control.definitions(None, 10).await.unwrap().items, [paused]);
    control.resume(&operator("resume-definitions"), "workflow").await.unwrap();
    assert_eq!(control.definition("workflow").await.unwrap(), Some(before_pause));
    // Reactivation replaces the schedules and discards the buffered slot.
    let replacement = rt.save("workflow", &yaml("  - ok: { set: { ok: true } }")).await.unwrap();
    assert!(rt.activate_as("workflow", replacement.version, " ", None).await.is_err());
    rt.activate_as("workflow", replacement.version, "operator", Some("drop the schedule")).await.unwrap();
    let summary = control.definition("workflow").await.unwrap().unwrap();
    assert_eq!((summary.active, summary.epoch, summary.versions.len()), (Some(2), 2, 2));
    let second = &summary.versions[1];
    assert_eq!((second.activated_ms, second.activated_epoch), (Some(2000), Some(2)));
    assert_eq!(
        (second.activated_by.as_deref(), second.activation_note.as_deref()),
        (Some("operator"), Some("drop the schedule"))
    );
    assert_eq!(summary.versions[0].activated_epoch, Some(1), "the first version keeps its latest activation");
    assert!(summary.schedules.is_empty());
    let page = control.definitions(None, 1).await.unwrap();
    assert_eq!(page.items, [summary]);
    assert!(control.definitions(page.next.as_ref(), 1).await.unwrap().items.is_empty());
    assert!(matches!(control.definitions(None, 0).await, Err(ControlError::InvalidInput(_))));
    assert!(matches!(control.definitions(None, 1001).await, Err(ControlError::InvalidInput(_))));
}

#[tokio::test]
async fn read_only_control_answers_queries_and_refuses_commands() {
    use ouro_runtime::control::{Control, ControlError, Runs, Waits};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-only.sqlite");
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::sqlite::open(&path).unwrap()), Registry::new(), clock.clone());
    save(&rt, &yaml("  - wait: { listen: { to: { one: { with: { type: answer } } } } }")).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    let writable = Control::new(Arc::new(durable::sqlite::open(&path).unwrap()), clock.clone());
    let control = Control::new(Arc::new(durable::sqlite::open_read_only(&path).unwrap()), clock.clone());
    assert_eq!(control.inspect("r").await.unwrap(), writable.inspect("r").await.unwrap());
    assert_eq!(
        control.list(&Runs::default(), None, 10).await.unwrap(),
        writable.list(&Runs::default(), None, 10).await.unwrap()
    );
    assert_eq!(control.definitions(None, 10).await.unwrap(), writable.definitions(None, 10).await.unwrap());
    let waits = control.waits(&Waits::default(), None, 10).await.unwrap().items;
    assert_eq!(waits.len(), 1);
    let answer = json!({"source":"/operator","id":"1","type":"answer","data":{}});
    let refused = control.respond(&operator("answer"), "r", &waits[0].key, answer).await;
    assert!(matches!(refused, Err(ControlError::ReadOnly)), "{refused:?}");
    assert!(matches!(control.cancel(&operator("cancel"), "r").await, Err(ControlError::ReadOnly)));
    assert!(matches!(control.emergency_stop(&operator("stop"), true).await, Err(ControlError::ReadOnly)));
    assert!(!control.stopped().await.unwrap());
    assert!(control.journal(None, None, 10).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn store_only_control_answers_a_multi_event_listen_with_persisted_correlation() {
    use ouro_runtime::control::{Control, ControlError, Runs, Waits};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sqlite");
    let clock = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::sqlite::open(&path).unwrap()), Registry::new(), clock.clone());
    save(
        &rt,
        &yaml(
            "  - decision:\n      timeout: { after: PT1S }\n      listen:\n        to:\n          all:\n            - with: { type: first }\n              correlate: { ticket: { from: .ticket } }\n            - with: { type: second }\n              correlate: { ticket: { from: .ticket } }",
        ),
    ).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    let stop = CancellationToken::new();
    assert!(matches!(rt.drive("r", &stop).await.unwrap(), Some(Status::Waiting { .. })));
    let control = Control::new(Arc::new(durable::sqlite::open(path).unwrap()), clock.clone());
    let waits = control.waits(&Waits::default(), None, 10).await.unwrap().items;
    assert_eq!(waits.len(), 1);
    let wait = &waits[0];
    assert_eq!(wait.waiting.types, ["first", "second"]);
    assert_eq!(wait.waiting.timeout_ms, Some(1000));
    assert_eq!(wait.waiting.since_ms, 0);
    let event =
        |id: &str, type_: &str, ticket| json!({"source":"/operator","id":id,"type":type_,"ticket":ticket,"data":{}});
    control.respond(&operator("first"), "r", &wait.key, event("1", "first", 42)).await.unwrap();
    assert!(matches!(rt.drive("r", &stop).await.unwrap(), Some(Status::Waiting { .. })));
    let wait = control.waits(&Waits::default(), None, 10).await.unwrap().items.pop().unwrap();
    assert_eq!(wait.waiting.progress.expectations["ticket"], 42);
    let filled = control.respond(&operator("filled"), "r", &wait.key, event("filled", "first", 42)).await;
    assert!(matches!(filled, Err(ControlError::WaitMismatch { .. })), "{filled:?}");
    let wrong = control.respond(&operator("wrong"), "r", &wait.key, event("wrong", "second", 43)).await;
    assert!(matches!(wrong, Err(ControlError::WaitMismatch { .. })), "{wrong:?}");
    let reused = control.respond(&operator("first"), "r", &wait.key, event("other", "second", 42)).await;
    assert!(matches!(reused, Err(ControlError::RequestConflict { .. })), "{reused:?}");
    let unbounded = control.history(&Runs::default(), None, 10).await;
    assert!(matches!(unbounded, Err(ControlError::InvalidInput(_))), "{unbounded:?}");
    let anywhere = control.counts(&Runs::default(), false, None, 10).await;
    assert!(matches!(anywhere, Err(ControlError::InvalidInput(_))), "{anywhere:?}");
    let answer = event("2", "second", 42);
    control.respond(&operator("second"), "r", &wait.key, answer.clone()).await.unwrap();
    assert!(matches!(rt.drive("r", &stop).await.unwrap(), Some(Status::Completed(_))));
    assert!(control.waits(&Waits::default(), None, 10).await.unwrap().items.is_empty());
    control.respond(&operator("retry"), "r", &wait.key, answer).await.unwrap();
    let inspection = control.inspect("r").await.unwrap().unwrap();
    assert!(inspection.calls.is_empty() && inspection.waits.is_empty());
    assert_eq!(inspection.snapshot.run.state, durable::RunState::Completed);
    let filter = Runs { definition: Some("workflow".into()), ..Runs::default() };
    assert_eq!(control.counts(&filter, false, None, 10).await.unwrap().states["completed"], 1);
    let filter = Runs { since_ms: Some(0), before_ms: Some(1000), ..filter };
    assert_eq!(control.counts(&filter, true, None, 10).await.unwrap().states["completed"], 1);
    assert_eq!(control.history(&filter, None, 10).await.unwrap().items.len(), 1);
    assert_eq!(control.journal(Some("r"), None, 10).await.unwrap().items.len(), 3);
}

#[tokio::test]
async fn control_respond_supports_wildcard_listens_and_refuses_expired_waits() {
    use ouro_runtime::control::{Control, ControlError, Waits};
    let store = Arc::new(durable::memory::memory());
    let clock = Arc::new(Time::default());
    let rt = runtime(store.clone(), Registry::new(), clock.clone());
    save(&rt, &yaml("  - decision: { timeout: { after: PT1S }, listen: { to: { any: [] } } }")).await;
    let control = Control::new(store, clock.clone());
    let stop = CancellationToken::new();
    for id in ["answered", "expired"] {
        rt.start(id, "workflow", json!({})).await.unwrap();
        assert!(matches!(rt.drive(id, &stop).await.unwrap(), Some(Status::Waiting { .. })));
    }
    let wait = control
        .waits(&Waits { run: Some("answered".into()), ..Waits::default() }, None, 10)
        .await
        .unwrap()
        .items
        .pop()
        .unwrap();
    assert_eq!(wait.waiting.types, ["*"]);
    let event = json!({"source":"/operator","id":"answer","type":"choice","data":42});
    control.respond(&operator("answer"), "answered", &wait.key, event.clone()).await.unwrap();
    assert_eq!(rt.drive("answered", &stop).await.unwrap(), Some(Status::Completed(json!([42]))));
    let wait = control
        .waits(&Waits { run: Some("expired".into()), ..Waits::default() }, None, 10)
        .await
        .unwrap()
        .items
        .pop()
        .unwrap();
    clock.0.store(1000, Ordering::SeqCst);
    let late = control.respond(&operator("late"), "expired", &wait.key, event).await;
    assert!(matches!(late, Err(ControlError::WaitClosed { .. })), "{late:?}");
    assert!(matches!(rt.drive("expired", &stop).await.unwrap(), Some(Status::Faulted(_))));
    assert!(control.waits(&Waits::default(), None, 10).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn emergency_stop_preserves_delivery_at_the_next_step_boundary_and_resumes() {
    use ouro_runtime::control::{Command, Control};
    for delivery in [Delivery::AtLeastOnce, Delivery::AtMostOnce] {
        let store = Arc::new(durable::memory::memory());
        let clock = Arc::new(Time::default());
        let control = Arc::new(Control::new(store.clone(), clock.clone()));
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        let stop = control.clone();
        let calls = first.clone();
        registry
            .register(
                Function::new("stop", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                    let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                    let stop = stop.clone();
                    async move {
                        if first {
                            stop.execute(&operator("stop"), &Command::Stop { enabled: true }).await.unwrap();
                        }
                        Ok(json!({}))
                    }
                })
                .with_delivery(delivery),
            )
            .unwrap();
        let calls = second.clone();
        registry
            .register(Function::new("next", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({})) }
            }))
            .unwrap();
        let rt = runtime(store, registry, clock);
        save(&rt, &yaml("  - stop: { call: stop }\n  - next: { call: next }")).await;
        rt.start("r", "workflow", json!({})).await.unwrap();
        let token = CancellationToken::new();
        assert!(matches!(rt.drive("r", &token).await.unwrap(), Some(Status::Interrupted(_))));
        assert_eq!(second.load(Ordering::SeqCst), 0);
        assert_eq!(rt.get("r").await.unwrap().unwrap().state, durable::RunState::Ready);
        assert!(rt.start("blocked", "workflow", json!({})).await.unwrap_err().is::<ouro_runtime::control::Stopped>());
        assert_eq!(rt.recover_once(&token).await.unwrap(), 0);
        control.emergency_stop(&operator("clear"), false).await.unwrap();
        let outcome = rt.drive("r", &token).await.unwrap();
        if delivery == Delivery::AtLeastOnce {
            assert!(matches!(outcome, Some(Status::Completed(_))));
            assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (2, 1));
        } else {
            assert!(matches!(outcome, Some(Status::Faulted(_))));
            assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (1, 0));
            assert_eq!(rt.calls("r").await.unwrap()[0].state, CallStatus::Unknown);
        }
    }
}

#[tokio::test]
async fn attributed_in_process_cancel_fires_the_token_synchronously() {
    use ouro_runtime::control::{Command, Reply};
    let entered = Arc::new(tokio::sync::Notify::new());
    let token = Arc::new(Mutex::new(None));
    let mut registry = Registry::new();
    let observed = token.clone();
    let ready = entered.clone();
    registry
        .register(Function::new("hold", json!({}), json!({}), move |_: Value, ctx: FunctionContext| {
            *observed.lock().unwrap() = Some(ctx.cancel.clone());
            ready.notify_one();
            async move {
                ctx.cancel.cancelled().await;
                Ok(json!({}))
            }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Arc::new(Time::default()));
    save(&rt, &yaml("  - hold: { call: hold }")).await;
    rt.start("r", "workflow", Value::Null).await.unwrap();
    let stop = CancellationToken::new();
    let cancel = async {
        entered.notified().await;
        assert_eq!(
            rt.control(&operator("cancel"), &Command::Cancel { run: "r".into() }).await.unwrap(),
            Reply::Changed(true)
        );
        assert!(token.lock().unwrap().as_ref().unwrap().is_cancelled());
    };
    let (result, ()) = tokio::join!(rt.drive("r", &stop), cancel);
    assert_eq!(result.unwrap(), Some(Status::Cancelled));
}

#[tokio::test]
async fn operator_answers_respect_enclosing_workflow_and_task_deadlines() {
    use ouro_runtime::control::{Control, Waits};
    for workflow_timeout in [false, true] {
        let store = Arc::new(durable::memory::memory());
        let clock = Arc::new(Time::default());
        let rt = runtime(store.clone(), Registry::new(), clock.clone());
        let definition = if workflow_timeout {
            yaml("  - decision: { listen: { to: { any: [] } } }").replace("\ndo:", "\ntimeout: { after: PT1S }\ndo:")
        } else {
            yaml(
                "  - enclosing:\n      timeout: { after: PT1S }\n      do:\n        - decision: { listen: { to: { any: [] } } }",
            )
        };
        save(&rt, &definition).await;
        rt.start("r", "workflow", Value::Null).await.unwrap();
        assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
        let control = Control::new(store, clock.clone());
        let wait = control.waits(&Waits::default(), None, 10).await.unwrap().items.pop().unwrap();
        assert_eq!(wait.waiting.timeout_ms, Some(1000));
        clock.0.store(1000, Ordering::SeqCst);
        assert!(
            control
                .respond(&operator("late"), "r", &wait.key, json!({"source":"/operator", "id":"late", "type":"answer"}))
                .await
                .is_err()
        );
    }
}

// ── async seams on a current-thread runtime (#7) ───────────────────────────

/// `write` is at most once and counted; `hold` never answers on the first
/// host (`held`: it signals and waits) and answers at once on the second;
/// `meet` waits until three calls meet, so it completes only when three runs
/// are in flight together.
fn crash_registry(writes: Arc<AtomicUsize>, held: Option<Arc<tokio::sync::Notify>>) -> Registry {
    let mut registry = Registry::new();
    registry
        .register(
            Function::new("write", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                writes.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({})) }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    registry
        .register(Function::new("hold", json!({}), json!({}), move |_: Value, _: FunctionContext| {
            let held = held.clone();
            async move {
                if let Some(held) = held {
                    held.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(json!({}))
            }
        }))
        .unwrap();
    let meeting = Arc::new(tokio::sync::Barrier::new(3));
    registry
        .register(Function::new("meet", json!({}), json!({}), move |_: Value, _: FunctionContext| {
            let meeting = meeting.clone();
            async move {
                meeting.wait().await;
                Ok(json!({}))
            }
        }))
        .unwrap();
    registry
}

/// One thread runs everything. A worker dies mid-run: its `drive` future is
/// dropped with a call in flight, so nothing releases or renews the lease. The
/// lease then simply ends: before its expiry no other worker can claim the
/// run, after it another runtime over the same database resumes it from its
/// records without sending the at-most-once write again. Then three runs make
/// progress together under one worker on that same thread.
fn current_thread_crash_and_resume(open: impl Fn() -> Arc<dyn RunStore>) {
    let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    executor.block_on(async {
        let writes = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(tokio::sync::Notify::new());
        let clock = Arc::new(Time::default());
        let first = runtime(open(), crash_registry(writes.clone(), Some(held.clone())), clock.clone());
        save(&first, &yaml("  - write: { call: write }\n  - hold: { call: hold }\n  - done: { set: { ok: true } }"))
            .await;
        first.start("r", "workflow", json!({})).await.unwrap();
        let crashed = CancellationToken::new();
        tokio::select! {
            _ = first.drive("r", &crashed) => panic!("the held run completed"),
            _ = held.notified() => {}
        }
        drop(first);
        let lease = first_lease(&open, "r").await;
        assert_eq!(lease, durable::LEASE_MS, "the crashed worker's lease from its claim at 0");
        // Nothing renews a dropped run's lease, however long its heartbeat waits.
        clock.0.store(durable::LEASE_MS - 1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(first_lease(&open, "r").await, lease);
        let second = runtime(open(), crash_registry(writes.clone(), None), clock.clone());
        assert_eq!(second.drive("r", &CancellationToken::new()).await.unwrap(), None, "claimed before its lease ended");
        clock.0.store(durable::LEASE_MS, Ordering::SeqCst);
        assert_eq!(second.recover_once(&CancellationToken::new()).await.unwrap(), 1);
        let run = second.get("r").await.unwrap().unwrap();
        assert_eq!(run.outcome, Some(Status::Completed(json!({"ok": true}))));
        assert_eq!(writes.load(Ordering::SeqCst), 1, "the at-most-once write was sent again");

        let d =
            second.save("meeting", &yaml("  - meet: { call: meet }").replace("name: workflow", "name: meeting")).await;
        second.activate("meeting", d.unwrap().version).await.unwrap();
        for n in 0..3 {
            second.start(&format!("m{n}"), "meeting", json!({})).await.unwrap();
        }
        let stop = CancellationToken::new();
        let met = async {
            for n in 0..3 {
                while second.get(&format!("m{n}")).await.unwrap().unwrap().outcome.is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            stop.cancel();
        };
        let (worker, ()) =
            tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(second.worker(stop.clone()), met) })
                .await
                .expect("three runs did not progress together on one thread");
        worker.unwrap();
    });
}
async fn first_lease(open: &impl Fn() -> Arc<dyn RunStore>, id: &str) -> i64 {
    open().get(id).await.unwrap().unwrap().lease_until_ms.expect("a running run holds a lease")
}
#[test]
fn memory_runs_crash_and_resume_on_a_current_thread_runtime() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    current_thread_crash_and_resume(|| store.clone());
}
#[test]
fn sqlite_runs_crash_and_resume_on_a_current_thread_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    // Each runtime opens its own connection to the same file, as a restarted
    // process would.
    current_thread_crash_and_resume(|| Arc::new(durable::sqlite::open(&path).unwrap()));
}

#[tokio::test]
async fn a_run_started_at_a_version_keeps_it_without_activating_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pinned.sqlite");
    let time = Arc::new(Time::default());
    let open = || {
        let counted = counter_registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        runtime(Arc::new(durable::sqlite::open(&path).unwrap()), counted, time.clone())
    };
    let rt = open();
    let first =
        rt.save("workflow", &yaml("  - wait: { wait: PT1S }\n  - done: { set: { result: first } }")).await.unwrap();
    let second = rt.save("workflow", &yaml("  - done: { set: { result: second } }")).await.unwrap();
    rt.activate("workflow", second.version).await.unwrap();
    let before = rt.definitions("workflow").await.unwrap();

    let saved = rt.start_at("p", "workflow", first.version, json!({})).await.unwrap();
    assert_eq!(saved.admission.version, first.version);
    assert_eq!(rt.definitions("workflow").await.unwrap(), before, "starting a version activates nothing");
    let control = ouro_runtime::control::Control::new(Arc::new(durable::sqlite::open(&path).unwrap()), time.clone());
    let inspection = control.inspect("p").await.unwrap().unwrap();
    assert!(inspection.pinned);
    assert_eq!(inspection.snapshot.run.admission.version, first.version);
    // An inspection a host stored without the optional members still reads.
    let mut stored = serde_json::to_value(&inspection).unwrap();
    stored.as_object_mut().unwrap().retain(|k, _| k != "pinned" && k != "digest");
    let read: ouro_runtime::control::Inspection = serde_json::from_value(stored).unwrap();
    assert_eq!((read.pinned, read.digest.as_str()), (false, ""));
    // Starting again returns the run; another version or definition is refused.
    assert_eq!(rt.start_at("p", "workflow", first.version, json!({})).await.unwrap().admission, saved.admission);
    assert!(rt.start_at("p", "workflow", second.version, json!({})).await.is_err());
    assert!(rt.start_at("q", "workflow", 99, json!({})).await.is_err(), "an unknown version");
    // The ordinary start follows the active version and is not pinned.
    rt.start("a", "workflow", json!({})).await.unwrap();
    assert!(!control.inspect("a").await.unwrap().unwrap().pinned);
    assert_eq!(rt.get("a").await.unwrap().unwrap().admission.version, second.version);
    // `start_at` returns an existing run of that name and version, pinned or not.
    assert_eq!(rt.start_at("a", "workflow", second.version, json!({})).await.unwrap().admission.id, "a");
    assert!(rt.start_at("a", "workflow", first.version, json!({})).await.is_err());

    // A restart resumes the pinned run at its own version.
    assert!(matches!(rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    drop(rt);
    time.0.store(1000, Ordering::SeqCst);
    let rt = open();
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    assert_eq!(rt.get("p").await.unwrap().unwrap().outcome, Some(Status::Completed(json!({"result":"first"}))));
}
#[tokio::test]
async fn a_pinned_start_is_checked_like_any_start_and_a_trip_refuses_only_its_content() {
    let store = Arc::new(durable::memory::memory());
    let time: Arc<dyn Clock> = Arc::new(Time::default());
    let counted = counter_registry(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let unregistered = runtime(store.clone(), counted, time.clone())
        .save("workflow", &yaml("  - r: { call: read, with: {} }"))
        .await
        .unwrap();
    let rt = runtime(store, Registry::new(), time);
    let e = rt.start_at("m", "workflow", unregistered.version, json!({})).await.unwrap_err();
    assert_eq!(missing(&e), &MissingFunction { function: "read".into(), path: vec![("workflow".into(), 1)] });
    assert!(rt.get("m").await.unwrap().is_none());

    let good = rt.save("workflow", &yaml("  - ok: { set: { result: ok } }")).await.unwrap();
    let bad = rt
        .save("workflow", &yaml("  - fail: { raise: { error: { type: 'urn:test:failure', status: 500 } } }"))
        .await
        .unwrap();
    rt.activate("workflow", bad.version).await.unwrap();
    let stop = CancellationToken::new();
    for id in ["f0", "f1", "f2"] {
        rt.start(id, "workflow", json!({})).await.unwrap();
        rt.drive(id, &stop).await.unwrap();
    }
    assert!(rt.definitions("workflow").await.unwrap().tripped.is_some());
    // The tripped content is refused by any path and version number; other
    // content runs pinned, uncounted, while the tripped version stays active.
    let copy = rt
        .save("workflow", &yaml("  - fail: { raise: { error: { type: 'urn:test:failure', status: 500 } } }"))
        .await
        .unwrap();
    for refused in [
        rt.start_at("c", "workflow", copy.version, json!({})).await,
        rt.start("n", "workflow", json!({})).await.map(|_| unreachable!()),
    ] {
        assert!(refused.unwrap_err().is::<durable::DefinitionTripped>());
    }
    let before = rt.definitions("workflow").await.unwrap();
    rt.start_at("p", "workflow", good.version, json!({})).await.unwrap();
    assert_eq!(rt.drive("p", &stop).await.unwrap(), Some(Status::Completed(json!({"result":"ok"}))));
    rt.start_at("p2", "workflow", good.version, json!({})).await.unwrap();
    assert_eq!(rt.definitions("workflow").await.unwrap(), before, "a pinned run changes nothing of the trip");
    assert_eq!(
        before.versions[&good.version].digest,
        rt.definitions("workflow").await.unwrap().versions[&good.version].digest
    );
    assert_eq!(before.versions[&bad.version].digest, before.versions[&copy.version].digest);
    // An activation of even the identical content clears the trip.
    rt.activate("workflow", copy.version).await.unwrap();
    rt.start_at("q", "workflow", bad.version, json!({})).await.unwrap();
}

#[tokio::test]
async fn a_hold_stops_the_run_at_the_next_step_records_the_answer_in_flight_and_release_continues() {
    use ouro_runtime::control::Control;
    for delivery in [Delivery::AtLeastOnce, Delivery::AtMostOnce] {
        let store = Arc::new(durable::memory::memory());
        let clock = Arc::new(Time::default());
        let control = Arc::new(Control::new(store.clone(), clock.clone()));
        let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut registry = Registry::new();
        let (hold, calls) = (control.clone(), first.clone());
        registry
            .register(
                Function::new("hold", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                    let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                    let hold = hold.clone();
                    async move {
                        // The operator holds the run while its call is out.
                        if first {
                            hold.hold(&operator("hold"), "r", "while it runs").await.unwrap();
                        }
                        Ok(json!({}))
                    }
                })
                .with_delivery(delivery),
            )
            .unwrap();
        let calls = second.clone();
        registry
            .register(Function::new("next", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({})) }
            }))
            .unwrap();
        let rt = runtime(store.clone(), registry, clock);
        save(&rt, &yaml("  - hold: { call: hold }\n  - next: { call: next }")).await;
        rt.start("r", "workflow", json!({})).await.unwrap();
        let token = CancellationToken::new();
        assert!(matches!(rt.drive("r", &token).await.unwrap(), Some(Status::Interrupted(_))));
        // The step after the answer never ran; the answer was recorded.
        assert_eq!(second.load(Ordering::SeqCst), 0);
        let held = rt.get("r").await.unwrap().unwrap();
        assert_eq!(held.state, durable::RunState::Ready);
        assert_eq!(held.held.as_ref().map(|h| h.note.as_str()), Some("while it runs"));
        assert_eq!(rt.calls("r").await.unwrap()[0].state, CallStatus::Completed);
        // Not due, not claimed, and a counted state of its own.
        assert_eq!(rt.recover_once(&token).await.unwrap(), 0);
        assert_eq!(rt.drive("r", &token).await.unwrap(), None);
        let filter = ouro_runtime::control::Runs { definition: Some("workflow".into()), ..Default::default() };
        assert_eq!(control.counts(&filter, false, None, 10).await.unwrap().states["held"], 1);
        assert_eq!(control.inspect("r").await.unwrap().unwrap().snapshot.run.held, held.held);
        // The same request repeats its reply; a second hold changes nothing.
        let again = control.hold(&operator("again"), "r", "x").await.unwrap();
        assert_eq!(again, ouro_runtime::control::Reply::Changed(false));
        control.release(&operator("release"), "r").await.unwrap();
        assert_eq!(rt.drive("r", &token).await.unwrap(), Some(Status::Completed(json!({}))));
        // Neither step repeated: the recorded answer stood, whatever the delivery.
        assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (1, 1));
    }
}

// A hold never changes a run's outcome: a worker that sees it at its lease
// renewal keeps the lease (a held run's renewal still extends it) until the
// call it already dispatched answers, however long that takes, records the
// answer, and only then lets the run go. Released, the run goes on from that
// answer; nothing is sent twice, whatever the delivery.
#[tokio::test]
async fn a_hold_waits_for_the_answer_of_a_call_in_flight_however_long_it_takes() {
    use ouro_runtime::control::Control;
    for delivery in [Delivery::AtLeastOnce, Delivery::AtMostOnce] {
        let store = Arc::new(durable::memory::memory());
        let clock = Arc::new(Time::default());
        let control = Arc::new(Control::new(store.clone(), clock.clone()));
        let (started, answer) = (Arc::new(tokio::sync::Notify::new()), Arc::new(tokio::sync::Notify::new()));
        let (slow, next) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut registry = Registry::new();
        let (signal, release, count) = (started.clone(), answer.clone(), slow.clone());
        registry
            .register(
                Function::new("slow", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                    // The first dispatch answers only when the test says so.
                    let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                    let (signal, release) = (signal.clone(), release.clone());
                    async move {
                        if first {
                            signal.notify_one();
                            release.notified().await;
                        }
                        Ok(json!({ "slow": true }))
                    }
                })
                .with_delivery(delivery),
            )
            .unwrap();
        let count = next.clone();
        registry
            .register(Function::new("next", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Ok(json!({})) }
            }))
            .unwrap();
        let rt = Runtime::new(
            store,
            Arc::new(registry),
            Config { heartbeat_interval: Duration::from_millis(20), ..Config::default() },
            clock.clone(),
            tracer(),
        )
        .unwrap();
        save(&rt, &yaml("  - slow: { call: slow }\n  - next: { call: next }")).await;
        rt.start("r", "workflow", json!({})).await.unwrap();
        let token = CancellationToken::new();
        let operate = async {
            started.notified().await;
            control.hold(&operator("hold"), "r", "busy").await.unwrap();
            // The call outlives many heartbeats and twice the lease it had
            // when the hold came.
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(60)).await;
                clock.0.fetch_add(durable::LEASE_MS / 2, Ordering::SeqCst);
            }
            answer.notify_one();
        };
        let (driven, ()) = tokio::join!(rt.drive("r", &token), operate);
        assert!(matches!(driven.unwrap(), Some(Status::Interrupted(_))), "{delivery:?}");
        let held = rt.get("r").await.unwrap().unwrap();
        assert!(held.held.is_some() && held.state == durable::RunState::Ready && held.fence.is_none());
        assert_eq!(
            rt.calls("r").await.unwrap()[0].state,
            CallStatus::Completed,
            "{delivery:?}: the answer was recorded"
        );
        assert_eq!(next.load(Ordering::SeqCst), 0, "the step after the answer waits for the release");
        assert_eq!(held.interruptions, 0, "a hold spends no interruption");
        control.release(&operator("release"), "r").await.unwrap();
        assert_eq!(rt.drive("r", &token).await.unwrap(), Some(Status::Completed(json!({}))), "{delivery:?}");
        assert_eq!((slow.load(Ordering::SeqCst), next.load(Ordering::SeqCst)), (1, 1), "{delivery:?}");
    }
}

// Waiting for a call in flight never keeps a run from the worker's shutdown:
// a stopped worker lets it go like any run, and the call keeps its delivery
// rules (an at-most-once call without an answer has an unknown outcome).
#[tokio::test]
async fn a_held_run_waiting_for_its_call_is_let_go_at_shutdown() {
    use ouro_runtime::control::Control;
    let store = Arc::new(durable::memory::memory());
    let clock = Arc::new(Time::default());
    let control = Arc::new(Control::new(store.clone(), clock.clone()));
    let started = Arc::new(tokio::sync::Notify::new());
    let mut registry = Registry::new();
    let signal = started.clone();
    registry
        .register(
            Function::new("slow", json!({}), json!({}), move |_: Value, _: FunctionContext| {
                let signal = signal.clone();
                async move {
                    signal.notify_one();
                    std::future::pending::<()>().await;
                    Ok(json!({}))
                }
            })
            .with_delivery(Delivery::AtMostOnce),
        )
        .unwrap();
    let rt = Runtime::new(
        store,
        Arc::new(registry),
        Config { heartbeat_interval: Duration::from_millis(20), ..Config::default() },
        clock,
        tracer(),
    )
    .unwrap();
    save(&rt, &yaml("  - slow: { call: slow }")).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    let token = CancellationToken::new();
    let operate = async {
        started.notified().await;
        control.hold(&operator("hold"), "r", "busy").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    };
    let (driven, ()) = tokio::join!(rt.drive("r", &token), operate);
    assert!(matches!(driven.unwrap(), Some(Status::Interrupted(_))));
    let held = rt.get("r").await.unwrap().unwrap();
    assert!(held.held.is_some() && held.state == durable::RunState::Ready && held.fence.is_none());
    assert_eq!(rt.calls("r").await.unwrap()[0].state, CallStatus::Unknown);
}

/// A memory backend whose first writing transaction after `arm` waits at its
/// commit until `release`: the window between deciding and committing.
#[derive(Default)]
struct Gated {
    inner: durable::memory::MemoryBackend,
    armed: std::sync::atomic::AtomicBool,
    parked: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct GatedTransaction<'a> {
    inner: Box<dyn durable::Transaction + 'a>,
    gate: &'a Gated,
    writes: bool,
}
#[async_trait::async_trait]
impl durable::AtomicBackend for Gated {
    async fn begin(&self) -> anyhow::Result<Box<dyn durable::Transaction + '_>> {
        Ok(Box::new(GatedTransaction { inner: self.inner.begin().await?, gate: self, writes: false }))
    }
    async fn snapshot(&self) -> anyhow::Result<durable::Snapshot> {
        self.inner.snapshot().await
    }
}
#[async_trait::async_trait]
impl durable::Transaction for GatedTransaction<'_> {
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
        self.writes = true;
        self.inner.put(kind, id, value).await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.writes = true;
        self.inner.delete(kind, id).await
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        if self.writes && self.gate.armed.swap(false, Ordering::SeqCst) {
            self.gate.parked.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.commit().await
    }
}

// Two submissions of one new request race: the store decides which one is
// the repeat in the command's own commit, so the event is reported once.
#[tokio::test]
async fn concurrent_submissions_of_one_hold_request_report_it_once() {
    use ouro_runtime::observer::{EventKind, Observer};
    struct Seen(Mutex<Vec<EventKind>>);
    impl Observer for Seen {
        fn observe(&self, event: &ouro_runtime::observer::Event) {
            if matches!(event.kind, EventKind::RunHeld { .. }) {
                self.0.lock().unwrap().push(event.kind.clone());
            }
        }
    }
    let gate = Arc::new(Gated::default());
    let store = Arc::new(durable::TransactionalStore::new(gate.clone()));
    let seen = Arc::new(Seen(Mutex::default()));
    let rt = Runtime::new(store, Arc::new(Registry::new()), Config::default(), Arc::new(Time::default()), tracer())
        .unwrap()
        .with_observer(seen.clone());
    save(&rt, &yaml("  - done: { set: { ok: true } }")).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    let hold = operator("hold");
    let command = ouro_runtime::control::Command::Hold { run: "r".into(), note: "n".into() };
    gate.armed.store(true, Ordering::SeqCst);
    let first = rt.control(&hold, &command);
    let second = async {
        gate.parked.notified().await;
        let reply = rt.control(&hold, &command).await;
        gate.release.notify_one();
        reply
    };
    let (first, second) = tokio::join!(first, second);
    let changed = ouro_runtime::control::Reply::Changed(true);
    assert_eq!((first.unwrap(), second.unwrap()), (changed.clone(), changed));
    assert_eq!(seen.0.lock().unwrap().len(), 1, "one request, one event");
}

#[tokio::test]
async fn a_hold_and_a_release_are_reported_once_and_the_pause_of_a_definition_refuses_every_version() {
    use ouro_runtime::{
        control::{Control, Reply},
        observer::{EventKind, Observer},
    };
    struct Seen(Mutex<Vec<(String, EventKind)>>);
    impl Observer for Seen {
        fn observe(&self, event: &ouro_runtime::observer::Event) {
            self.0.lock().unwrap().push((event.run_id.clone(), event.kind.clone()));
        }
    }
    let store = Arc::new(durable::memory::memory());
    let clock = Arc::new(Time::default());
    let seen = Arc::new(Seen(Mutex::default()));
    let rt = Runtime::new(store.clone(), Arc::new(Registry::new()), Config::default(), clock.clone(), tracer())
        .unwrap()
        .with_observer(seen.clone());
    let control = Control::new(store, clock);
    save(&rt, &yaml("  - done: { set: { ok: true } }")).await;
    rt.start("r", "workflow", json!({})).await.unwrap();
    let (hold, release) = (operator("hold"), operator("release"));
    assert_eq!(
        rt.control(&hold, &ouro_runtime::control::Command::Hold { run: "r".into(), note: "n".into() }).await.unwrap(),
        Reply::Changed(true)
    );
    // A retry of the committed request repeats the reply, not the event.
    rt.control(&hold, &ouro_runtime::control::Command::Hold { run: "r".into(), note: "n".into() }).await.unwrap();
    rt.control(&operator("again"), &ouro_runtime::control::Command::Hold { run: "r".into(), note: "n".into() })
        .await
        .unwrap();
    rt.control(&release, &ouro_runtime::control::Command::Release { run: "r".into() }).await.unwrap();
    let events: Vec<_> = seen
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, k)| matches!(k, EventKind::RunHeld { .. } | EventKind::RunReleased { .. }))
        .cloned()
        .collect();
    assert_eq!(
        events,
        [
            (
                "r".to_owned(),
                EventKind::RunHeld { actor: "operator".into(), reason: "review".into(), note: "n".into() }
            ),
            ("r".to_owned(), EventKind::RunReleased { actor: "operator".into(), reason: "review".into() }),
        ]
    );
    // A manual pause refuses every version, pinned or not.
    let other = rt.save("workflow", &yaml("  - other: { set: { ok: false } }")).await.unwrap();
    control.pause(&operator("pause"), "workflow", "look").await.unwrap();
    for refused in [
        rt.start_at("p", "workflow", other.version, json!({})).await,
        rt.start_at("q", "workflow", 1, json!({})).await,
        rt.start("s", "workflow", json!({})).await.map(|_| unreachable!()),
    ] {
        assert!(refused.unwrap_err().downcast_ref::<durable::DefinitionTripped>().is_some_and(|t| t.paused));
    }
    control.resume(&operator("resume"), "workflow").await.unwrap();
    rt.start_at("p", "workflow", other.version, json!({})).await.unwrap();
}
