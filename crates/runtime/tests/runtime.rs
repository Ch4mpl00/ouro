use ouro_runtime::{
    Config, Effect, Function, FunctionContext, Registry, Runtime,
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
fn counter_registry(reads: Arc<AtomicUsize>, writes: Arc<AtomicUsize>) -> Registry {
    let mut registry = Registry::new();
    registry
        .register(Function::new(
            "read",
            json!({"type":"object"}),
            json!({"type":"object"}),
            Effect::Read,
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
            Effect::Write,
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
    registry.register(Function::new("typed",json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}},"additionalProperties":false}),json!({"type":"integer"}),Effect::Read,move |_:Value,_:FunctionContext| {c.fetch_add(1,Ordering::SeqCst);async {Err(WorkflowError {type_:"urn:test:rejected".into(),status:409,title:Some("Rejected".into()),detail:None,instance:None})}})).unwrap();
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
async fn dry_run_reads_are_real_writes_are_mocked_and_replay_calls_no_handlers() {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let rt = runtime(
        Arc::new(durable::memory::memory()),
        counter_registry(reads.clone(), writes.clone()),
        Arc::new(Time::default()),
    );
    let definition =
        yaml("  - read: { call: read, with: { n: 1 } }\n  - write: { call: write, with: { n: '${ .n + 1 }' } }");
    let dry = rt
        .dry_run(
            &definition,
            json!({}),
            Arc::new(|_: &str, a: &Value, _: &FunctionContext| Ok(a.clone())),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(dry.status, Status::Completed(json!({"n":2})));
    assert_eq!(dry.writes.len(), 1);
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
        .register(Function::new("write", json!({}), json!({}), Effect::Write, move |_: Value, ctx: FunctionContext| {
            w.fetch_add(1, Ordering::SeqCst);
            n.notify_one();
            async move {
                ctx.cancel.cancelled().await;
                Ok(Value::Null)
            }
        }))
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
    assert_eq!(rt.deliver("answer", json!(42)).unwrap(), Some("r".into()));
    assert!(rt.deliver("answer", json!(0)).unwrap().is_none());
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
        .register(Function::new("read", json!({}), json!({}), Effect::Read, move |_: Value, ctx: FunctionContext| {
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
        .register(Function::new("read", json!({}), json!({}), Effect::Read, move |_: Value, ctx: FunctionContext| {
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
#[tokio::test]
async fn write_response_survives_crash_before_task_checkpoint() {
    let backend = Arc::new(durable::memory::MemoryBackend::default());
    let store = Arc::new(durable::TransactionalStore::new(backend.clone()));
    let writes = Arc::new(AtomicUsize::new(0));
    let b = backend.clone();
    let w = writes.clone();
    let mut registry = Registry::new();
    registry
        .register(Function::new("write", json!({}), json!({}), Effect::Write, move |_: Value, _: FunctionContext| {
            w.fetch_add(1, Ordering::SeqCst);
            let b = b.clone();
            async move {
                b.faults.arm(durable::CommitFault::AfterCommit);
                Ok(json!(42))
            }
        }))
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
        .register(Function::new("read", json!({}), json!({}), Effect::Read, move |_: Value, c: FunctionContext| {
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
    for key in ["ouro.task.path", "ouro.effect", "ouro.attempt"] {
        assert!(function.attributes.iter().any(|a| a.key.as_str() == key));
    }
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
    let dry = rt
        .dry_run(
            &definition,
            json!({}),
            Arc::new(|_: &str, a: &Value, _: &FunctionContext| Ok(a.clone())),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(dry.writes.len(), 1);
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
        .register(Function::new(
            "read",
            json!({}),
            json!({}),
            Effect::Read,
            |_: Value, ctx: FunctionContext| async move {
                ctx.cancel.cancelled().await;
                Ok(Value::Null)
            },
        ))
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
