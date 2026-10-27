//! Host data at admission and host-started children (design 03): the host
//! vouches for a run's facts, which no workflow can set, and a host function
//! starts owned children it vouches for. A handler reaches the store that
//! drives its call, live and dry, within its run's lineage (design 07).
use ouro_runtime::{
    ChildError, ChildRef, ChildRefusal, ChildStart, Config, DryRunOptions, Function, FunctionContext, FunctionError,
    HostDataTooLarge, ProcessRunner, Registry, RunAccessError, Runtime, Start,
    control::{Control, REDACTED, Request, Runs},
    durable::{self, AdmissionField, AdmissionMismatch, Clock, RunState, RunStore},
    ows::{durable::Record, run::Status},
};
use serde_json::{Value, json};
use sha2::Digest;
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
    let config = Config::default();
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

// ── design 07: a handler's run access ──────────────────────────────────────

/// Why run access answered nothing, as a label.
fn refusal(error: &RunAccessError) -> String {
    match error {
        RunAccessError::Unavailable => "unavailable".into(),
        RunAccessError::OutOfScope => "out-of-scope".into(),
        RunAccessError::ReservedEventType => "reserved".into(),
        RunAccessError::TargetEnded => "ended".into(),
        RunAccessError::Interrupted(_) => "interrupted".into(),
        RunAccessError::InvalidEvent(_) => "invalid".into(),
        RunAccessError::Storage(e) => format!("storage: {e:#}"),
        other => format!("{other:?}"),
    }
}
/// The id of the run `cx.run(id)` returned, `none`, or why it answered nothing.
async fn seen_run(cx: &FunctionContext, id: &str) -> String {
    match cx.run(id).await {
        Ok(Some(run)) => run.admission.id,
        Ok(None) => "none".into(),
        Err(e) => refusal(&e),
    }
}
/// `cx.records(id)`: `none`, the event types its consumed events carry, or why nothing.
async fn seen_records(cx: &FunctionContext, id: &str) -> String {
    match cx.records(id).await {
        Ok(Some(records)) => {
            let types: Vec<_> = records
                .values()
                .filter_map(|r| match r {
                    Record::Event { event } => event["type"].as_str().map(str::to_owned),
                    _ => None,
                })
                .collect();
            format!("events:{}", types.join(","))
        }
        Ok(None) => "none".into(),
        Err(e) => refusal(&e),
    }
}
/// `cx.children(parent)`: the children's ids, or why none.
async fn seen_children(cx: &FunctionContext, parent: &str) -> String {
    match cx.children(parent, None, 10).await {
        Ok(page) => page.items.iter().map(|r| r.admission.id.clone()).collect::<Vec<_>>().join(","),
        Err(e) => refusal(&e),
    }
}
/// `cx.deliver(event, target)`: the recipient, or why none.
async fn delivered(cx: &FunctionContext, event: Value, target: &str) -> String {
    match cx.deliver(event, target).await {
        Ok(recipient) => recipient,
        Err(e) => refusal(&e),
    }
}
/// An identified event of `type_`, its id derived from the call's
/// idempotency key: a re-dispatch delivers the same identity.
fn event(cx: &FunctionContext, type_: &str) -> Value {
    json!({ "id": format!("{}#{type_}", cx.idempotency_key), "source": "test", "type": type_, "data": { "ok": true } })
}
/// `(caller, asked, answer)` for every run a `look` call read.
type Looks = Arc<Mutex<Vec<(String, String, String)>>>;
/// Reads the caller's own run and the runs its arguments name (`others`).
fn look(looks: Looks) -> Function {
    Function::new("look", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
        let looks = looks.clone();
        async move {
            let mut asked = vec![cx.run_id.clone()];
            asked.extend(args["others"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
            for id in asked {
                let answer = seen_run(&cx, &id).await;
                looks.lock().unwrap().push((cx.run_id.clone(), id, answer));
            }
            Ok(json!({}))
        }
    })
}
/// Delivers an event of `type` to the run `id`, after reading its state.
type Pokes = Arc<Mutex<Vec<(String, String)>>>;
fn poke(pokes: Pokes) -> Function {
    Function::new("poke", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
        let pokes = pokes.clone();
        async move {
            let id = args["id"].as_str().unwrap_or_default().to_owned();
            let state = match cx.run(&id).await {
                Ok(Some(run)) => format!("{:?}", run.state),
                other => format!("{:?}", other.map(|_| ()).map_err(|e| refusal(&e))),
            };
            let answer = delivered(&cx, event(&cx, args["type"].as_str().unwrap_or("poke")), &id).await;
            pokes.lock().unwrap().push((state, answer));
            Ok(json!({}))
        }
    })
}

