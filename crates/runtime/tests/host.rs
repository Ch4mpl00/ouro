//! Host data at admission and host-started children (design 03): the host
//! vouches for a run's facts, which no workflow can set, and a host function
//! starts owned children it vouches for.
use ouro_runtime::{
    ChildError, ChildRef, ChildRefusal, ChildStart, Config, Function, FunctionContext, FunctionError, HostDataTooLarge,
    Registry, Runtime, Start,
    control::{Control, REDACTED, Request, Runs},
    durable::{self, AdmissionField, AdmissionMismatch, Clock, RunStore},
    ows::run::Status,
};
use serde_json::{Value, json};
use sha2::Digest;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
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
    fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
fn tracer() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::BoxedTracer::new(Box::new(opentelemetry::trace::noop::NoopTracer::new()))
}
fn doc(name: &str, body: &str) -> String {
    format!("document: {{ dsl: '1.0.4', namespace: test, name: {name}, version: '1.0.0' }}\n{body}\n")
}
fn runtime(store: Arc<dyn RunStore>, registry: Registry, config: Config, clock: Arc<Time>) -> Runtime {
    Runtime::new(store, Arc::new(registry), config, clock, tracer()).unwrap()
}
async fn save(rt: &Runtime, name: &str, yaml: &str) -> u64 {
    let d = rt.save(name, yaml).await.unwrap();
    rt.activate(name, d.version).await.unwrap();
    d.version
}
fn operator(id: &str) -> Request {
    Request { id: id.into(), actor: "operator".into(), reason: "review".into() }
}
/// Drives due runs until `id` ends, moving the clock past interruption
/// backoffs when nothing is due.
async fn settle(rt: &Runtime, time: &Time, id: &str) -> Option<Status> {
    for _ in 0..60 {
        if let Some(status) = rt.get(id).await.unwrap().and_then(|r| r.outcome) {
            return Some(status);
        }
        if rt.recover_once(&CancellationToken::new()).await.unwrap() == 0 {
            time.advance(600_000);
        }
    }
    rt.get(id).await.unwrap().and_then(|r| r.outcome)
}

/// What each call of `whoami` saw: its run and its host data.
type Seen = Arc<Mutex<Vec<(String, Option<Value>)>>>;
fn whoami(seen: Seen) -> Function {
    Function::new("whoami", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
        seen.lock().unwrap().push((cx.run_id.clone(), cx.host().cloned()));
        async { Ok(json!({})) }
    })
}

/// How each `spawn` call's child start ended.
type Starts = Arc<Mutex<Vec<Result<ChildRef, String>>>>;
fn kind(error: &ChildError) -> String {
    match error {
        ChildError::Refused(ChildRefusal::Unavailable) => "unavailable".into(),
        ChildError::Refused(ChildRefusal::HostData(HostDataTooLarge { .. })) => "host-data".into(),
        ChildError::Refused(ChildRefusal::Depth(_)) => "depth".into(),
        ChildError::Refused(ChildRefusal::Tripped(t)) if t.paused => "paused".into(),
        ChildError::Refused(ChildRefusal::Tripped(_)) => "tripped".into(),
        ChildError::Refused(ChildRefusal::Mismatch(m)) => format!("mismatch:{:?}", m.field),
        ChildError::Refused(ChildRefusal::Inadmissible(_)) => "inadmissible".into(),
        ChildError::Interrupted(_) => "interrupted".into(),
    }
}
/// The child a `spawn` call starts by default: the definition its arguments
/// name (`name`, optional `version` and `key`), and the parent's host data
/// with the parent's id added, which the host vouches for.
fn vouched(args: &Value, cx: &FunctionContext) -> ChildStart {
    ChildStart {
        key: args["key"].as_str().unwrap_or("k").into(),
        name: args["name"].as_str().unwrap_or_default().into(),
        version: args["version"].as_u64(),
        input: json!({}),
        host: json!({ "parent": cx.run_id, "of": cx.host().cloned() }),
    }
}
/// A host function that starts the child `build` describes and answers its
/// `ChildRef`; a refusal answers `not_executed` and an interruption
/// `interrupted`, through `ChildError`'s conversion.
fn spawner(starts: Starts, build: impl Fn(&Value, &FunctionContext) -> ChildStart + Send + Sync + 'static) -> Function {
    let build = Arc::new(build);
    Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
        let (starts, build) = (starts.clone(), build.clone());
        async move {
            let result = cx.start_child(build(&args, &cx)).await;
            starts.lock().unwrap().push(result.as_ref().map(Clone::clone).map_err(kind));
            Ok(serde_json::to_value(result?).unwrap())
        }
    })
}
fn mismatch(error: anyhow::Error) -> Option<AdmissionField> {
    error.downcast_ref::<AdmissionMismatch>().map(|m| m.field)
}
async fn host_children(rt: &Runtime) -> Vec<durable::SavedRun> {
    rt.list(None, "hostchild:").await.unwrap()
}