/// Test 1: live, a handler reads its own run and its child's; an unrelated
/// run answers `None`.
#[tokio::test]
async fn a_live_handler_reads_its_own_run_and_its_child() {
    let time = Arc::new(Time::default());
    let looks = Looks::default();
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry.register(look(looks.clone())).unwrap();
    registry.register(spawner(starts.clone(), vouched)).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", "do:\n  - look: { call: look }")).await;
    let parent = "do:
  - spawn: { call: spawn, with: { name: child }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }
  - look: { call: look, with: { others: [ '${ $context.id }', unrelated ] } }";
    save(&rt, "parent", &doc("parent", parent)).await;
    save(&rt, "idle", &doc("idle", "do:\n  - pause: { wait: P1D }")).await;
    rt.start("unrelated", "idle", json!({})).await.unwrap();
    rt.start("p", "parent", json!({})).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    let child = starts.lock().unwrap()[0].clone().unwrap().id;
    let looks = looks.lock().unwrap().clone();
    for expected in [
        ("p", "p", "p"),
        ("p", child.as_str(), child.as_str()),
        ("p", "unrelated", "none"),
        (child.as_str(), child.as_str(), child.as_str()),
    ] {
        let expected = (expected.0.to_owned(), expected.1.to_owned(), expected.2.to_owned());
        assert!(looks.contains(&expected), "{expected:?} in {looks:#?}");
    }
}

/// Test 2: dry, the same through `dry_run_opts`, for the root, a host child
/// and a `run: workflow` child; the host's store is out of sight.
#[tokio::test]
async fn a_dry_handler_reads_its_own_run_and_its_children_in_the_private_store() {
    let time = Arc::new(Time::default());
    let looks = Looks::default();
    let starts = Starts::default();
    let mut registry = Registry::new();
    registry.register(look(looks.clone())).unwrap();
    registry.register(spawner(starts.clone(), vouched)).unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", "do:\n  - look: { call: look }")).await;
    save(&rt, "kid", &doc("kid", "do:\n  - look: { call: look }")).await;
    save(&rt, "idle", &doc("idle", "do:\n  - pause: { wait: P1D }")).await;
    rt.start("unrelated", "idle", json!({})).await.unwrap();
    let root = doc(
        "root",
        "do:
  - spawn: { call: spawn, with: { name: child }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }
  - kid: { run: { workflow: { namespace: test, name: kid, version: latest } } }
  - look: { call: look, with: { others: [ '${ $context.id }', unrelated ] } }",
    );
    let nothing =
        ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(json!({})));
    let rows = store.list(None, "").await.unwrap();
    let dry =
        rt.dry_run_opts(&root, json!({}), nothing, DryRunOptions::default(), &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(store.list(None, "").await.unwrap(), rows, "the host's store is untouched");
    let child = starts.lock().unwrap()[0].clone().unwrap().id;
    let looks = looks.lock().unwrap().clone();
    let kid = looks.iter().map(|l| l.0.clone()).find(|caller| caller.starts_with("child:")).expect("the kid looked");
    for expected in [
        ("dry-run", "dry-run", "dry-run"),
        ("dry-run", child.as_str(), child.as_str()),
        ("dry-run", "unrelated", "none"),
        (child.as_str(), child.as_str(), child.as_str()),
        (kid.as_str(), kid.as_str(), kid.as_str()),
    ] {
        let expected = (expected.0.to_owned(), expected.1.to_owned(), expected.2.to_owned());
        assert!(looks.contains(&expected), "{expected:?} in {looks:#?}");
    }
}

/// Test 3: dry, a handler delivers to its own run before a `listen` with a
/// timeout; the listen takes it from the mailbox under the zero listen
/// ceiling and the dry run completes instead of timing out (the
/// confirmation pattern).
#[tokio::test]
async fn a_dry_handler_answers_its_own_listen() {
    let answers = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    let a = answers.clone();
    registry
        .register(Function::new("confirm", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let answers = a.clone();
            async move {
                let answer = delivered(&cx, event(&cx, "answer"), &cx.run_id).await;
                answers.lock().unwrap().push(answer);
                Ok(json!({}))
            }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), Arc::new(Time::default()));
    let definition = doc(
        "confirming",
        "do:
  - ask: { call: confirm }
  - answer: { listen: { to: { one: { with: { type: answer } } } }, timeout: { after: PT1H } }",
    );
    let nothing =
        ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(json!({})));
    let dry = rt
        .dry_run_opts(&definition, json!({}), nothing, DryRunOptions::default(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(dry.status, Status::Completed(json!([{ "ok": true }])));
    assert_eq!(*answers.lock().unwrap(), ["dry-run"]);
    assert!(dry.mocked.is_empty(), "a delivery is not a mocked call");
}

/// Test 4: dry, a parent's handler delivers to a child suspended on a
/// `listen` without a timeout; the child wakes and the dry run completes.
#[tokio::test]
async fn a_dry_parent_wakes_its_listening_child() {
    let pokes = Pokes::default();
    let mut registry = Registry::new();
    registry.register(poke(pokes.clone())).unwrap();
    registry.register(spawner(Starts::default(), vouched)).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), Arc::new(Time::default()));
    save(&rt, "sleeper", &doc("sleeper", "do:\n  - wake: { listen: { to: { one: { with: { type: poke } } } } }")).await;
    let root = doc(
        "root",
        "do:
  - spawn: { call: spawn, with: { name: sleeper }, export: { as: '${ . }' } }
  - asleep: { listen: { to: { one: { with: { type: '${ $context.waiting }' } } } } }
  - poke: { call: poke, with: { id: '${ $context.id }', type: poke } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }",
    );
    let nothing =
        ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(json!({})));
    let dry =
        rt.dry_run_opts(&root, json!({}), nothing, DryRunOptions::default(), &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    let pokes = pokes.lock().unwrap().clone();
    assert_eq!(pokes.len(), 1);
    assert_eq!(pokes[0].0, format!("{:?}", RunState::Waiting), "the child was suspended on its listen");
    assert!(pokes[0].1.starts_with("hostchild:"), "delivered to the child: {pokes:?}");
    let child = dry.runs.iter().find(|r| r.id == pokes[0].1).unwrap();
    assert!(matches!(child.outcome, Some(Status::Completed(_))), "{:?}", child.outcome);
}

/// Test 5: a delivery is refused, each with its type: to an unrelated run,
/// with a reserved type, an event that is not identified or whose identity
/// was delivered with another payload, and after the call's lease is lost.
#[tokio::test]
async fn a_handler_delivery_is_refused_out_of_scope_reserved_or_without_a_lease() {
    let time = Arc::new(Time::default());
    let answers = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    let (a, t) = (answers.clone(), time.clone());
    registry
        .register(Function::new("refused", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let (answers, time) = (a.clone(), t.clone());
            async move {
                let mut seen = vec![delivered(&cx, event(&cx, "x"), "unrelated").await];
                for reserved in ["done:c", "waiting:c", "child:c"] {
                    seen.push(delivered(&cx, event(&cx, reserved), &cx.run_id).await);
                }
                seen.push(delivered(&cx, json!({ "source": "test", "type": "x" }), &cx.run_id).await);
                let same = |n: u32| json!({ "id": "same", "source": "test", "type": "y", "data": n });
                seen.push(delivered(&cx, same(1), &cx.run_id).await);
                seen.push(delivered(&cx, same(2), &cx.run_id).await);
                // The lease ends while the handler works.
                time.advance(durable::LEASE_MS + 1);
                seen.push(delivered(&cx, event(&cx, "x"), &cx.run_id).await);
                answers.lock().unwrap().extend(seen);
                Ok(json!({}))
            }
        }))
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "idle", &doc("idle", "do:\n  - pause: { wait: P1D }")).await;
    save(&rt, "refusing", &doc("refusing", "do:\n  - refused: { call: refused }")).await;
    rt.start("unrelated", "idle", json!({})).await.unwrap();
    rt.start("p", "refusing", json!({})).await.unwrap();
    let _ = rt.drive("p", &CancellationToken::new()).await;
    assert_eq!(
        *answers.lock().unwrap(),
        ["out-of-scope", "reserved", "reserved", "reserved", "invalid", "p", "invalid", "interrupted"]
    );
    assert!(store.inspect("unrelated").await.unwrap().unwrap().events.is_empty(), "unrelated got no event");
}

/// Test 6: a delivering handler dispatched again (at least once) with an
/// event id derived from its idempotency key leaves one pending copy.
#[tokio::test]
async fn a_redispatched_delivery_leaves_one_pending_copy() {
    let time = Arc::new(Time::default());
    let dispatches = Arc::new(AtomicUsize::new(0));
    let answers = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    let (d, a) = (dispatches.clone(), answers.clone());
    registry
        .register(Function::new("confirm", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let (dispatch, answers) = (d.fetch_add(1, Ordering::SeqCst), a.clone());
            async move {
                let answer = delivered(&cx, event(&cx, "answer"), &cx.run_id).await;
                answers.lock().unwrap().push(answer);
                if dispatch == 0 {
                    // The delivery committed, the answer is lost.
                    return Err(FunctionError::interrupted("the host is shutting down"));
                }
                Ok(json!({}))
            }
        }))
        .unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "confirming", &doc("confirming", "do:\n  - ask: { call: confirm }\n  - later: { wait: P7D }")).await;
    rt.start("p", "confirming", json!({})).await.unwrap();
    for _ in 0..20 {
        let run = rt.get("p").await.unwrap().unwrap();
        if dispatches.load(Ordering::SeqCst) == 2 && run.state == RunState::Waiting {
            break;
        }
        if rt.recover_once(&CancellationToken::new()).await.unwrap() == 0 {
            time.advance(60_000);
        }
    }
    assert_eq!(dispatches.load(Ordering::SeqCst), 2, "the call was dispatched again");
    assert_eq!(*answers.lock().unwrap(), ["p", "p"]);
    let pending = store.inspect("p").await.unwrap().unwrap().events;
    assert_eq!(pending.len(), 1, "{pending:#?}");
}