const CHILD: &str = "do:\n  - who: { call: whoami }\n  - done: { set: { answer: 42 } }";
/// Starts a host child, joins it and reads its outcome.
const PARENT: &str = "do:
  - spawn: { call: spawn, with: { name: child }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }
  - outcome: { call: child_outcome, with: { id: '${ $context.id }', key: '${ $context.key }' } }";

#[tokio::test]
async fn host_data_reaches_functions_survives_a_restart_and_stays_out_of_expressions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.sqlite");
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let open = || {
        let mut registry = Registry::new();
        registry.register(whoami(seen.clone())).unwrap();
        runtime(Arc::new(durable::sqlite::open(&path).unwrap()), registry, Config::default(), time.clone())
    };
    let rt = open();
    let body = "do:
  - before: { call: whoami }
  - pause: { wait: PT1S }
  - after: { call: whoami }
  - out: { set: { runtime: '${ $runtime }', input: '${ $input }' } }";
    save(&rt, "workflow", &doc("workflow", body)).await;
    let host = json!({ "tenant": "t-1", "user": "u-7" });
    let saved =
        rt.start_with("r", "workflow", json!({ "n": 1 }), Start { version: None, host: host.clone() }).await.unwrap();
    assert_eq!(saved.admission.host(), Some(&host));
    assert_eq!(saved.admission.input, json!({ "n": 1 }), "host data is never part of the input");
    assert!(matches!(rt.drive("r", &CancellationToken::new()).await.unwrap(), Some(Status::Waiting { .. })));
    drop(rt);
    // Another process over the same file reads the same host data.
    time.advance(1_000);
    let rt = open();
    rt.recover_once(&CancellationToken::new()).await.unwrap();
    let Some(Status::Completed(out)) = rt.get("r").await.unwrap().unwrap().outcome else { panic!("not completed") };
    assert_eq!(*seen.lock().unwrap(), [("r".into(), Some(host.clone())), ("r".into(), Some(host.clone()))]);
    // Expressions never see it: `$runtime` is a whitelist, `$input` the input.
    assert!(out["runtime"].get("host").is_none(), "{out}");
    assert!(!out.to_string().contains("t-1"), "{out}");
    // A run started without host data has none.
    rt.start("plain", "workflow", json!({})).await.unwrap();
    rt.drive("plain", &CancellationToken::new()).await.unwrap();
    assert_eq!(seen.lock().unwrap().last(), Some(&("plain".to_owned(), None)));
}

#[tokio::test]
async fn start_with_joins_an_equal_admission_and_refuses_another() {
    let time = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Config::default(), time.clone());
    let first = save(&rt, "workflow", &doc("workflow", "do:\n  - a: { set: { v: 1 } }")).await;
    let second = save(&rt, "workflow", &doc("workflow", "do:\n  - a: { set: { v: 2 } }")).await;
    save(&rt, "other", &doc("other", "do:\n  - a: { set: { v: 3 } }")).await;
    let host = json!({ "tenant": "a" });
    let with = |host: &Value| Start { version: None, host: host.clone() };
    let saved = rt.start_with("r", "workflow", json!({}), with(&host)).await.unwrap();
    assert_eq!(saved.admission.version, second);
    // The same admission joins, with any input: the run is returned as stored.
    assert_eq!(rt.start_with("r", "workflow", json!({ "x": 1 }), with(&host)).await.unwrap(), saved);
    // Other host data, none, or another definition is refused, typed.
    let refused = rt.start_with("r", "workflow", json!({}), with(&json!({ "tenant": "b" }))).await.unwrap_err();
    assert_eq!(mismatch(refused), Some(AdmissionField::Host));
    assert_eq!(mismatch(rt.start("r", "workflow", json!({})).await.unwrap_err()), Some(AdmissionField::Host));
    assert_eq!(
        mismatch(rt.start_with("r", "other", json!({}), with(&host)).await.unwrap_err()),
        Some(AdmissionField::Name)
    );
    // A pinned start names a version: another one is refused, the admitted one joins.
    let pinned = |version| Start { version: Some(version), host: host.clone() };
    assert_eq!(
        mismatch(rt.start_with("r", "workflow", json!({}), pinned(first)).await.unwrap_err()),
        Some(AdmissionField::Version)
    );
    assert_eq!(rt.start_with("r", "workflow", json!({}), pinned(second)).await.unwrap(), saved);
    // A run without host data joins only starts without host data.
    rt.start("plain", "workflow", json!({})).await.unwrap();
    assert_eq!(
        mismatch(rt.start_with("plain", "workflow", json!({}), with(&host)).await.unwrap_err()),
        Some(AdmissionField::Host)
    );
    assert!(rt.start("plain", "workflow", json!({})).await.unwrap().admission.host().is_none());
    assert!(rt.start_with("plain", "workflow", json!({}), Start::default()).await.is_ok());
    // A pinned start with host data keeps both.
    let p = rt.start_with("p", "workflow", json!({}), pinned(first)).await.unwrap();
    assert!(p.admission.pinned());
    assert_eq!((p.admission.version, p.admission.host()), (first, Some(&host)));
    // Nothing was overwritten.
    assert_eq!(rt.get("r").await.unwrap().unwrap().admission, saved.admission);
}

#[tokio::test]
async fn host_data_is_bounded_at_start() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry
        .register(spawner(starts.clone(), |args, cx| ChildStart {
            host: json!("x".repeat(durable::MAX_HOST_BYTES)),
            ..vouched(args, cx)
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
    save(&rt, "workflow", &doc("workflow", "do:\n  - spawn: { call: spawn, with: { name: workflow } }")).await;
    // The serialized value, quotes included, may not exceed the bound.
    let fits = json!("x".repeat(durable::MAX_HOST_BYTES - 2));
    rt.start_with("fits", "workflow", json!({}), Start { version: None, host: fits }).await.unwrap();
    let large = json!("x".repeat(durable::MAX_HOST_BYTES));
    let error = rt.start_with("large", "workflow", json!({}), Start { version: None, host: large }).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<HostDataTooLarge>(),
        Some(&HostDataTooLarge { bytes: durable::MAX_HOST_BYTES + 2 })
    );
    assert!(rt.get("large").await.unwrap().is_none(), "nothing is stored");
    // A host child with too much host data is refused before anything happens.
    let Some(Status::Faulted(e)) = rt.drive("fits", &CancellationToken::new()).await.unwrap() else { panic!() };
    assert_eq!(e.type_, ouro_runtime::CALL_NOT_EXECUTED, "{e:?}");
    assert_eq!(*starts.lock().unwrap(), [Err("host-data".to_owned())]);
    assert!(host_children(&rt).await.is_empty());
}

#[tokio::test]
async fn children_a_workflow_starts_get_no_host_data() {
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let mut registry = Registry::new();
    registry.register(whoami(seen.clone())).unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    let parent = "do:
  - who: { call: whoami }
  - native: { run: { workflow: { namespace: test, name: child, version: latest } } }
  - detached: { run: { workflow: { namespace: test, name: child, version: latest }, await: false } }
  - builtin: { call: start_child, with: { name: child, input: {} }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }";
    save(&rt, "parent", &doc("parent", parent)).await;
    let host = json!({ "tenant": "t-1" });
    rt.start_with("p", "parent", json!({}), Start { version: None, host: host.clone() }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    // The detached child is a root of its own: drive it too.
    while rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    let children: Vec<_> = rt.list(Some("child"), "").await.unwrap();
    assert_eq!(children.len(), 3, "a native, a detached and a built-in child");
    for child in &children {
        assert!(child.admission.host().is_none(), "{} inherited host data", child.admission.id);
        assert!(child.admission.runtime.get(durable::HOST).is_none());
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.iter().filter(|(_, host)| host.is_some()).collect::<Vec<_>>(), [&("p".to_owned(), Some(host))]);
    assert_eq!(seen.len(), 4, "the parent and three children called whoami");
}

#[tokio::test]
async fn a_host_function_starts_an_owned_child_it_vouches_for() {
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let starts = Starts::default();
    let keys = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(whoami(seen.clone())).unwrap();
    let k = keys.clone();
    registry
        .register(spawner(starts.clone(), move |args, cx| {
            k.lock().unwrap().push(cx.task_path.clone());
            vouched(args, cx)
        }))
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    // The child jumps twice: the tree's budget counts them.
    let child = "do:
  - who: { call: whoami }
  - step: { set: { k: '${ (.k // 0) + 1 }' } }
  - again: { switch: [ { stop: { when: '${ .k >= 3 }', then: exit } }, { more: { then: step } } ] }";
    save(&rt, "child", &doc("child", child)).await;
    save(&rt, "parent", &doc("parent", PARENT)).await;
    let host = json!({ "tenant": "t-1" });
    rt.start_with("p", "parent", json!({}), Start { version: None, host: host.clone() }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));

    // The documented derivation: the first 128 bits of the SHA-256 of the
    // JSON array [run, call reference, key], in lowercase hex.
    let reference = keys.lock().unwrap()[0].clone();
    let digest = sha2::Sha256::digest(serde_json::to_string(&["p", reference.as_str(), "k"]).unwrap().as_bytes());
    let id = format!("hostchild:{}", digest[..16].iter().map(|b| format!("{b:02x}")).collect::<String>());
    let started = starts.lock().unwrap()[0].clone().unwrap();
    assert_eq!(started.id, id);
    assert_eq!(started.key, "/do/0/spawn/k");
    assert_eq!((started.done.as_str(), started.waiting.as_str()), (&*format!("done:{id}"), &*format!("waiting:{id}")));
    let c = rt.get(&id).await.unwrap().unwrap();
    let vouched = json!({ "parent": "p", "of": host });
    assert_eq!(c.admission.host(), Some(&vouched));
    assert!(matches!(c.outcome, Some(Status::Completed(_))));
    // Owned by the parent, under its link key; in the parent's budget tree, one level deeper.
    let link = c.child.clone().unwrap();
    assert_eq!((link.parent.as_str(), link.key.as_str()), ("p", started.key.as_str()));
    assert_eq!(c.root.as_deref(), Some("p"));
    assert_eq!(c.admission.runtime["limits"]["depth"], json!(1));
    let control = Control::new(store.clone(), time.clone());
    assert_eq!(control.inspect("p").await.unwrap().unwrap().budget_spent, 2, "the child's jumps are the tree's");
    // The child's functions read the host data it was given.
    assert!(seen.lock().unwrap().contains(&(id.clone(), Some(vouched.clone()))));
    // The parent read the child's outcome with the built-in `child_outcome`.
    let Some(Status::Completed(out)) = rt.get("p").await.unwrap().unwrap().outcome else { panic!() };
    assert_eq!(out, serde_json::to_value(&c.outcome).unwrap());

    // A parent that ends without joining cancels its host child.
    let ends = "do:\n  - spawn: { call: spawn, with: { name: slow } }";
    save(&rt, "slow", &doc("slow", "do:\n  - pause: { wait: PT1H }")).await;
    save(&rt, "ends", &doc("ends", ends)).await;
    rt.start("e", "ends", json!({})).await.unwrap();
    assert!(matches!(rt.drive("e", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    let slow = starts.lock().unwrap()[1].clone().unwrap();
    let slow = rt.get(&slow.id).await.unwrap().unwrap();
    assert_eq!(slow.outcome, Some(Status::Cancelled));
    assert_eq!(slow.admission.host(), Some(&json!({ "parent": "e", "of": null })));
}

#[tokio::test]
async fn host_children_respect_depth_versions_trips_and_pauses() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let registry = || {
        let mut registry = Registry::new();
        registry.register(whoami(Seen::default())).unwrap();
        registry.register(spawner(starts.clone(), vouched)).unwrap();
        registry
            .register(Function::new("boom", json!({}), json!({}), |_: Value, _: FunctionContext| async {
                Err::<Value, _>(FunctionError::communication(500, "boom"))
            }))
            .unwrap();
        registry
    };
    // A refusal answers `not_executed`, which the parent catches: its own
    // breaker never trips on them.
    let parent = "do:
  - attempt:
      try:
        - spawn: { call: spawn, with: { name: child, version: '${ .version }', key: '${ .key }' } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.not_executed } }
        do:
          - refused: { set: { refused: true } }";
    let not_executed = |status: Option<Status>| status == Some(Status::Completed(json!({ "refused": true })));

    // No child nests past the host's depth.
    let mut shallow = Config::default();
    shallow.limits.max_depth = 0;
    let rt = runtime(Arc::new(durable::memory::memory()), registry(), shallow, time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    save(&rt, "parent", &doc("parent", parent)).await;
    rt.start("deep", "parent", json!({})).await.unwrap();
    assert!(not_executed(rt.drive("deep", &CancellationToken::new()).await.unwrap()));
    assert_eq!(starts.lock().unwrap().pop(), Some(Err("depth".into())));
    assert!(host_children(&rt).await.is_empty());

    let config = Config { breaker_threshold: 1, ..Config::default() };
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry(), config, time.clone());
    let calm = save(&rt, "child", &doc("child", CHILD)).await;
    let failing = save(&rt, "child", &doc("child", "do:\n  - fail: { call: boom }")).await;
    let same = rt.save("child", &doc("child", "do:\n  - fail: { call: boom }")).await.unwrap().version;
    save(&rt, "parent", &doc("parent", parent)).await;
    let spawn = async |id: &str, version: Option<u64>| {
        let input = json!({ "version": version, "key": id });
        rt.start(id, "parent", input).await.unwrap();
        let status = rt.drive(id, &CancellationToken::new()).await.unwrap();
        (status, starts.lock().unwrap().pop().unwrap())
    };
    // A pinned version is honoured and marked.
    let (status, started) = spawn("pinned", Some(calm)).await;
    assert!(matches!(status, Some(Status::Completed(_))));
    let c = rt.get(&started.unwrap().id).await.unwrap().unwrap();
    assert_eq!(c.admission.version, calm);
    assert!(c.admission.pinned());
    // The breaker trips on the failing version's content.
    rt.start("direct", "child", json!({})).await.unwrap();
    assert!(matches!(rt.drive("direct", &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(_))));
    assert!(rt.definitions("child").await.unwrap().tripped.is_some());
    // The active version and any version of the tripped content are refused;
    // other content is admitted.
    let (status, started) = spawn("active", None).await;
    assert!(not_executed(status));
    assert_eq!(started, Err("tripped".into()));
    let (status, started) = spawn("same", Some(same)).await;
    assert!(not_executed(status));
    assert_eq!(started, Err("tripped".into()));
    assert_ne!(same, failing);
    let (status, started) = spawn("other", Some(calm)).await;
    assert!(matches!(status, Some(Status::Completed(_))), "{status:?}");
    assert!(started.is_ok());
    // An operator's pause refuses every admission.
    let control = Control::new(store.clone(), time.clone());
    rt.activate("child", calm).await.unwrap();
    control.pause(&operator("pause"), "child", "maintenance").await.unwrap();
    let (status, started) = spawn("paused", Some(calm)).await;
    assert!(not_executed(status));
    assert_eq!(started, Err("paused".into()));
    // A definition that cannot be admitted is a refusal too.
    let (status, started) = spawn("unknown", Some(99)).await;
    assert!(not_executed(status));
    assert_eq!(started, Err("inadmissible".into()));
    assert_eq!(host_children(&rt).await.len(), 2, "only the admitted children exist");
}

#[tokio::test]
async fn a_redispatched_call_joins_its_child_and_keeps_the_first_host_data() {
    let time = Arc::new(Time::default());
    let attempts = Arc::new(AtomicUsize::new(0));
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry.register(whoami(Seen::default())).unwrap();
    let (a, s) = (attempts.clone(), starts.clone());
    registry
        .register(Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let (attempt, starts) = (a.fetch_add(1, Ordering::SeqCst), s.clone());
            async move {
                let start = |host: Value| ChildStart { host, ..vouched(&args, &cx) };
                // The id a host can key its records by before starting the child.
                starts.lock().unwrap().push(Ok(ChildRef {
                    id: cx.child_id("k"),
                    key: String::new(),
                    done: String::new(),
                    waiting: String::new(),
                }));
                if attempt == 0 {
                    // The child commits, then the call is stopped without an answer.
                    let first = cx.start_child(start(json!("a"))).await;
                    starts.lock().unwrap().push(first.map_err(|e| kind(&e)));
                    return Err(FunctionError::interrupted("the host is shutting down"));
                }
                // The re-dispatched call recomputed its host data: the first
                // commit wins, so it joins the child as it was admitted.
                // Another definition under the same key is refused.
                let other = cx.start_child(ChildStart { name: "parent".into(), ..start(json!("a")) }).await;
                starts.lock().unwrap().push(other.map_err(|e| kind(&e)));
                let joined = cx.start_child(start(json!("b"))).await;
                starts.lock().unwrap().push(joined.as_ref().map(Clone::clone).map_err(kind));
                // The size bound applies to a child it admits: a re-dispatch
                // whose recomputed host data grew past it still joins.
                let large = cx.start_child(start(json!("x".repeat(durable::MAX_HOST_BYTES)))).await;
                starts.lock().unwrap().push(large.map_err(|e| kind(&e)));
                Ok(serde_json::to_value(joined?).unwrap())
            }
        }))
        .unwrap();
    let (observer, mut events) = ouro_runtime::buffered(1_000);
    let rt =
        runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone()).with_observer(observer);
    save(&rt, "child", &doc("child", CHILD)).await;
    save(&rt, "parent", &doc("parent", PARENT)).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "the call was dispatched again");
    let starts = starts.lock().unwrap().clone();
    // Each attempt: `child_id("k")` first, then its starts.
    let first = starts[1].clone().unwrap();
    assert_eq!(starts[0].as_ref().unwrap().id, first.id, "child_id names the child start_child starts");
    assert_eq!(starts[2].as_ref().unwrap().id, first.id, "and the same one in a re-dispatch");
    assert_eq!(starts[3], Err(format!("mismatch:{:?}", AdmissionField::Name)));
    assert_eq!(starts[4], Ok(first.clone()), "the re-dispatch joins the same child");
    assert_eq!(starts[5], Ok(first.clone()), "larger host data on a join is not refused");
    let children = host_children(&rt).await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].admission.host(), Some(&json!("a")), "the first host data stays");
    let mut admitted = 0;
    while let Some(event) = events.try_recv() {
        admitted +=
            usize::from(event.run_id == first.id && matches!(event.kind, ouro_runtime::EventKind::RunAdmitted { .. }));
    }
    assert_eq!(admitted, 1, "the child is admitted once");
}