/// Test 11: a dry run's `Mock::response`, a process runner and replay get
/// no run access; `cx.run` answers `Unavailable` there.
#[tokio::test]
async fn mocked_calls_process_runs_and_replays_have_no_run_access() {
    struct Runner(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl ProcessRunner for Runner {
        fn runs(&self, kind: &str) -> bool {
            kind == "shell"
        }
        async fn run(&self, _: &str, _: Value, cx: FunctionContext) -> Result<Value, FunctionError> {
            let answer = seen_run(&cx, &cx.run_id).await;
            self.0.lock().unwrap().push(format!("process:{}:{answer}", cx.runs.is_some()));
            Ok(json!({ "code": 0, "stdout": "", "stderr": "" }))
        }
    }
    let time = Arc::new(Time::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let looks = Looks::default();
    let mut registry = Registry::new();
    registry.register(look(looks.clone())).unwrap();
    registry
        .register(Function::new("marked", json!({}), json!({}), |_: Value, _: FunctionContext| async { Ok(json!({})) }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone())
        .with_process_runner(Arc::new(Runner(seen.clone())));
    let definition =
        doc("mixed", "do:\n  - s: { run: { shell: { command: ls } } }\n  - m: { call: marked }\n  - l: { call: look }");
    let s = seen.clone();
    let mock = ouro_runtime::mock(
        |function: &str, _: &Value| function == "marked",
        move |_: &str, _: &Value, cx: &FunctionContext| {
            let answer = futures::executor::block_on(seen_run(cx, &cx.run_id));
            s.lock().unwrap().push(format!("mock:{}:{answer}", cx.runs.is_some()));
            Ok(json!({}))
        },
    );
    let dry = rt.dry_run(&definition, json!({}), mock, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert_eq!(*seen.lock().unwrap(), ["process:false:unavailable", "mock:false:unavailable"]);
    // A registered function's handler in the same dry run has it.
    assert_eq!(*looks.lock().unwrap(), [("dry-run".to_owned(), "dry-run".to_owned(), "dry-run".to_owned())]);
    // Replay dispatches nothing: no handler or runner gets a context at all.
    save(&rt, "mixed", &definition).await;
    rt.start("r", "mixed", json!({})).await.unwrap();
    assert!(matches!(settle(&rt, &time, "r").await, Some(Status::Completed(_))));
    let (heard, looked) = (seen.lock().unwrap().len(), looks.lock().unwrap().len());
    let replay = rt.replay("r", &CancellationToken::new()).await.unwrap();
    assert!(matches!(replay.status, Status::Completed(_)), "{:?}", replay.status);
    assert_eq!((seen.lock().unwrap().len(), looks.lock().unwrap().len()), (heard, looked));
}

/// A run: workflow child's id (`child:` and the first 128 bits of the
/// SHA-256 of `[run, task reference]`).
fn workflow_child_id(run: &str, reference: &str) -> String {
    let digest = sha2::Sha256::digest(serde_json::to_string(&[run, reference]).unwrap().as_bytes());
    format!("child:{}", digest[..16].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Test 12: dry, run access covers the call's lineage: a handler reads a
/// child's records (a consumed event) and lists its children; a grandchild's
/// handler reads its parent and its grandparent; a sibling's and an
/// unrelated run's records answer `None` and their children `OutOfScope`; a
/// delivery to an ancestor is `OutOfScope`, to an ended child `TargetEnded`.
#[tokio::test]
async fn run_access_covers_the_lineage_and_nothing_else() {
    // label -> answer, from every handler of the tree.
    let checks = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
    let ids = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
    let mut registry = Registry::new();
    registry.register(poke(Pokes::default())).unwrap();
    registry.register(spawner(Starts::default(), vouched)).unwrap();
    let i = ids.clone();
    // The root's: starts the sibling `s`, then `a`, and answers both refs.
    registry
        .register(Function::new("family", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let ids = i.clone();
            async move {
                let start = |key: &str| ChildStart { key: key.into(), name: key.into(), ..ChildStart::default() };
                let s = cx.start_child(start("s")).await?;
                let a = cx.start_child(start("a")).await?;
                let mut ids = ids.lock().unwrap();
                ids.insert("root".into(), cx.run_id.clone());
                ids.insert("s".into(), s.id.clone());
                ids.insert("a".into(), a.id.clone());
                ids.insert("free".into(), workflow_child_id(&cx.run_id, "/do/1/free"));
                Ok(json!({ "a": a, "s": s }))
            }
        }))
        .unwrap();
    let (c, i) = (checks.clone(), ids.clone());
    // `a`'s: its sibling, an unrelated run and its parent are out of scope.
    registry
        .register(Function::new("sibling", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let (checks, ids) = (c.clone(), i.clone());
            async move {
                let ids = ids.lock().unwrap().clone();
                let mut seen = BTreeMap::new();
                seen.insert("a reads s's records".into(), seen_records(&cx, &ids["s"]).await);
                seen.insert("a lists s's children".into(), seen_children(&cx, &ids["s"]).await);
                seen.insert("a reads free's records".into(), seen_records(&cx, &ids["free"]).await);
                seen.insert("a lists free's children".into(), seen_children(&cx, &ids["free"]).await);
                seen.insert("a reads free".into(), seen_run(&cx, &ids["free"]).await);
                seen.insert("a delivers to root".into(), delivered(&cx, event(&cx, "up"), &ids["root"]).await);
                checks.lock().unwrap().extend(seen);
                Ok(json!({}))
            }
        }))
        .unwrap();
    let (c, i) = (checks.clone(), ids.clone());
    // `g`'s: its parent `a` and its grandparent, the root.
    registry
        .register(Function::new("lineage", json!({}), json!({}), move |_: Value, cx: FunctionContext| {
            let (checks, ids) = (c.clone(), i.clone());
            async move {
                let ids = ids.lock().unwrap().clone();
                let mut seen = BTreeMap::new();
                seen.insert("g".into(), cx.run_id.clone());
                seen.insert("g reads a".into(), seen_run(&cx, &ids["a"]).await);
                seen.insert("g reads root".into(), seen_run(&cx, &ids["root"]).await);
                seen.insert("g reads root's records".into(), seen_records(&cx, &ids["root"]).await);
                seen.insert("g reads s".into(), seen_run(&cx, &ids["s"]).await);
                checks.lock().unwrap().extend(seen);
                Ok(json!({}))
            }
        }))
        .unwrap();
    let c = checks.clone();
    // The root's, after `a` ended: its records, its children, a delivery.
    registry
        .register(Function::new("examine", json!({}), json!({}), move |args: Value, cx: FunctionContext| {
            let checks = c.clone();
            async move {
                let a = args["a"].as_str().unwrap_or_default().to_owned();
                let mut seen = BTreeMap::new();
                seen.insert("root reads a's records".into(), seen_records(&cx, &a).await);
                seen.insert("root lists a's children".into(), seen_children(&cx, &a).await);
                seen.insert("root delivers to a".into(), delivered(&cx, event(&cx, "late"), &a).await);
                checks.lock().unwrap().extend(seen);
                Ok(json!({}))
            }
        }))
        .unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), Arc::new(Time::default()));
    save(&rt, "s", &doc("s", "do:\n  - done: { set: { s: 1 } }")).await;
    let a = "do:
  - ping: { listen: { to: { one: { with: { type: ping } } } } }
  - spawn: { call: spawn, with: { name: g }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }
  - sibling: { call: sibling }";
    save(&rt, "a", &doc("a", a)).await;
    save(&rt, "g", &doc("g", "do:\n  - lineage: { call: lineage }")).await;
    save(&rt, "idle", &doc("idle", "do:\n  - pause: { wait: P1D }")).await;
    let root = doc(
        "root",
        "do:
  - family: { call: family, export: { as: '${ . }' } }
  - free: { run: { workflow: { namespace: test, name: idle, version: latest }, await: false } }
  - ping: { call: poke, with: { id: '${ $context.a.id }', type: ping } }
  - join: { listen: { to: { one: { with: { type: '${ $context.a.done }' } } } } }
  - examine: { call: examine, with: { a: '${ $context.a.id }' } }",
    );
    let nothing =
        ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(json!({})));
    let dry =
        rt.dry_run_opts(&root, json!({}), nothing, DryRunOptions::default(), &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    let ids = ids.lock().unwrap().clone();
    assert!(dry.runs.iter().any(|r| r.id == ids["free"] && r.independent), "the free run is admitted and unrelated");
    let checks = checks.lock().unwrap().clone();
    let g = checks["g"].clone();
    let consumed = format!("events:ping,done:{g}");
    let expected: BTreeMap<String, String> = [
        ("a reads s's records", "none"),
        ("a lists s's children", "out-of-scope"),
        ("a reads free's records", "none"),
        ("a lists free's children", "out-of-scope"),
        ("a reads free", "none"),
        ("a delivers to root", "out-of-scope"),
        ("g", g.as_str()),
        ("g reads a", ids["a"].as_str()),
        ("g reads root", ids["root"].as_str()),
        ("g reads root's records", "events:"),
        ("g reads s", "none"),
        ("root reads a's records", consumed.as_str()),
        ("root lists a's children", g.as_str()),
        ("root delivers to a", "ended"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(checks, expected);
}


// ── design 08: explicitly inherited, opaque host data ─────────────────────
const INHERITED_PARENT: &str = "do:
  - native: { metadata: { ouro: { host: inherit } }, run: { workflow: { namespace: test, name: child, version: latest, input: { forged: true } } } }
  - detached: { metadata: { ouro: { host: inherit } }, run: { workflow: { namespace: test, name: child, version: latest }, await: false } }
  - builtin: { call: start_child, with: { name: child, host: inherit }, export: { as: '${ . }' } }
  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }";

#[tokio::test]
async fn inherited_host_is_exact_for_owned_independent_and_reserved_children() {
    for host in [json!({ "opaque": [null, true, { "tenant": "t", "rights": ["read"] }], "n": 7 }),
        json!("x".repeat(durable::MAX_HOST_BYTES - 2)), Value::Null] {
        let time = Arc::new(Time::default());
        let seen = Seen::default();
        let mut registry = Registry::new();
        registry.register(whoami(seen.clone())).unwrap();
        let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
        save(&rt, "child", &doc("child", CHILD)).await;
        save(&rt, "parent", &doc("parent", INHERITED_PARENT)).await;
        rt.start_with("p", "parent", json!({ "host": { "forged": true } }), Start { version: None, host: host.clone() }).await.unwrap();
        assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
        while rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
        let children = rt.list(Some("child"), "").await.unwrap();
        assert_eq!(children.len(), 3);
        let expected = (!host.is_null()).then_some(&host);
        assert_eq!(children.iter().filter(|c| c.child.is_some()).count(), 2);
        assert_eq!(children.iter().filter(|c| c.child.is_none()).count(), 1);
        for child in children {
            assert_eq!(child.admission.host(), expected);
            assert_eq!(serde_json::to_vec(&child.admission.host()).unwrap(), serde_json::to_vec(&expected).unwrap());
            assert!(seen.lock().unwrap().contains(&(child.admission.id, expected.cloned())));
        }
    }
}

#[tokio::test]
async fn inherited_host_requires_opt_in_at_every_generation() {
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let mut registry = Registry::new();
    registry.register(whoami(seen.clone())).unwrap();
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone());
    save(&rt, "leaf", &doc("leaf", CHILD)).await;
    let branch = "do:
  - who: { call: whoami }
  - inherited: { metadata: { ouro: { host: inherit } }, run: { workflow: { namespace: test, name: leaf, version: latest } } }
  - plain: { run: { workflow: { namespace: test, name: leaf, version: latest } } }";
    save(&rt, "child", &doc("child", branch)).await;
    save(&rt, "parent", &doc("parent", INHERITED_PARENT)).await;
    let host = json!({ "opaque": { "a": [1, 2, 3] } });
    rt.start_with("p", "parent", json!({}), Start { host: host.clone(), ..Start::default() }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    while rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    let leaves = rt.list(Some("leaf"), "").await.unwrap();
    assert_eq!(leaves.len(), 6);
    assert_eq!(leaves.iter().filter(|r| r.admission.host() == Some(&host)).count(), 3);
    assert_eq!(leaves.iter().filter(|r| r.admission.host().is_none()).count(), 3);
    assert_eq!(seen.lock().unwrap().iter().filter(|(_, h)| h.as_ref() == Some(&host)).count(), 6);
}

#[test]
fn inherited_host_metadata_is_literal_and_only_for_workflow_run_tasks() {
    let wf = "run: { workflow: { namespace: test, name: child, version: latest } }";
    let yaml = |task: &str, mode: &str| doc("p", &format!("do: [ {{ a: {{ metadata: {{ ouro: {{ host: {mode} }} }}, {task} }} }} ]"));
    assert!(ouro_runtime::ows::model::parse_yaml(&yaml(wf, "inherit")).is_ok());
    for mode in ["null", "{}", "[]", "true", "42", "copy", "'${ {tenant: \"forged\"} }'", "'${ \"inherit\" }'"] {
        let error = ouro_runtime::ows::model::parse_yaml(&yaml(wf, mode)).unwrap_err().to_string();
        assert!(error.contains("/metadata/ouro/host"), "{mode}: {error}");
    }
    for task in ["set: { x: 1 }", "call: start_child", "run: { shell: { command: ls } }", "wait: PT1S"] {
        assert!(ouro_runtime::ows::model::parse_yaml(&yaml(task, "inherit")).is_err(), "{task}");
    }
}

fn reserved_catch(arguments: &str, error: &str) -> String {
    doc("parent", &format!("do:
  - attempt:
      try:
        - spawn: {{ call: start_child, with: {{ {arguments} }} }}
      catch:
        errors: {{ with: {{ type: '{error}' }} }}
        do: [ {{ refused: {{ set: {{ refused: true }} }} }} ]"))
}

#[tokio::test]
async fn reserved_host_modes_are_validation_refusals_without_admission() {
    let time = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Config::default(), time.clone());
    save(&rt, "child", &doc("child", "do: [ { done: { set: { ok: true } } } ]")).await;
    for (i, mode) in ["null", "{}", "[]", "true", "42", "copy", "'${ {tenant: \"forged\"} }'", "'${ .host }'"].iter().enumerate() {
        let parent = reserved_catch(&format!("name: child, host: {mode}"), "https://open-workflow-specification.org/spec/1.0.0/errors/validation");
        save(&rt, "parent", &parent).await;
        let id = format!("p{i}");
        rt.start_with(&id, "parent", json!({ "host": { "forged": true } }), Start { host: json!({ "tenant": "real" }), ..Start::default() }).await.unwrap();
        assert_eq!(rt.drive(&id, &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({ "refused": true }))), "{mode}");
        assert!(rt.list(Some("child"), "").await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn reserved_child_admission_refusals_are_catchable() {
    for case in ["missing", "inadmissible", "depth", "paused", "tripped", "name", "link"] {
        let time = Arc::new(Time::default());
        let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
        let mut config = Config { breaker_threshold: 1, ..Config::default() };
        if case == "depth" { config.limits.max_depth = 0; }
        let mut registry = Registry::new();
        registry.register(Function::new("boom", json!({}), json!({}), |_: Value, _: FunctionContext| async {
            Err::<Value, _>(FunctionError::communication(500, "boom"))
        })).unwrap();
        let rt = runtime(store.clone(), registry, config, time.clone());
        if case != "missing" {
            save(&rt, "child", &doc("child", if case == "tripped" { "do: [ { fail: { call: boom } } ]" } else { "do: [ { wait: { wait: P1D } } ]" })).await;
        }
        if case == "inadmissible" {
            let version = store.save_definition("child", &doc("child", "do: [ { absent: { call: absent } } ]"), durable::Breaker::default()).await.unwrap().version;
            store.activate("child", version, 0).await.unwrap();
        }
        if case == "paused" { Control::new(store.clone(), time.clone()).pause(&operator("pause"), "child", "maintenance").await.unwrap(); }
        if case == "tripped" {
            rt.start("trip", "child", json!({})).await.unwrap();
            assert!(matches!(rt.drive("trip", &CancellationToken::new()).await.unwrap(), Some(Status::Faulted(_))));
        }
        let error = if case == "paused" || case == "tripped" { "urn:ouro:errors:definition.tripped" } else { "urn:ouro:errors:call.not_executed" };
        save(&rt, "parent", &reserved_catch("name: child, host: inherit", error)).await;
        rt.start_with("p", "parent", json!({}), Start { host: json!({ "tenant": "real" }), ..Start::default() }).await.unwrap();
        if case == "name" || case == "link" {
            let mut a = rt.get("p").await.unwrap().unwrap().admission;
            a.id = workflow_child_id("p", "/do/0/attempt/try/0/spawn");
            if case == "link" { a.name = "child".into(); }
            store.admit(&a, 0).await.unwrap();
        }
        assert_eq!(rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(json!({ "refused": true }))), "{case}");
        assert_eq!(rt.list(None, "child:").await.unwrap().len(), usize::from(case == "name" || case == "link"), "{case}: no new child");
    }
}

#[tokio::test]
async fn inherited_host_dry_runs_and_replays_use_private_admissions() {
    let time = Arc::new(Time::default());
    let seen = Seen::default();
    let mut registry = Registry::new();
    registry.register(whoami(seen.clone())).unwrap();
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let rt = runtime(store.clone(), registry, Config::default(), time.clone());
    save(&rt, "child", &doc("child", CHILD)).await;
    let parent = doc("parent", INHERITED_PARENT);
    let host = json!({ "opaque": ["private", 2] });
    let mock = ouro_runtime::mock(|_: &str, _: &Value| false, |_: &str, _: &Value, _: &FunctionContext| Ok(Value::Null));
    let dry = rt.dry_run_opts(&parent, json!({}), mock, DryRunOptions { host: Some(host.clone()), ..DryRunOptions::default() }, &CancellationToken::new()).await.unwrap();
    assert!(matches!(dry.status, Status::Completed(_)), "{:?}", dry.status);
    assert!(seen.lock().unwrap().iter().all(|(_, h)| h.as_ref() == Some(&host)));
    assert!(!seen.lock().unwrap().is_empty());
    assert!(store.list(None, "").await.unwrap().is_empty());
    save(&rt, "parent", &parent).await;
    rt.start_with("p", "parent", json!({}), Start { host, ..Start::default() }).await.unwrap();
    assert!(matches!(settle(&rt, &time, "p").await, Some(Status::Completed(_))));
    while rt.recover_once(&CancellationToken::new()).await.unwrap() > 0 {}
    let before = store.list(None, "").await.unwrap();
    let calls = seen.lock().unwrap().len();
    let replay = rt.replay("p", &CancellationToken::new()).await.unwrap();
    assert!(matches!(replay.status, Status::Completed(_)), "{:?}", replay.status);
    assert_eq!(store.list(None, "").await.unwrap(), before);
    assert_eq!(seen.lock().unwrap().len(), calls);
}

#[tokio::test]
async fn inherited_owned_children_survive_restart_with_their_version_and_cancel_with_parent() {
    for reserved in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.sqlite");
        let time = Arc::new(Time::default());
        let open = || runtime(Arc::new(durable::sqlite::open(&path).unwrap()), Registry::new(), Config::default(), time.clone());
        let rt = open();
        let version = save(&rt, "child", &doc("child", "do: [ { pause: { wait: P1D } } ]")).await;
        let body = if reserved { "do:\n  - spawn: { call: start_child, with: { name: child, host: inherit }, export: { as: '${ . }' } }\n  - join: { listen: { to: { one: { with: { type: '${ $context.done }' } } } } }" }
            else { "do:\n  - spawn: { metadata: { ouro: { host: inherit } }, run: { workflow: { namespace: test, name: child, version: latest } } }" };
        save(&rt, "parent", &doc("parent", body)).await;
        let host = json!({ "tenant": "kept" });
        rt.start_with("p", "parent", json!({}), Start { host: host.clone(), ..Start::default() }).await.unwrap();
        rt.recover_once(&CancellationToken::new()).await.unwrap();
        let before = rt.list(Some("child"), "").await.unwrap().pop().unwrap();
        assert_eq!(before.admission.host(), Some(&host));
        assert_eq!(before.admission.version, version);
        drop(rt);
        let rt = open();
        save(&rt, "child", &doc("child", "do: [ { changed: { set: { changed: true } } } ]")).await;
        time.advance(durable::LEASE_MS + 1);
        rt.recover_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(rt.get(&before.admission.id).await.unwrap().unwrap().admission, before.admission);
        rt.cancel("p").await.unwrap();
        assert_eq!(rt.get(&before.admission.id).await.unwrap().unwrap().outcome, Some(Status::Cancelled));
    }
}

#[tokio::test]
async fn inherited_workflows_wait_for_the_configured_application_version() {
    let store: Arc<dyn RunStore> = Arc::new(durable::memory::memory());
    let time = Arc::new(Time::default());
    let at = |version: &str| runtime(store.clone(), Registry::new(), Config { application: durable::Compatibility { version: Some(version.into()), accepts: vec![] }, ..Config::default() }, time.clone());
    let new = at("inherit-v2");
    save(&new, "child", &doc("child", "do: [ { done: { set: { ok: true } } } ]")).await;
    save(&new, "parent", &doc("parent", INHERITED_PARENT)).await;
    let host = json!({ "tenant": "fenced" });
    new.start_with("p", "parent", json!({}), Start { host: host.clone(), ..Start::default() }).await.unwrap();
    let old = at("before-inherit");
    assert_eq!(old.recover_once(&CancellationToken::new()).await.unwrap(), 0);
    assert!(old.list(Some("child"), "").await.unwrap().is_empty());
    assert!(matches!(settle(&new, &time, "p").await, Some(Status::Completed(_))));
    for child in new.list(Some("child"), "").await.unwrap() {
        assert_eq!(child.admission.host(), Some(&host));
        assert_eq!(child.admission.application_version(), Some("inherit-v2"));
    }
}

#[tokio::test]
async fn reserved_child_transient_resolution_interrupts_without_admission() {
    struct Resolver(Arc<std::sync::atomic::AtomicBool>);
    impl ouro_runtime::ResourceResolver for Resolver {
        fn resolve(&self, _: &str) -> anyhow::Result<Value> {
            anyhow::ensure!(!self.0.load(Ordering::SeqCst), "resolver offline");
            Ok(json!({ "type": "object" }))
        }
    }
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = Registry::new();
    let f = failed.clone();
    registry.register(Function::new("offline", json!({}), json!({}), move |_: Value, _: FunctionContext| {
        f.store(true, Ordering::SeqCst);
        async { Ok(json!({})) }
    })).unwrap();
    let time = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), registry, Config::default(), time.clone()).with_resources(Arc::new(Resolver(failed)));
    save(&rt, "child", &doc("child", "input: { schema: { format: json, resource: { endpoint: 'https://schema.test/input' } } }\ndo: [ { done: { set: { ok: true } } } ]")).await;
    let parent = reserved_catch("name: child, host: inherit", "https://open-workflow-specification.org/spec/1.0.0/errors/validation").replace("do:\n  - attempt:", "do:\n  - offline: { call: offline }\n  - attempt:");
    save(&rt, "parent", &parent).await;
    rt.start("p", "parent", json!({})).await.unwrap();
    let status = rt.drive("p", &CancellationToken::new()).await.unwrap();
    assert!(matches!(status, Some(Status::Interrupted(_))), "{status:?}");
    assert!(rt.list(Some("child"), "").await.unwrap().is_empty());
}

#[tokio::test]
async fn inherited_independent_child_outlives_its_starter() {
    let time = Arc::new(Time::default());
    let rt = runtime(Arc::new(durable::memory::memory()), Registry::new(), Config::default(), time.clone());
    save(&rt, "child", &doc("child", "do: [ { pause: { wait: P1D } } ]")).await;
    let parent = "do: [ { spawn: { metadata: { ouro: { host: inherit } }, run: { workflow: { namespace: test, name: child, version: latest }, await: false } } } ]";
    save(&rt, "parent", &doc("parent", parent)).await;
    let host = json!({ "tenant": "independent" });
    rt.start_with("p", "parent", json!({}), Start { host: host.clone(), ..Start::default() }).await.unwrap();
    assert!(matches!(rt.drive("p", &CancellationToken::new()).await.unwrap(), Some(Status::Completed(_))));
    let child = rt.list(Some("child"), "").await.unwrap().pop().unwrap();
    assert_eq!(child.admission.host(), Some(&host));
    assert!(child.child.is_none());
    assert!(child.root.is_none());
    assert!(child.outcome.is_none());
}