#[tokio::test]
async fn a_lost_lease_starts_no_child() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let mut registry = Registry::new();
    let (t, s) = (time.clone(), starts.clone());
    registry
        .register(Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let (time, starts) = (t.clone(), s.clone());
            async move {
                // The lease ends while the handler works.
                time.advance(durable::LEASE_MS + 1);
                let result = cx.start_child(vouched(&args, &cx)).await;
                starts.lock().unwrap().push(result.as_ref().map(Clone::clone).map_err(kind));
                Ok(serde_json::to_value(result?).unwrap())
            }
        }))
        .unwrap();
    registry.register(whoami(Seen::default())).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    save(&rt, "parent", &doc("parent", PARENT)).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    let _ = rt.drive("p", &CancellationToken::new()).await;
    assert_eq!(*starts.lock().unwrap(), [Err("interrupted".to_owned())]);
    assert!(host_children(&rt).await.is_empty());
}

#[tokio::test]
async fn the_host_api_start_child_takes_host_data() {
    let time = Arc::new(Time::default());
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), Registry::new(), Config::default(), time.clone());
    save(&rt, "child", &doc("child", "do:\n  - done: { set: { answer: 42 } }")).await;
    save(&rt, "parent", &doc("parent", "do:\n  - pause: { wait: PT1H }")).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    let fence = store.claim("p", 0).await.unwrap().unwrap();
    let host = json!({ "user": "u-1" });
    let c = rt.start_child(&fence, "k", "c", "child", json!({}), host.clone()).await.unwrap();
    assert_eq!(c.admission.host(), Some(&host));
    assert_eq!(
        rt.start_child(&fence, "k", "c", "child", json!({}), host.clone()).await.unwrap().admission,
        c.admission
    );
    // A child join is first commit wins: other input or host data joins the
    // child as it was admitted; another definition is refused.
    let joined = rt.start_child(&fence, "k", "c", "child", json!({ "x": 1 }), Value::Null).await.unwrap();
    assert_eq!(joined.admission, c.admission);
    let other = rt.start_child(&fence, "k", "c", "parent", json!({}), host.clone()).await.unwrap_err();
    assert_eq!(mismatch(other), Some(AdmissionField::Name));
    let none = rt.start_child(&fence, "n", "n", "child", json!({}), Value::Null).await.unwrap();
    assert!(none.admission.host().is_none());
    // The size bound applies to a child it admits; a join is not refused.
    let large = json!("x".repeat(durable::MAX_HOST_BYTES));
    let joined = rt.start_child(&fence, "k", "c", "child", json!({}), large.clone()).await.unwrap();
    assert_eq!(joined.admission, c.admission);
    let refused = rt.start_child(&fence, "big", "big", "child", json!({}), large).await.unwrap_err();
    assert!(refused.downcast_ref::<HostDataTooLarge>().is_some(), "{refused}");
    // An id held under another link (another key, or a root run of the same
    // definition) is a typed mismatch, not a failure to retry.
    let other = rt.start_child(&fence, "other", "c", "child", json!({}), Value::Null).await.unwrap_err();
    assert_eq!(mismatch(other), Some(AdmissionField::Link));
    rt.start("root", "child", json!({})).await.unwrap();
    let root = rt.start_child(&fence, "r", "root", "child", json!({}), Value::Null).await.unwrap_err();
    assert_eq!(mismatch(root), Some(AdmissionField::Link));
}

/// Fails its first attempt: the call answers an error unless it runs in a
/// retry attempt (its task path names `try[n]`).
fn flaky() -> Function {
    Function::new("flaky", json!({}), json!({}), |_: Value, cx: FunctionContext| async move {
        match cx.task_path.contains("/try[") {
            true => Ok(json!({})),
            false => Err(FunctionError::communication(500, "the first attempt fails")),
        }
    })
}
const HOST_CHILD_ID: usize = "hostchild:".len() + 32;
fn hashed(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

#[tokio::test]
async fn a_dsl_retry_starts_a_host_child_per_attempt_and_a_redispatch_joins_it() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let ids = Arc::new(Mutex::new(Vec::new()));
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    registry.register(whoami(Seen::default())).unwrap();
    registry.register(flaky()).unwrap();
    let (s, i, d) = (starts.clone(), ids.clone(), dispatches.clone());
    registry
        .register(Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let (starts, ids, dispatch) = (s.clone(), i.clone(), d.fetch_add(1, Ordering::SeqCst));
            async move {
                ids.lock().unwrap().push(cx.child_id("k"));
                let result = cx.start_child(vouched(&args, &cx)).await;
                starts.lock().unwrap().push(result.as_ref().map(Clone::clone).map_err(kind));
                if dispatch == 0 {
                    // The child committed, the answer is lost: the call is
                    // dispatched again in the same attempt.
                    return Err(FunctionError::interrupted("the host is shutting down"));
                }
                Ok(serde_json::to_value(result?).unwrap())
            }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    let parent = "do:
  - attempt:
      try:
        - spawn: { call: spawn, with: { name: child } }
        - fail: { call: flaky }
      catch:
        retry: { limit: { attempt: { count: 2 } } }";
    save(&rt, "parent", &doc("parent", parent)).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    let status = settle(&rt, &time, "p").await;
    assert!(matches!(status, Some(Status::Completed(_))), "{status:?}");
    assert_eq!(dispatches.load(Ordering::SeqCst), 3, "two dispatches of the first attempt, one of the second");
    let starts: Vec<_> = starts.lock().unwrap().iter().map(|s| s.clone().unwrap()).collect();
    assert_eq!(starts[0], starts[1], "a re-dispatch of the same attempt joins its child");
    assert_ne!(starts[1].id, starts[2].id, "a retry attempt starts its own child");
    assert_eq!(starts[0].key, starts[2].key, "under the call's logical key");
    let ids = ids.lock().unwrap().clone();
    assert_eq!(ids, starts.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), "child_id follows the attempt");
    for started in &starts {
        assert!(hashed(&started.id, "hostchild:"), "{}", started.id);
    }
    let children = host_children(&rt).await;
    assert_eq!(children.len(), 2, "one child per attempt");
    // Both owned by the parent under the call's logical key, each linked to
    // the attempt that started it.
    let links: Vec<_> = children.iter().map(|c| c.child.clone().unwrap()).collect();
    assert!(links.iter().all(|l| l.parent == "p" && l.key == starts[0].key), "{links:?}");
    assert_ne!(links[0].reference, links[1].reference);
}

#[tokio::test]
async fn host_child_ids_have_a_fixed_length_at_the_depth_limit() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let mut registry = Registry::new();
    let s = starts.clone();
    registry
        .register(Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let starts = s.clone();
            async move {
                // Refuses instead of storing an id that grew with the nesting.
                if cx.child_id("k").len() != HOST_CHILD_ID {
                    return Err(FunctionError::not_executed(format!("an id of {} bytes", cx.child_id("k").len())));
                }
                let result = cx.start_child(vouched(&args, &cx)).await;
                starts.lock().unwrap().push(result.as_ref().map(Clone::clone).map_err(kind));
                Ok(serde_json::to_value(result?).unwrap())
            }
        }))
        .unwrap();
    // Room for every run of the chain in one pass: a waiting parent of a
    // waiting child stays due (its child's `waiting:` notice is never taken),
    // and with the default four slots such parents can take every slot of a
    // pass ahead of the one run that can progress, depending on id order.
    let config = Config { max_concurrency: 16, ..Config::default() };
    let depth = config.limits.max_depth;
    assert_eq!(depth, 10, "the default nesting depth");
    let rt = runtime(Arc::new(durable::memory::memory()), registry, config, time.clone());
    // Each level starts a child of its own definition and joins it, until
    // the depth limit refuses one.
    let nest = "do:
  - deeper:
      try:
        - spawn: { call: spawn, with: { name: nest }, export: { as: '${ . }' } }
        - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.not_executed } }
        do: [ { bottom: { set: { bottom: true } } } ]";
    save(&rt, "nest", &doc("nest", nest)).await;
    rt.start("r", "nest", json!({})).await.unwrap();
    let status = settle(&rt, &time, "r").await;
    assert!(matches!(status, Some(Status::Completed(_))), "{status:?}");
    let starts = starts.lock().unwrap().clone();
    assert_eq!(starts.len(), depth as usize + 1);
    assert_eq!(starts.last(), Some(&Err("depth".to_owned())));
    let children = host_children(&rt).await;
    assert_eq!(children.len(), depth as usize, "one child per level");
    for child in &children {
        assert!(hashed(&child.admission.id, "hostchild:"), "{}", child.admission.id);
    }
    let deepest = children.iter().map(|c| c.admission.runtime["limits"]["depth"].as_u64()).max();
    assert_eq!(deepest, Some(Some(u64::from(depth))));
}

#[tokio::test]
async fn an_id_held_under_another_link_is_a_typed_refusal() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let mut registry = Registry::new();
    registry.register(whoami(Seen::default())).unwrap();
    let (s, taken) = (starts.clone(), store.clone());
    registry
        .register(Function::new("spawn", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let (starts, store) = (s.clone(), taken.clone());
            async move {
                // A run of the same definition already holds the child's id,
                // with no link to this call.
                let mut squatter = store.get("template").await.unwrap().unwrap().admission;
                squatter.id = cx.child_id("k");
                store.admit(&squatter, 0).await.unwrap();
                let result = cx.start_child(vouched(&args, &cx)).await;
                starts.lock().unwrap().push(result.as_ref().map(Clone::clone).map_err(kind));
                Ok(serde_json::to_value(result?).unwrap())
            }
        }))
        .unwrap();
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    rt.start("template", "child", json!({})).await.unwrap();
    let parent = "do:
  - attempt:
      try:
        - spawn: { call: spawn, with: { name: child } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.not_executed } }
        do: [ { refused: { set: { refused: true } } } ]";
    save(&rt, "parent", &doc("parent", parent)).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    let status = settle(&rt, &time, "p").await;
    assert_eq!(status, Some(Status::Completed(json!({ "refused": true }))));
    assert_eq!(*starts.lock().unwrap(), [Err(format!("mismatch:{:?}", AdmissionField::Link))], "never an interruption");
}

#[tokio::test]
async fn control_redacts_host_data_except_in_inspect() {
    let time = Arc::new(Time::default());
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry.register(whoami(Seen::default())).unwrap();
    registry.register(spawner(starts.clone(), vouched)).unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    save(&rt, "parent", &doc("parent", PARENT)).await;
    let host = json!({ "tenant": "secret-tenant" });
    rt.start_with("p", "parent", json!({}), Start { version: None, host: host.clone() }).await.unwrap();
    save(&rt, "idle", &doc("idle", "do:\n  - pause: { wait: PT1H }")).await;
    rt.start("plain", "idle", json!({})).await.unwrap();
    let control = Control::new(store.clone(), time.clone());
    let redacted = |runs: &[durable::SavedRun]| {
        for run in runs {
            let shown = run.admission.runtime.get(durable::HOST);
            match run.admission.id.as_str() {
                "plain" => assert!(shown.is_none(), "a run without host data shows none"),
                _ => assert_eq!(shown, Some(&json!(REDACTED)), "{}", run.admission.id),
            }
        }
    };
    let live = control.list(&Runs::default(), None, 100).await.unwrap();
    assert_eq!(live.items.len(), 2);
    redacted(&live.items);
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    let child = starts.lock().unwrap()[0].clone().unwrap().id;
    let window = Runs { since_ms: Some(0), before_ms: Some(i64::MAX), ..Runs::default() };
    let ended = control.history(&window, None, 100).await.unwrap();
    assert_eq!(ended.items.len(), 2, "the parent and its child");
    redacted(&ended.items);
    redacted(&control.list(&Runs::default(), None, 100).await.unwrap().items);
    // `inspect` of one run shows its host data; its children's are redacted.
    let inspection = control.inspect("p").await.unwrap().unwrap();
    assert_eq!(inspection.host, Some(host.clone()));
    assert_eq!(inspection.snapshot.run.admission.host(), Some(&host));
    assert_eq!(inspection.children.len(), 1);
    redacted(&inspection.children);
    let inspection = control.inspect(&child).await.unwrap().unwrap();
    assert_eq!(inspection.host, Some(json!({ "parent": "p", "of": host })));
    assert_eq!(control.inspect("plain").await.unwrap().unwrap().host, None);
    // The host's own reads are not the control API: they return the run as stored.
    assert_eq!(rt.get("p").await.unwrap().unwrap().admission.host(), Some(&host));
}

#[tokio::test]
async fn observers_never_carry_host_data() {
    let time = Arc::new(Time::default());
    let mut registry = Registry::new();
    registry.register(whoami(Seen::default())).unwrap();
    registry.register(spawner(Starts::default(), vouched)).unwrap();
    let (observer, mut events) = ouro_runtime::buffered(10_000);
    let rt =
        runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone()).with_observer(observer);
    save(&rt, "child", &doc("child", CHILD)).await;
    save(&rt, "parent", &doc("parent", PARENT)).await;
    let host = json!({ "tenant": "secret-tenant" });
    rt.start_with("p", "parent", json!({}), Start { version: None, host }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    let mut count = 0;
    while let Some(event) = events.try_recv() {
        count += 1;
        assert!(!format!("{event:?}").contains("secret-tenant"), "{event:?}");
    }
    assert!(count > 10);
}

#[tokio::test]
async fn dry_runs_and_replays_start_children_only_in_their_private_store() {
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry.register(whoami(seen.clone())).unwrap();
    registry.register(spawner(starts.clone(), vouched)).unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    let parent = doc("parent", &format!("{PARENT}\n  - who: {{ call: whoami }}"));
    let nothing =
        ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(Value::Null));
    let host = json!({ "tenant": "t-1" });
    let dry =
        rt.dry_run_with(&parent, json!({}), Some(host.clone()), nothing, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    // The root read its host data, and its child was started in the private store.
    assert!(seen.lock().unwrap().contains(&("dry-run".to_owned(), Some(host.clone()))));
    let started = starts.lock().unwrap().pop().unwrap().unwrap();
    assert!(seen.lock().unwrap().iter().any(|(run, host)| *run == started.id && host.is_some()));
    assert!(store.list(None, "").await.unwrap().is_empty(), "the host's store holds no run");
    // A mocked call runs no handler: it has no child capability and starts nothing.
    let capable = Arc::new(Mutex::new(Vec::new()));
    let c = capable.clone();
    let spawn_mocked = ouro_runtime::mock(
        |function: &str, _: &Value| function == "spawn",
        move |_: &str, _: &Value, cx: &FunctionContext| {
            c.lock().unwrap().push(cx.children.is_some());
            Ok(json!({}))
        },
    );
    let lone = doc("lone", "do:\n  - spawn: { call: spawn, with: { name: child } }");
    let dry =
        rt.dry_run_with(&lone, json!({}), Some(host.clone()), spawn_mocked, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)));
    assert_eq!(*capable.lock().unwrap(), [false]);
    assert!(starts.lock().unwrap().is_empty());
    // Replay dispatches nothing, so it starts no child.
    save(&rt, "parent", &parent).await;
    rt.start_with("p", "parent", json!({}), Start { version: None, host }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    let before = store.list(None, "").await.unwrap();
    let calls = starts.lock().unwrap().len();
    let replay = rt.replay("p", &CancellationToken::new()).await.unwrap();
    assert!(matches!(replay.status, Status::Completed(_)), "{:?}", replay.status);
    assert_eq!(store.list(None, "").await.unwrap(), before);
    assert_eq!(starts.lock().unwrap().len(), calls);
}
