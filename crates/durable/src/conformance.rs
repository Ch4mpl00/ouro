//! Adapter-independent safety and liveness checks. Every audit reloads persisted
//! state. Factories must return a fresh store for each case; reopening must
//! create an independent handle to the same database (memory may share state).
//! Based on the original admission, event-history, child and crash properties.
use crate::*;
use futures::future::BoxFuture;
use ows::run::WorkflowError;
use serde_json::json;
use std::collections::BTreeSet;

/// Opens another independent handle to the same database.
pub type Reopen = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Arc<dyn RunStore>>> + Send + Sync>;
/// Opens a read-only backend over the same database (`is_read_only`): the
/// adapter's read-only constructor, which never migrates or writes.
pub type OpenReadOnly = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Arc<dyn AtomicBackend>>> + Send + Sync>;

/// One case's store. Every operation is awaited on the caller's runtime,
/// whose flavour the suite does not assume; the interpreter cases run on a
/// paused clock of their own and reach the store through `Offloaded`.
#[derive(Clone)]
pub struct TestStore {
    pub store: Arc<dyn RunStore>,
    pub reopen: Reopen,
    pub faults: Arc<FaultInjector>,
    /// Read actual adapter rows, independently of the transition engine.
    pub backend: Arc<dyn AtomicBackend>,
    pub read_only: OpenReadOnly,
}
#[derive(Debug, Default)]
pub struct Report {
    pub cases: usize,
    pub operations: usize,
    pub crash_points: usize,
    pub execution_crashes: usize,
    pub witnesses: BTreeSet<&'static str>,
}
pub fn admission(id: &str) -> Admission {
    Admission {
        id: id.into(),
        name: "workflow".into(),
        version: 1,
        definition: "original".into(),
        input: json!({"n":1}),
        runtime: json!({}),
    }
}
/// An identified CloudEvent, as `deliver` requires.
pub fn event(id: &str, type_: &str, data: Value) -> Value {
    json!({"specversion":"1.0","source":"/conformance","id":id,"type":type_,"data":data})
}
async fn audit(store: &dyn RunStore) -> anyhow::Result<()> {
    for r in store.list(None, "").await? {
        let s = store.inspect(&r.admission.id).await?.unwrap();
        if r.ended_ms.is_some() {
            assert!(s.awaits.is_empty() && s.listeners.is_empty() && s.subscriptions.is_empty());
            assert!(r.fence.is_none());
            assert!(r.held.is_none(), "an ended run is never held");
        }
        // An owned child never outlives its parent: the parent's end, of any
        // kind, cancels it in the same commit (C3).
        if let Some(link) = &r.child
            && let Some(end) = store.get(&link.parent).await?.and_then(|p| p.ended_ms)
        {
            assert!(r.ended_ms.is_some_and(|c| c <= end), "owned child {} outlived its parent", r.admission.id);
        }
        let mut pending = BTreeSet::new();
        let mut consumed = BTreeSet::new();
        for e in &s.events {
            if let Some(k) = &e.consumed_key {
                assert!(consumed.insert(k));
                // Taken by a listen, or consumed by its observable expiry.
                let (Some(Record::Event { event }) | Some(Record::Expired { event })) = s.records.get(k) else {
                    panic!("consumption without journal evidence")
                };
                assert_eq!(event["type"], e.type_);
                assert_eq!(event["data"], e.data);
                assert_eq!(k.starts_with("mailbox#expired["), matches!(s.records[k], Record::Expired { .. }));
            } else if e.envelope.is_none() {
                assert!(pending.insert(&e.type_), "duplicate pending delivery");
            }
            if let Some(envelope) = &e.envelope {
                assert_eq!(envelope["type"], e.type_);
                assert_eq!(envelope.get("data").unwrap_or(&Value::Null), &e.data);
            }
        }
        for (k, r) in &s.records {
            if matches!(r, Record::Event { .. } | Record::Expired { .. }) {
                assert!(consumed.contains(k));
            }
        }
    }
    Ok(())
}

/// Run the full contract against any adapter. The factory label is suitable
/// for naming a fresh database. Panics are assertion failures; storage errors
/// are returned with their original cause.
pub async fn run_all(factory: impl AsyncFn(&str) -> anyhow::Result<TestStore>) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let t = factory("leases").await?;
    report.cases += 1;
    let s = &t.store;
    assert!(s.admit(&admission("r"), 0).await?);
    let mut changed = admission("r");
    changed.definition = "changed".into();
    changed.input = json!(2);
    assert!(!s.admit(&changed, 0).await?);
    changed.name = "other".into();
    assert!(s.admit(&changed, 0).await.is_err());
    assert_eq!(s.get("r").await?.unwrap().admission, admission("r"));
    let old = s.claim("r", 0).await?.unwrap();
    assert!(s.claim("r", 0).await?.is_none());
    s.renew(&old, 1).await?;
    let record = Record::Timer { deadline_ms: 100 };
    s.save_record(&old, "wait", &record, 2).await?;
    assert!(s.save_record(&old, "wait", &Record::Timer { deadline_ms: 200 }, 2).await.is_err());
    let reopened = (t.reopen)().await?;
    let current = reopened.claim("r", LEASE_MS + 2).await?.unwrap();
    assert!(s.load_records(&old, LEASE_MS + 2).await.is_err());
    assert!(s.renew(&old, LEASE_MS + 2).await.is_err());
    assert!(s.release(&old, LEASE_MS + 2).await.is_err());
    assert!(s.save_record(&old, "new", &record, LEASE_MS + 2).await.is_err());
    assert!(s.finish(&old, &Status::Completed(json!(0)), LEASE_MS + 2, 3).await.is_err());
    assert_eq!(reopened.load_records(&current, LEASE_MS + 2).await?.get("wait"), Some(&record));
    reopened.finish(&current, &Status::Completed(json!(42)), LEASE_MS + 2, 3).await?;
    assert!(reopened.due(i64::MAX, 100).await?.is_empty());
    assert!(reopened.claim("r", i64::MAX).await?.is_none());
    audit(&*reopened).await?;
    report.operations += 18;
    report.witnesses.insert("expired worker fenced after reopen");

    let t = factory("interruptions").await?;
    report.cases += 1;
    let s = &t.store;
    let mut now = 0;
    let mut f = s.start(&admission("r"), now).await?.unwrap();
    assert!(s.due(now, 100).await?.is_empty());
    assert!(s.start(&admission("r"), now).await?.is_none());
    for n in 1..=MAX_INTERRUPTIONS {
        let status = s.finish(&f, &Status::Interrupted(WorkflowError::runtime("corrupt")), now, 3).await?;
        audit(&**s).await?;
        report.operations += 1;
        if n == MAX_INTERRUPTIONS {
            assert!(matches!(status, Status::Faulted(_)));
            break;
        }
        let r = s.get("r").await?.unwrap();
        assert_eq!(r.interruptions, n);
        assert!(r.wake_at_ms > now);
        assert!(s.claim("r", r.wake_at_ms - 1).await?.is_none());
        now = r.wake_at_ms;
        f = s.claim("r", now).await?.unwrap();
    }
    report.witnesses.insert("interruptions back off and terminate");

    let t = factory("events").await?;
    report.cases += 1;
    let s = &t.store;
    let f = s.start(&admission("r"), 0).await?.unwrap();
    let types = vec!["answer".into()];
    s.register_wait(&f, &types, 0).await?;
    for k in ["first", "second"] {
        assert!(s.take_event(&f, k, &types, 0).await?.is_none());
    }
    assert_eq!(s.deliver(&event("1", "answer", json!(1)), None, 1).await?, Some("r".into()));
    assert!(s.deliver(&event("2", "answer", json!(2)), None, 1).await?.is_none());
    let first = s.take_event(&f, "first", &types, 2).await?.unwrap();
    assert_eq!(s.take_event(&f, "first", &types, 2).await?, Some(first));
    assert!(s.deliver(&event("2", "answer", json!(2)), None, 2).await?.is_some());
    // Delivery wins the race against the worker releasing as waiting.
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3).await?;
    let f = s.claim("r", 3).await?.unwrap();
    assert_eq!(s.take_event(&f, "second", &types, 3).await?.unwrap()["data"], 2);
    s.register_wait(&f, &types, 3).await?;
    s.take_event(&f, "timeout", &types, 3).await?;
    s.take_event(&f, "sibling", &types, 3).await?;
    s.unregister_wait(&f, "timeout", &types, 3).await?;
    assert!(s.deliver(&event("3", "answer", json!(3)), None, 3).await?.is_some());
    assert!(s.take_event(&f, "sibling", &types, 3).await?.is_some());
    s.finish(&f, &Status::Completed(json!(3)), 3, 3).await?;
    assert!(s.deliver(&event("4", "answer", json!(4)), None, 3).await?.is_none());
    audit(&**s).await?;
    report.operations += 20;
    report.witnesses.extend([
        "same-type concurrent listeners",
        "replayed consumption",
        "delivery wake survives release",
        "terminal cleanup",
    ]);

    let t = factory("published").await?;
    report.cases += 1;
    let s = &t.store;
    let cloud = |id: &str, t: &str| json!({"specversion":"1.0","id":id,"source":"/test","type":t,"data":{"n":id}});
    let a = s.start(&admission("a"), 0).await?.unwrap();
    let b = s.start(&admission("b"), 0).await?.unwrap();
    s.start(&admission("idle"), 0).await?.unwrap();
    s.subscribe(&a, "listen", &["reading".into()], 0).await?;
    s.subscribe(&b, "listen", &["*".into()], 0).await?;
    assert_eq!(s.publish(&cloud("1", "reading"), None, 1).await?, vec!["a".to_string(), "b".into()]);
    assert!(s.publish(&cloud("1", "reading"), None, 1).await?.is_empty(), "republishing is idempotent");
    assert_eq!(s.publish(&cloud("2", "reading"), None, 1).await?.len(), 2, "events of one type queue");
    assert_eq!(s.publish(&cloud("3", "other"), None, 1).await?, vec!["b".to_string()]);
    assert_eq!(s.publish(&cloud("4", "other"), Some("idle"), 1).await?, vec!["idle".to_string()]);
    assert!(s.publish(&json!({"data":1}), None, 1).await.is_err());
    // Subscriptions outlive consumption; takes keep publication order.
    for (k, n) in [("e0", "1"), ("e1", "2")] {
        assert_eq!(s.take_event(&a, k, &["reading".into()], 2).await?.unwrap()["data"]["n"], n);
    }
    assert_eq!(s.publish(&cloud("5", "reading"), None, 2).await?, vec!["a".to_string(), "b".into()]);
    // A wildcard takes published events, never targeted deliveries.
    s.register_wait(&b, &["answer".into()], 2).await?;
    s.deliver(&event("0", "answer", json!(0)), None, 2).await?;
    for (k, n) in [("w0", "1"), ("w1", "2"), ("w2", "3"), ("w3", "5")] {
        let taken = s.take_event(&b, k, &["*".into()], 3).await?.unwrap();
        assert_eq!((taken["data"]["n"].as_str(), taken["specversion"].as_str()), (Some(n), Some("1.0")));
    }
    assert!(s.take_event(&b, "w4", &["*".into()], 3).await?.is_none());
    // A published event wakes a waiting subscriber, even one already releasing.
    s.finish(&a, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3).await?;
    assert_eq!(s.get("a").await?.unwrap().wake_at_ms, 3, "a pending subscribed event wakes it");
    let a = s.claim("a", 3).await?.unwrap();
    s.take_event(&a, "e2", &["reading".into()], 3).await?.unwrap();
    s.finish(&a, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3).await?;
    assert_eq!(s.get("a").await?.unwrap().wake_at_ms, i64::MAX);
    s.publish(&cloud("6", "reading"), None, 4).await?;
    assert_eq!(s.get("a").await?.unwrap().wake_at_ms, 4);
    let a = s.claim("a", 4).await?.unwrap();
    s.unsubscribe(&a, "listen", 4).await?;
    assert_eq!(s.publish(&cloud("7", "reading"), None, 4).await?, vec!["b".to_string()], "unsubscribed");
    s.finish(&b, &Status::Completed(json!(0)), 5, 3).await?;
    assert!(s.publish(&cloud("9", "reading"), Some("b"), 5).await?.is_empty(), "terminal runs receive nothing");
    audit(&**s).await?;
    report.operations += 33;
    report.witnesses.extend([
        "published events reach every subscriber once",
        "subscriptions outlive consumption",
        "wildcards skip targeted deliveries",
    ]);

    let t = factory("triggers").await?;
    report.cases += 1;
    let s = &t.store;
    let timer = |at: Option<i64>, n: i64| Trigger { at_ms: at, types: Vec::new(), state: json!({ "n": n }) };
    let on = Trigger { at_ms: None, types: vec!["order".into(), "*".into()], state: json!({}) };
    assert!(s.swap_trigger("late", None, Some(&timer(Some(30), 0)), &[], None, 0).await?);
    assert!(s.swap_trigger("early", None, Some(&timer(Some(10), 0)), &[], None, 0).await?);
    assert!(!s.swap_trigger("early", None, Some(&timer(Some(5), 0)), &[], None, 0).await?, "it exists already");
    assert!(s.swap_trigger("on", None, Some(&on), &[], None, 0).await?);
    let due: Vec<String> = s.due_triggers(30, 10).await?.into_iter().map(|(k, _)| k).collect();
    assert_eq!(due, vec!["early".to_string(), "late".into()]);
    assert_eq!(s.due_triggers(9, 10).await?.len(), 0);
    assert_eq!(s.due_triggers(30, 1).await?.len(), 1);
    assert_eq!(s.triggers_for("order", 10).await?.len(), 1, "matched once though indexed twice");
    assert_eq!(s.triggers_for("other", 10).await?[0].0, "on", "a wildcard reacts to every type");
    assert!(s.triggers_for("order", 0).await?.is_empty());
    // Firing claims the slot and admits its run in one commit, once.
    let fired = admission("slot:10");
    let rearm = Rearm { trigger: "early".into(), delay_ms: 100 };
    let parked = timer(None, 1);
    assert!(
        s.swap_trigger(
            "early",
            Some(&timer(Some(10), 0)),
            Some(&parked),
            std::slice::from_ref(&fired),
            Some(&rearm),
            10
        )
        .await?
    );
    assert!(
        !s.swap_trigger("early", Some(&timer(Some(10), 0)), Some(&parked), std::slice::from_ref(&fired), None, 10)
            .await?
    );
    assert!(s.get("slot:10").await?.is_some());
    assert!(s.due_triggers(i64::MAX, 10).await?.iter().all(|(k, _)| k != "early"), "parked: not due");
    // The fired run's end makes the trigger due again after the delay.
    let f = s.claim("slot:10", 20).await?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 50, 3).await?;
    assert_eq!(s.trigger("early").await?, Some(timer(Some(150), 1)));
    assert!(s.swap_trigger("on", Some(&on), None, &[], None, 60).await?);
    assert!(s.trigger("on").await?.is_none() && s.triggers_for("order", 10).await?.is_empty());
    audit(&**s).await?;
    report.operations += 18;
    report.witnesses.extend(["triggers fire once and re-arm at a run's end"]);

    let t = factory("children").await?;
    report.cases += 1;
    let s = &t.store;
    let p = s.start(&admission("p"), 0).await?.unwrap();
    let link = ChildLink { parent: "p".into(), key: "round:1".into(), reference: "/start".into() };
    let c = s.start_child(&p, &admission("c"), &link, 0).await?.unwrap();
    s.finish(&c, &Status::Waiting { wake_at_ms: 0 }, 1, 3).await?;
    assert!(s.take_event(&p, "waiting", &["waiting:c".into()], 1).await?.is_some());
    let c = s.claim("c", 1).await?.unwrap();
    s.finish(&c, &Status::Completed(json!(42)), 2, 3).await?;
    assert!(s.take_event(&p, "done", &["done:c".into()], 2).await?.is_some());
    assert!(s.start_child(&p, &admission("c"), &link, 2).await?.is_none());
    assert!(s.take_event(&p, "again", &["done:c".into()], 2).await?.is_none());
    assert!(s.child_outcome("other", "round:1", "c").await.is_err());
    assert_eq!(s.child_outcome("p", "round:1", "c").await?, Some(Status::Completed(json!(42))));
    let c = s
        .start_child(
            &p,
            &admission("active"),
            &ChildLink { parent: "p".into(), key: "2".into(), reference: "/start".into() },
            2,
        )
        .await?
        .unwrap();
    s.start_child(
        &c,
        &admission("nested"),
        &ChildLink { parent: "active".into(), key: "3".into(), reference: "/start".into() },
        2,
    )
    .await?;
    s.finish(&p, &Status::Completed(json!(0)), 3, 3).await?;
    for id in ["active", "nested"] {
        assert_eq!(s.get(id).await?.unwrap().outcome, Some(Status::Cancelled));
    }
    audit(&**s).await?;
    report.operations += 16;
    report.witnesses.insert("child completion and cascading cancellation");

    let t = factory("retention").await?;
    report.cases += 1;
    let s = &t.store;
    for id in ["old", "recent", "waiting"] {
        let f = s.start(&admission(id), 0).await?.unwrap();
        s.save_record(&f, "k", &record, 0).await?;
        s.finish(
            &f,
            &if id == "waiting" { Status::Waiting { wake_at_ms: i64::MAX } } else { Status::Completed(json!(id)) },
            if id == "old" { 0 } else { 100 },
            3,
        )
        .await?;
    }
    assert_eq!(s.sweep(100, 50).await?.records, 1);
    assert!(s.inspect("old").await?.unwrap().records.is_empty());
    assert!(!s.inspect("recent").await?.unwrap().records.is_empty());
    assert!(!s.inspect("waiting").await?.unwrap().records.is_empty());
    assert!(s.claim("old", 100).await?.is_none());
    audit(&**s).await?;
    report.operations += 14;
    report.witnesses.insert("retention keeps terminal outcome");

    let t = factory("definitions").await?;
    report.cases += 1;
    let s = &t.store;
    let first = s.save_definition("workflow", "first").await?;
    s.activate("workflow", first.version, 0).await?;
    let second = s.save_definition("workflow", "second").await?;
    s.activate("workflow", second.version, 0).await?;
    let pinned = s.get("absent").await?;
    assert!(pinned.is_none());
    // A run pinned at the older version while the second is active: it
    // records the pin, has no epoch, and its fault is not the breaker's
    // observation.
    let mut pinned_at_first = admission("pinned");
    pinned_at_first.version = first.version;
    pinned_at_first.definition = first.yaml.clone();
    pinned_at_first.runtime = json!({ PINNED: true });
    let f = s.start(&pinned_at_first, 0).await?.unwrap();
    let seen = s.inspect_control("pinned").await?.unwrap();
    assert!(seen.pinned && seen.snapshot.run.admission.version == first.version && seen.snapshot.run.epoch.is_none());
    s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3).await?;
    assert_eq!(s.definitions("workflow").await?.consecutive_faults, 0);
    for n in 0..3 {
        let mut a = admission(&format!("fault{n}"));
        a.version = second.version;
        a.definition = second.yaml.clone();
        let f = s.start(&a, 0).await?.unwrap();
        s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3).await?;
    }
    let d = s.definitions("workflow").await?;
    assert_eq!((d.active, d.epoch), (Some(second.version), 2), "a trip keeps the active version and the epoch");
    let trip = d.tripped.expect("tripped at the threshold");
    assert_eq!((trip.version, trip.epoch, trip.faults, trip.threshold), (second.version, 2, 3, 3));
    assert_eq!(trip.run, "fault2");
    assert_eq!(s.get("fault0").await?.unwrap().admission.version, second.version);
    assert!(s.activate("workflow", 99, 0).await.is_err());
    // A trip refuses an admission by content digest: a pinned start of other
    // content is admitted and never counted, the same content under another
    // version number is refused.
    assert_eq!(trip.digest, digest_of("second"));
    let mut again = pinned_at_first.clone();
    again.id = "pinned-again".into();
    let f = s.start(&again, 0).await?.expect("other content is admitted while tripped");
    let seen = s.inspect_control("pinned-again").await?.unwrap();
    assert!(seen.pinned && seen.snapshot.run.epoch.is_none());
    assert_eq!(seen.digest, digest_of("first"));
    s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3).await?;
    let after = s.definitions("workflow").await?;
    assert_eq!((after.consecutive_faults, after.tripped.as_ref()), (3, Some(&trip)), "not counted");
    assert_eq!(s.notices(10).await?.len(), 1, "no second notice");
    let copy = s.save_definition("workflow", "second").await?;
    assert_eq!(copy.digest, second.digest);
    let mut same = admission("same-content");
    same.version = copy.version;
    same.definition = copy.yaml.clone();
    same.runtime = json!({ PINNED: true });
    assert!(s.start(&same, 0).await.unwrap_err().is::<DefinitionTripped>(), "same content, other number");
    assert!(s.get("same-content").await?.is_none());
    assert!(s.inspect_control("pinned").await?.unwrap().pinned, "the run admitted before the trip is kept");
    let versions = s.definitions("workflow").await?.versions;
    assert_eq!(
        versions.values().map(|v| v.digest.clone()).collect::<Vec<_>>(),
        [digest_of("first"), digest_of("second"), digest_of("second")]
    );
    report.operations += 6;
    report
        .witnesses
        .insert("a trip refuses by content digest: other content is admitted uncounted, same content is not");
    report.operations += 2;
    report.witnesses.insert("a pinned run is not the breaker's observation");
    report.operations += 13;
    report.witnesses.insert("breaker trips without changing the active version or pinned runs");
    decisions(&factory, &mut report).await?;

    // Inject before each of the first row writes, before commit, and after commit.
    // Child start touches both parent and child. Terminal finish may also
    // touch definitions and every live descendant; each point gets a fresh DB.
    for operation in [
        "admit",
        "record",
        "register",
        "take",
        "deliver",
        "child",
        "finish",
        "cancel",
        "sweep",
        "release",
        "renew",
        "definition",
        "activate",
        "subscribe",
        "publish",
        "trigger",
        "mailbox",
        "expire",
        "ingress",
        "arm",
        "trip",
        "refuse",
        "acknowledge",
    ] {
        for point in [
            CommitFault::BeforeWrite(0),
            CommitFault::BeforeWrite(1),
            CommitFault::BeforeWrite(2),
            CommitFault::BeforeWrite(3),
            CommitFault::BeforeCommit,
            CommitFault::AfterCommit,
        ] {
            let t = factory("crash").await?;
            report.cases += 1;
            let s = &t.store;
            let f = s.start(&admission("p"), 0).await?.unwrap();
            s.register_wait(&f, &["answer".into()], 0).await?;
            s.take_event(&f, "event", &["answer".into()], 0).await?;
            s.subscribe(&f, "listen", &["answer".into()], 0).await?;
            if operation == "take" {
                s.deliver(&event("42", "answer", json!(42)), None, 0).await?;
            }
            if operation == "sweep" {
                s.save_record(&f, "timer", &record, 0).await?;
                s.finish(&f, &Status::Completed(json!(0)), 0, 3).await?;
            }
            if operation == "expire" {
                s.deliver(&event("expiring", "answer", json!(1)), Some("p"), 0).await?;
            }
            let d = s.save_definition("workflow", "yaml").await?;
            let mut tripping = None;
            let mut notice = None;
            if matches!(operation, "trip" | "refuse" | "acknowledge") {
                // An activation with an armed trigger and a run of its epoch
                // whose fault trips the breaker (removing the trigger and
                // storing the notice); `acknowledge` then removes the notice.
                let arming = Arming {
                    triggers: BTreeMap::from([(
                        "armed".into(),
                        Trigger { at_ms: Some(9), types: vec!["t".into()], state: json!(2) },
                    )]),
                };
                let epoch = s.definitions("workflow").await?.epoch;
                s.activate_with("workflow", d.version, epoch, &arming, &Activation::at(0)).await?;
                tripping = s.start(&admission("tripping"), 0).await?;
                if operation == "acknowledge" {
                    s.finish(tripping.as_ref().unwrap(), &Status::Faulted(WorkflowError::runtime("fault")), 0, 1)
                        .await?;
                    notice = s.notices(1).await?.pop().map(|(k, _)| k);
                }
            }
            if matches!(operation, "finish" | "cancel") {
                s.activate("workflow", d.version, 0).await?;
                let child = s
                    .start_child(
                        &f,
                        &admission("active"),
                        &ChildLink { parent: "p".into(), key: "child".into(), reference: "/child".into() },
                        0,
                    )
                    .await?
                    .unwrap();
                s.start_child(
                    &child,
                    &admission("nested"),
                    &ChildLink { parent: "active".into(), key: "nested".into(), reference: "/nested".into() },
                    0,
                )
                .await?;
            }
            let epoch = s.definitions("workflow").await?.epoch;
            let before = t.backend.snapshot().await?;
            t.faults.arm(point);
            let result: anyhow::Result<()> = match operation {
                "admit" => s.admit(&admission("new"), 0).await.map(|_| ()),
                "record" => s.save_record(&f, "timer", &record, 0).await,
                "register" => s.register_wait(&f, &["another".into()], 0).await,
                "take" => s.take_event(&f, "event", &["answer".into()], 0).await.map(|_| ()),
                "deliver" => s.deliver(&event("42", "answer", json!(42)), None, 0).await.map(|_| ()),
                "child" => s
                    .start_child(
                        &f,
                        &admission("c"),
                        &ChildLink { parent: "p".into(), key: "k".into(), reference: "/child".into() },
                        0,
                    )
                    .await
                    .map(|_| ()),
                "finish" => s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3).await.map(|_| ()),
                "cancel" => s.cancel("p", 0).await.map(|_| ()),
                "sweep" => s.sweep(1, 0).await.map(|_| ()),
                "release" => s.release(&f, 0).await,
                "renew" => s.renew(&f, 1).await,
                "definition" => s.save_definition("workflow", "next").await.map(|_| ()),
                "activate" => s.activate("workflow", d.version, 0).await,
                "subscribe" => s.subscribe(&f, "other", &["*".into()], 0).await,
                "publish" => s.publish(&json!({"type":"answer","source":"/t","id":"1"}), None, 0).await.map(|_| ()),
                "trigger" => s
                    .swap_trigger(
                        "slot",
                        None,
                        Some(&Trigger { at_ms: Some(5), types: vec!["answer".into()], state: json!(1) }),
                        &[admission("fired")],
                        Some(&Rearm { trigger: "slot".into(), delay_ms: 1 }),
                        0,
                    )
                    .await
                    .map(|_| ()),
                "mailbox" => s.deliver(&event("early", "early", json!(1)), Some("p"), 0).await.map(|_| ()),
                "expire" => s.sweep(MAILBOX_KEPT_MS, RECORDS_KEPT_MS).await.map(|_| ()),
                "ingress" => s
                    .fire(
                        &Firing {
                            key: "on",
                            next: Some(&Trigger { at_ms: None, types: vec!["order".into()], state: json!([]) }),
                            admit: &[admission("ingress")],
                            ingress: Some(&event("order", "order", json!(1))),
                            ..Firing::default()
                        },
                        0,
                    )
                    .await
                    .map(|_| ()),
                "arm" => s
                    .activate_with(
                        "workflow",
                        d.version,
                        epoch,
                        &Arming {
                            triggers: BTreeMap::from([(
                                "armed".into(),
                                Trigger { at_ms: Some(5), types: vec!["t".into()], state: json!(1) },
                            )]),
                        },
                        &Activation::at(0),
                    )
                    .await
                    .map(|_| ()),
                "trip" => s
                    .finish(tripping.as_ref().unwrap(), &Status::Faulted(WorkflowError::runtime("fault")), 0, 1)
                    .await
                    .map(|_| ()),
                // A refused slot whose fault trips the breaker: the run, the
                // moved trigger, the count, the trip, the trigger's removal
                // and the notice, all in one commit.
                "refuse" => s
                    .fire(
                        &Firing {
                            key: "armed",
                            expected: Some(&Trigger { at_ms: Some(9), types: vec!["t".into()], state: json!(2) }),
                            next: Some(&Trigger { at_ms: Some(19), types: vec!["t".into()], state: json!(3) }),
                            refused: Some(&Refused {
                                admission: admission("refused"),
                                error: WorkflowError::validation("invalid"),
                                breaker_threshold: 1,
                            }),
                            ..Firing::default()
                        },
                        9,
                    )
                    .await
                    .map(|_| ()),
                "acknowledge" => s.acknowledge(notice.as_ref().unwrap()).await.map(|_| ()),
                _ => unreachable!(),
            };
            t.faults.clear();
            let after = t.backend.snapshot().await?;
            if result.is_err() && point != CommitFault::AfterCommit {
                assert_eq!(before, after, "partial commit: {operation} at {point:?}");
                report.crash_points += 1;
            }
            if point == CommitFault::AfterCommit {
                assert!(result.is_err());
                assert_ne!(before, after);
                report.crash_points += 1;
            }
            let reopened = (t.reopen)().await?;
            audit(&*reopened).await?;
            report.operations += 1;
        }
    }
    report.witnesses.insert("rollback at each write and ambiguous committed error");

    // Generated overlapping event histories. Check actual stored runs
    // after EVERY operation, then drain surviving listeners to completion.
    for seed in 1..=24u64 {
        let t = factory("history").await?;
        report.cases += 1;
        let mut s = t.store.clone();
        let mut now = 0;
        let mut rng = seed;
        let mut leases = Vec::new();
        for id in ["r0", "r1", "r2"] {
            leases.push(s.start(&admission(id), now).await?);
        }
        for step in 0..80 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let r = usize::try_from((rng >> 32) % 3)?;
            let op = (rng >> 40) % 9;
            let id = format!("r{r}");
            let k = format!("listen{}", (rng >> 48) % 4);
            let types = vec![format!("type{}", (rng >> 52) % 3)];
            match op {
                0 | 1 => {
                    if let Some(f) = &leases[r] {
                        s.register_wait(f, &types, now).await?;
                        s.take_event(f, &k, &types, now).await?;
                    }
                }
                2 | 3 => {
                    s.deliver(&event(&format!("step{step}"), &types[0], json!({"step":step})), None, now).await?;
                }
                4 => {
                    if let Some(f) = &leases[r] {
                        s.unregister_wait(f, &k, &types, now).await?;
                    }
                }
                5 => {
                    s.cancel(&id, now).await?;
                    leases[r] = None;
                }
                6 => {
                    s = (t.reopen)().await?;
                    now += LEASE_MS + 1;
                    leases = Vec::new();
                    for r in 0..3 {
                        leases.push(s.claim(&format!("r{r}"), now).await?);
                    }
                    report.witnesses.insert("generated reopen and lease replacement");
                }
                7 => {
                    s.sweep(now, 0).await?;
                }
                8 => {
                    let before = t.backend.snapshot().await?;
                    t.faults.arm(CommitFault::BeforeCommit);
                    assert!(
                        s.deliver(&event(&format!("failed{step}"), &types[0], json!(step)), None, now).await.is_err()
                    );
                    t.faults.clear();
                    assert_eq!(t.backend.snapshot().await?, before);
                }
                _ => unreachable!(),
            }
            audit(&*s).await?;
            report.operations += 1;
        }
        for f in leases.iter().flatten() {
            let listeners = s.inspect(&f.run_id).await?.unwrap().listeners;
            for (key, types) in listeners {
                let mut taken = false;
                for attempt in 0..3 {
                    for t in &types {
                        let id = format!("drain:{}:{key}:{t}:{attempt}", f.run_id);
                        s.deliver(&event(&id, t, json!({"drain":true})), None, now).await?;
                    }
                    taken = s.take_event(f, &key, &types, now).await?.is_some();
                    audit(&*s).await?;
                    report.operations += 1;
                    if taken {
                        break;
                    }
                }
                assert!(taken, "listener stranded after finite history");
            }
            s.finish(f, &Status::Completed(json!({})), now, 3).await?;
        }
        assert!(s.due(i64::MAX, 100).await?.is_empty());
        audit(&*s).await?;
    }
    report.witnesses.insert("finite histories drain every live listener");
    assert_eq!(report.witnesses.len(), 29);
    let t = factory("concurrent-claims").await?;
    report.cases += 1;
    t.store.admit(&admission("r"), 0).await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let reopen = t.reopen.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let store = reopen().await?;
                barrier.wait().await;
                anyhow::Ok(store.claim("r", 0).await?.is_some())
            })
        })
        .collect();
    let mut winners = 0;
    for worker in workers {
        winners += usize::from(worker.await.expect("claim worker panicked")?);
    }
    assert_eq!(winners, 1);
    audit(&*t.store).await?;
    report.witnesses.insert("concurrent claims have one winner");
    let t = factory("corruption").await?;
    report.cases += 1;
    t.store.start(&admission("r"), 0).await?;
    let before = t.backend.snapshot().await?;
    let mut corrupt = before.rows.clone();
    let bad = (crate::engine::records_of("r"), "bad".to_string());
    corrupt.insert(bad.clone(), json!("invalid record"));
    assert!(t.backend.compare_exchange(&before, &corrupt).await?);
    let reopened = (t.reopen)().await?;
    let fence = reopened.claim("r", LEASE_MS + 1).await?.unwrap();
    assert!(reopened.load_records(&fence, LEASE_MS + 1).await.is_err());
    assert!(reopened.save_record(&fence, "bad", &Record::Timer { deadline_ms: 1 }, LEASE_MS + 1).await.is_err());
    assert!(reopened.cancel("r", LEASE_MS + 1).await?);
    assert_eq!(reopened.get("r").await?.unwrap().outcome, Some(Status::Cancelled));
    assert_eq!(t.backend.snapshot().await?.rows[&bad], corrupt[&bad]);
    report.witnesses.insert("corruption fails closed without erasure");
    for mode in 0..4 {
        let t = factory("listen-baseline").await?;
        let saves = paused(t, mode, usize::MAX, false, false).await?.0;
        report.cases += 1;
        for after in [false, true] {
            for fail in 0..saves {
                let t = factory("listen-checkpoint").await?;
                let (_, interrupted) = paused(t, mode, fail, after, false).await?;
                assert!(interrupted, "checkpoint was not reached: {mode}/{fail}/{after}");
                report.execution_crashes += 1;
                report.cases += 1;
            }
        }
        if mode < 2 {
            let t = factory("listen-atomic-take").await?;
            assert!(paused(t, mode, usize::MAX, false, true).await?.1);
            report.execution_crashes += 1;
            report.cases += 1;
        }
    }
    report
        .witnesses
        .extend(["listen recovery before and after every interpreter commit", "held delivery and catchable timeout"]);
    budgets(&factory, &mut report).await?;
    overlap_policies(&factory, &mut report).await?;
    controls(&factory, &mut report).await?;
    holds(&factory, &mut report).await?;
    control_overlap(&factory, &mut report).await?;
    queue_order_after_clear(&factory, &mut report).await?;
    definition_queries(&factory, &mut report).await?;
    heartbeats(&factory, &mut report).await?;
    annotation_totals(&factory, &mut report).await?;
    call_outcomes(&factory, &mut report).await?;
    read_only(&factory, &mut report).await?;
    dropped(&factory, &mut report).await?;
    Ok(report)
}

/// A combined schedule exercises each kind without a runtime or registry.
pub fn control_definition_yaml() -> &'static str {
    "document: { dsl: '1.0.4', namespace: test, name: workflow, version: '1.0.0', metadata: { overlap: queue, catchup: none } }
schedule:
  every: PT1S
  cron: '0 * * * * *'
  after: PT2S
  on: { one: { with: { type: start } } }
do:
  - ok: { set: { ok: true } }
"
}
async fn definition_queries(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::{Command, QueuedSlot, ScheduleCatchup, ScheduleKind, ScheduleSummary, VersionSummary};
    let t = factory("control-definitions").await?;
    report.cases += 1;
    let s = &t.store;
    assert!(s.query_definitions(None, 1).await?.items.is_empty());
    assert!(s.definition_control("missing").await?.is_none());
    // Unactivated versions need no parsing; summaries never return their YAML.
    s.save_definition("workflow", "old").await?;
    s.save_definition("workflow", control_definition_yaml()).await?;
    for name in ["z", "a:#", "é"] {
        s.save_definition(name, "unactivated").await?;
    }
    let slot = |kind: &str, at_ms| Trigger {
        at_ms,
        types: if kind == "on" { vec!["start".into()] } else { vec![] },
        state: json!({"kind":kind, "epoch":1, "name":"workflow"}),
    };
    let every = slot("every", Some(10));
    let arming = Arming {
        triggers: BTreeMap::from([
            ("custom-every".into(), every.clone()),
            ("custom-cron".into(), slot("cron", Some(20))),
            ("custom-after".into(), slot("after", Some(30))),
            ("custom-on".into(), slot("on", None)),
        ]),
    };
    let release = Activation { at_ms: 5, actor: Some("operator".into()), note: Some("release 2".into()) };
    s.activate_with("workflow", 2, 0, &arming, &release).await?.unwrap();
    let mut first = admission("first");
    first.version = 2;
    let mut buffered = first.clone();
    buffered.id = "buffered-slot".into();
    let next = slot("every", Some(40));
    let fire = async |expected: &Trigger, a: &Admission| {
        s.fire(
            &Firing {
                key: "custom-every",
                expected: Some(expected),
                next: Some(&next),
                busy: Some(&next),
                admit: std::slice::from_ref(a),
                overlap: Some(OverlapGuard { policy: Overlap::Queue, max_concurrent_runs: 10, ceiling: &next }),
                ..Firing::default()
            },
            0,
        )
        .await
    };
    assert_eq!(fire(&every, &first).await?, Fired::Committed);
    assert_eq!(fire(&next, &buffered).await?, Fired::Queued);
    let mut fault = first.clone();
    fault.id = "fault".into();
    let f = s.start(&fault, 0).await?.unwrap();
    let error = Status::Faulted(WorkflowError::runtime("fault"));
    s.finish(&f, &error, 1, 3).await?;
    let fresh = (t.reopen)().await?;
    let before = t.backend.snapshot().await?;
    let summary = fresh.definition_control("workflow").await?.unwrap();
    assert_eq!(summary.name, "workflow");
    let never = VersionSummary {
        version: 1,
        digest: digest_of("old"),
        activated_ms: None,
        activated_epoch: None,
        activated_by: None,
        activation_note: None,
    };
    let released = VersionSummary {
        version: 2,
        digest: digest_of(control_definition_yaml()),
        activated_ms: Some(5),
        activated_epoch: Some(1),
        activated_by: Some("operator".into()),
        activation_note: Some("release 2".into()),
    };
    assert_eq!(summary.versions, vec![never.clone(), released]);
    assert_eq!((summary.active, summary.epoch, summary.consecutive_faults), (Some(2), 1, 1));
    assert!(summary.tripped.is_none());
    assert_eq!(
        summary.schedules,
        vec![
            ScheduleSummary {
                kind: ScheduleKind::Every,
                next_due_ms: Some(40),
                overlap: Overlap::Queue,
                catchup: ScheduleCatchup::None,
                queued: Some(QueuedSlot { run: buffered.id.clone(), version: 2 })
            },
            ScheduleSummary {
                kind: ScheduleKind::Cron,
                next_due_ms: Some(20),
                overlap: Overlap::Queue,
                catchup: ScheduleCatchup::None,
                queued: None
            },
            ScheduleSummary {
                kind: ScheduleKind::After,
                next_due_ms: Some(30),
                overlap: Overlap::Queue,
                catchup: ScheduleCatchup::None,
                queued: None
            },
            ScheduleSummary {
                kind: ScheduleKind::On,
                next_due_ms: None,
                overlap: Overlap::Queue,
                catchup: ScheduleCatchup::None,
                queued: None
            },
        ]
    );
    let mut cursor = None;
    let mut names = Vec::new();
    loop {
        let page = fresh.query_definitions(cursor.as_ref(), 1).await?;
        for item in page.items {
            if item.name == "workflow" {
                assert_eq!(item, summary);
            } else {
                assert_eq!((item.active, item.epoch, item.consecutive_faults), (None, 0, 0));
                assert!(item.schedules.is_empty() && item.tripped.is_none());
            }
            names.push(item.name);
        }
        cursor = page.next.map(|c| serde_json::from_value(serde_json::to_value(c).unwrap()).unwrap());
        if cursor.is_none() {
            break;
        }
        assert!(fresh.workers(cursor.as_ref(), 1).await.is_err());
        assert!(fresh.query_runs(&control::Runs::default(), false, cursor.as_ref(), 1).await.is_err());
    }
    assert_eq!(names, ["a:#", "workflow", "z", "é"]);
    for limit in [0, control::MAX_PAGE + 1, usize::MAX] {
        assert!(fresh.query_definitions(None, limit).await.is_err());
    }
    assert_eq!(fresh.query_definitions(None, control::MAX_PAGE).await?.items.len(), 4);
    assert_eq!(t.backend.snapshot().await?, before, "observations must not write");
    for fault in [CommitFault::BeforeCommit, CommitFault::AfterCommit] {
        t.faults.arm(fault);
        assert!(fresh.query_definitions(None, 10).await.is_err());
        t.faults.clear();
        t.faults.arm(fault);
        assert!(fresh.definition_control("workflow").await.is_err());
        t.faults.clear();
        assert_eq!(t.backend.snapshot().await?, before);
    }
    let pause = Command::Pause { definition: "workflow".into(), note: "investigate".into() };
    s.control(&control_request("pause"), &pause, 2).await?;
    let paused = fresh.definition_control("workflow").await?.unwrap();
    assert_eq!(paused.schedules, summary.schedules);
    assert_eq!((paused.epoch, paused.consecutive_faults), (1, 1));
    assert_eq!(
        paused.tripped.as_ref().unwrap().manual,
        Some(control::ManualTrip {
            actor: "operator".into(),
            reason: "maintenance".into(),
            note: "investigate".into(),
        })
    );
    assert_eq!(fresh.query_definitions(None, 10).await?.items[1], paused);
    s.control(&control_request("resume"), &Command::Resume { definition: "workflow".into() }, 3).await?;
    assert_eq!(fresh.definition_control("workflow").await?, Some(summary));
    let f = s.claim(&first.id, 3).await?.unwrap();
    s.finish(&f, &error, 4, 3).await?;
    let promoted = fresh.definition_control("workflow").await?.unwrap();
    assert!(promoted.schedules.iter().all(|s| s.queued.is_none()));
    assert_eq!(promoted.consecutive_faults, 2);
    let f = s.claim(&buffered.id, 4).await?.unwrap();
    s.finish(&f, &error, 5, 3).await?;
    let tripped = fresh.definition_control("workflow").await?.unwrap();
    assert_eq!(tripped.consecutive_faults, 3);
    assert_eq!(tripped.tripped, fresh.definitions("workflow").await?.tripped);
    assert!(tripped.tripped.as_ref().unwrap().manual.is_none());
    assert!(tripped.schedules.iter().all(|s| s.next_due_ms.is_none() && s.queued.is_none()));
    // Only an activation clears an automatic trip; resuming it is refused.
    let resume = Command::Resume { definition: "workflow".into() };
    assert!(matches!(
        refusal(s.control(&control_request("resume-trip"), &resume, 5).await),
        control::ControlError::InvalidState(_)
    ));
    s.activate_with("workflow", 2, 1, &arming, &Activation::at(6)).await?.unwrap();
    let reactivated = fresh.definition_control("workflow").await?.unwrap();
    assert_eq!((reactivated.epoch, reactivated.consecutive_faults), (2, 0));
    // A reactivation is the version's latest activation, unattributed here.
    let again = VersionSummary { activated_ms: Some(6), activated_epoch: Some(2), ..never.clone() };
    assert_eq!(
        reactivated.versions,
        vec![never.clone(), VersionSummary { version: 2, digest: digest_of(control_definition_yaml()), ..again }]
    );
    assert!(reactivated.tripped.is_none());
    assert_eq!(reactivated.schedules[0].next_due_ms, Some(10));
    // A page racing activation must observe the old catalog and its triggers,
    // or the new catalog and its triggers, never a mixture of the two.
    let no_schedule = "document: { dsl: '1.0.4', namespace: test, name: workflow, version: '1.0.0' }\ndo:\n  - ok: { set: { ok: true } }\n";
    s.save_definition("workflow", no_schedule).await?;
    let old = fresh.definition_control("workflow").await?.unwrap();
    let unarmed = Arming::default();
    let at_zero = Activation::at(0);
    let (page, activated) =
        tokio::join!(fresh.query_definitions(None, 10), s.activate_with("workflow", 3, 2, &unarmed, &at_zero));
    activated?.unwrap();
    let new = fresh.definition_control("workflow").await?.unwrap();
    assert_eq!((new.active, new.epoch), (Some(3), 3));
    assert!(new.schedules.is_empty());
    // Activating another version keeps the earlier one's latest activation.
    assert_eq!(new.versions[1].activated_epoch, Some(2));
    assert_eq!((new.versions[2].activated_epoch, new.versions[2].activated_ms), (Some(3), Some(0)));
    let observed = &page?.items[1];
    assert!(*observed == old || *observed == new);
    // Persisted layout validation is shared by point and paged catalog reads.
    let before = t.backend.snapshot().await?;
    let mut corrupt = before.rows.clone();
    corrupt.get_mut(&("definitions".into(), "workflow".into())).unwrap()["version"] = json!(0);
    assert!(t.backend.compare_exchange(&before, &corrupt).await?);
    assert!(fresh.definition_control("workflow").await.is_err());
    assert!(fresh.query_definitions(None, 10).await.is_err());
    let after = t.backend.snapshot().await?;
    assert_eq!(after.rows, corrupt);
    assert!(t.backend.compare_exchange(&after, &before.rows).await?);
    report.witnesses.insert("bounded definition metadata, suspended queues and breaker reports survive reopen");
    Ok(())
}

/// Worker heartbeats: one row per worker id however often it beats, an
/// older beat never replaces a newer one, pages in id order, and a stale
/// row is removed by `sweep` only once it has been stale for
/// `WORKERS_KEPT_MS`, together with its expiry index entry.
async fn heartbeats(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::{WORKERS_KEPT_MS, Worker};
    let t = factory("heartbeats").await?;
    report.cases += 1;
    let s = &t.store;
    let worker = |id: &str, beat_ms: i64, held: u64| Worker {
        id: id.into(),
        host: Some("host-a".into()),
        application: Some("1".into()),
        started_ms: 0,
        beat_ms,
        expires_ms: beat_ms + LEASE_MS,
        held,
    };
    s.beat(&worker("a", 0, 1)).await?;
    s.beat(&worker("b", 0, 0)).await?;
    s.beat(&worker("a", 10, 3)).await?;
    // A delayed beat arriving after a newer one changes nothing.
    s.beat(&worker("a", 5, 9)).await?;
    assert!(s.beat(&worker("", 0, 0)).await.is_err());
    let rows = async || -> anyhow::Result<(usize, usize)> {
        let rows = t.backend.snapshot().await?.rows;
        let count = |kind: &str| rows.keys().filter(|(k, _)| k == kind).count();
        Ok((count("worker"), count("worker_expiry")))
    };
    assert_eq!(rows().await?, (2, 2));
    let fresh = (t.reopen)().await?;
    let first = fresh.workers(None, 1).await?;
    assert_eq!(first.items, vec![worker("a", 10, 3)]);
    let second = fresh.workers(first.next.as_ref(), 1).await?;
    assert_eq!(second.items, vec![worker("b", 0, 0)]);
    assert!(fresh.workers(second.next.as_ref(), 1).await?.items.is_empty());
    assert!(fresh.workers(None, 0).await.is_err());
    // `b` expired at LEASE_MS and `a` at 10 + LEASE_MS.
    assert_eq!(fresh.sweep(LEASE_MS + WORKERS_KEPT_MS - 1, RECORDS_KEPT_MS).await?.workers, 0);
    assert_eq!(fresh.sweep(LEASE_MS + WORKERS_KEPT_MS, RECORDS_KEPT_MS).await?.workers, 1);
    assert_eq!(fresh.workers(None, 10).await?.items, vec![worker("a", 10, 3)]);
    // A beat revives a stale row before it is swept.
    s.beat(&worker("a", WORKERS_KEPT_MS, 0)).await?;
    assert_eq!(fresh.sweep(10 + LEASE_MS + WORKERS_KEPT_MS, RECORDS_KEPT_MS).await?.workers, 0);
    assert_eq!(fresh.sweep(2 * WORKERS_KEPT_MS + LEASE_MS, RECORDS_KEPT_MS).await?.workers, 1);
    assert_eq!(rows().await?, (0, 0));
    report.operations += 12;
    report.witnesses.insert("one heartbeat row per worker, swept after it stays stale");
    Ok(())
}

/// Per-run annotation totals (#22): the run row sums its calls' numeric
/// annotations in the commit that saves each answer, a replayed save adds
/// nothing, texts are ignored, the names are bounded like one call's, and
/// listings, history and inspection return them, after `sweep` too.
async fn annotation_totals(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::Runs;
    use ows::durable::{Annotation, CallState, Delivery, MAX_ANNOTATIONS};
    let t = factory("annotation-totals").await?;
    report.cases += 1;
    let s = &t.store;
    let call = |state: CallState, annotations: &[(&str, Annotation)]| Record::Call {
        logical: "call".into(),
        function: "chat".into(),
        arguments: json!({}),
        delivery: Delivery::AtLeastOnce,
        state,
        annotations: annotations.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
    };
    let usage = |tokens: u64, cost: f64| {
        call(
            CallState::Done(Ok(json!("ok"))),
            &[("tokens", tokens.into()), ("cost_usd", cost.into()), ("model", "small".into())],
        )
    };
    let f = s.start(&admission("r"), 0).await?.unwrap();
    s.save_record(&f, "a#call", &call(CallState::Pending, &[]), 1).await?;
    s.save_record(&f, "a#call", &usage(100, 0.25), 1).await?;
    // The same answer saved again (a replay after a lost reply) adds nothing.
    s.save_record(&f, "a#call", &usage(100, 0.25), 2).await?;
    s.save_record(&f, "b#call", &usage(30, 0.5), 2).await?;
    // An error answer carries annotations too.
    let failed = call(CallState::Done(Err(WorkflowError::runtime("no"))), &[("tokens", 5.into())]);
    s.save_record(&f, "c#call", &failed, 2).await?;
    let expected: ows::durable::Totals = BTreeMap::from([
        ("cost_usd".into(), serde_json::Number::from_f64(0.75).unwrap()),
        ("tokens".into(), 135.into()),
    ]);
    let fresh = (t.reopen)().await?;
    assert_eq!(fresh.get("r").await?.unwrap().annotations, expected);
    let inspection = fresh.inspect_control("r").await?.unwrap();
    assert_eq!(inspection.annotations, expected);
    assert_eq!(inspection.annotations, ows::durable::annotation_totals(&inspection.calls));
    let listed = fresh.query_runs(&Runs::default(), false, None, 10).await?;
    assert_eq!(listed.items[0].annotations, expected);
    s.finish(&f, &Status::Completed(json!(1)), 3, 3).await?;
    let window = Runs { since_ms: Some(0), before_ms: Some(10), ..Runs::default() };
    assert_eq!(fresh.query_runs(&window, true, None, 10).await?.items[0].annotations, expected);
    // Sweeping the evidence removes the journal, not the run's totals.
    assert!(fresh.sweep(3 + RECORDS_KEPT_MS, RECORDS_KEPT_MS).await?.records > 0);
    let swept = fresh.inspect_control("r").await?.unwrap();
    assert!(swept.calls.is_empty());
    assert_eq!((swept.annotations, swept.snapshot.run.annotations), (expected.clone(), expected));

    // Names beyond the bound are left out; present names keep summing.
    let f = s.start(&admission("many"), 0).await?.unwrap();
    for n in 0..=MAX_ANNOTATIONS {
        let answer = call(CallState::Done(Ok(json!(n))), &[(&format!("k{n:02}"), Annotation::from(1))]);
        s.save_record(&f, &format!("{n}#call"), &answer, 1).await?;
    }
    let answer = call(CallState::Done(Ok(json!(0))), &[("k00", Annotation::from(2))]);
    s.save_record(&f, "again#call", &answer, 1).await?;
    let totals = s.get("many").await?.unwrap().annotations;
    assert_eq!(totals.len(), MAX_ANNOTATIONS);
    assert!(!totals.contains_key(&format!("k{MAX_ANNOTATIONS:02}")));
    assert_eq!(totals["k00"], 3.into());
    // A run without annotated answers keeps none.
    assert!(s.get("plain").await?.is_none_or(|r| r.annotations.is_empty()));
    audit(&*fresh).await?;
    report.operations += 16;
    report.witnesses.insert("per-run annotation totals, counted once per answer and kept after sweep");

    // Per-definition totals over a window: each run's terminal commit adds
    // its totals to its definition's bucket of the hour it ended in.
    use control::{ControlError, MAX_TOTALS_BUCKETS, TOTALS_BUCKET_MS};
    let hour = TOTALS_BUCKET_MS;
    let t = factory("definition-totals").await?;
    report.cases += 1;
    let s = &t.store;
    let tokens = |n: u64| call(CallState::Done(Ok(json!(n))), &[("tokens", n.into()), ("model", "small".into())]);
    let ended = async |id: &str, n: u64, at: i64| -> anyhow::Result<()> {
        let f = s.start(&admission(id), at).await?.unwrap();
        s.save_record(&f, "chat#call", &tokens(n), at).await?;
        s.finish(&f, &Status::Completed(json!(n)), at, 3).await?;
        Ok(())
    };
    ended("a", 10, 5).await?;
    ended("b", 20, hour - 1).await?;
    ended("c", 40, hour).await?;
    // A run without totals writes no bucket and is not counted.
    let plain = s.start(&admission("plain"), 6).await?.unwrap();
    s.finish(&plain, &Status::Completed(json!(0)), 6, 3).await?;
    // A cascade counts every ended run under its own definition.
    let parent = s.start(&admission("p"), 7).await?.unwrap();
    s.save_record(&parent, "chat#call", &tokens(1), 7).await?;
    let mut other = admission("child");
    other.name = "other".into();
    let link = ChildLink { parent: "p".into(), key: "child".into(), reference: "child".into() };
    let child = s.start_child(&parent, &other, &link, 7).await?.unwrap();
    s.save_record(&child, "chat#call", &tokens(2), 7).await?;
    assert!(s.cancel("p", 8).await?);
    // An ambiguous terminal commit counts once: a retry finds the run ended.
    let f = s.start(&admission("ambiguous"), 9).await?.unwrap();
    s.save_record(&f, "chat#call", &tokens(100), 9).await?;
    t.faults.arm(CommitFault::BeforeCommit);
    assert!(s.finish(&f, &Status::Completed(json!(0)), 9, 3).await.is_err());
    t.faults.arm(CommitFault::AfterCommit);
    assert!(s.finish(&f, &Status::Completed(json!(0)), 9, 3).await.is_err());
    t.faults.clear();
    assert!(s.finish(&f, &Status::Completed(json!(0)), 9, 3).await.is_err());
    let fresh = (t.reopen)().await?;
    let window = fresh.totals("workflow", 0, 2 * hour).await?;
    let sums = |n: u64| BTreeMap::from([("tokens".to_string(), serde_json::Number::from(n))]);
    assert_eq!((window.since_ms, window.before_ms, window.runs), (0, 2 * hour, 5));
    assert_eq!(window.annotations, sums(171));
    assert_eq!(
        window.buckets,
        vec![
            control::TotalsBucket { start_ms: 0, runs: 4, annotations: sums(131) },
            control::TotalsBucket { start_ms: hour, runs: 1, annotations: sums(40) },
        ]
    );
    // Windows round out to whole hours.
    let first = fresh.totals("workflow", 1, 2).await?;
    assert_eq!((first.since_ms, first.before_ms, first.runs), (0, hour, 4));
    assert_eq!(fresh.totals("workflow", hour, hour + 1).await?.annotations, sums(40));
    assert!(fresh.totals("workflow", hour, hour).await?.buckets.is_empty());
    assert_eq!(fresh.totals("other", 0, hour).await?.annotations, sums(2));
    let none = fresh.totals("missing", 0, hour).await?;
    assert!(none.buckets.is_empty() && none.runs == 0 && none.annotations.is_empty());
    let limit = i64::try_from(MAX_TOTALS_BUCKETS).unwrap() * hour;
    assert_eq!(fresh.totals("workflow", 0, limit).await?.runs, 5);
    for (since, before) in [(1, 0), (0, limit + 1), (i64::MIN, i64::MAX)] {
        let refused = refusal(fresh.totals("workflow", since, before).await);
        assert!(matches!(refused, ControlError::InvalidInput(_)), "{refused:?}");
    }
    // Buckets are aggregates of their own: sweeping evidence keeps them.
    fresh.sweep(i64::MAX / 2, 0).await?;
    assert_eq!(fresh.totals("workflow", 0, 2 * hour).await?, window);
    audit(&*fresh).await?;
    report.operations += 24;
    report.witnesses.insert("per-definition hourly totals written by terminal commits, read in bounded windows");
    Ok(())
}

/// Whether an operation failed with the read-only refusal.
fn refused_read_only<T: std::fmt::Debug>(result: anyhow::Result<T>) -> bool {
    result.is_err_and(|e| e.is::<ReadOnly>())
}
/// A read-only handle (#22) over a populated store: every query answers
/// what the writable handle answers, every mutating operation (and every
/// worker operation, fenced reads included) is refused with `ReadOnly`
/// before it touches a row, and a write through the backend's own
/// transaction is refused too. Nothing it does changes the store, and it
/// sees what the writer commits afterwards.
async fn read_only(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::{Command, Runs, Waits, Worker};
    use ows::durable::{Annotation, CallState, Delivery};
    let t = factory("read-only").await?;
    report.cases += 1;
    let s = &t.store;
    let d = s.save_definition("workflow", control_definition_yaml()).await?;
    let slot = Trigger { at_ms: Some(4), types: vec!["start".into()], state: json!({ "kind": "every" }) };
    let arming = Arming { triggers: BTreeMap::from([("w#every".into(), slot.clone())]) };
    s.activate_with("workflow", d.version, 0, &arming, &Activation::at(0)).await?;
    let parent = s.start(&admission("p"), 0).await?.unwrap();
    let call = Record::Call {
        logical: "call".into(),
        function: "f".into(),
        arguments: json!({}),
        delivery: Delivery::AtLeastOnce,
        state: CallState::Done(Ok(json!(1))),
        annotations: BTreeMap::from([("tokens".to_string(), Annotation::from(3))]),
    };
    s.save_record(&parent, "call#call", &call, 0).await?;
    let link = ChildLink { parent: "p".into(), key: "c".into(), reference: "c".into() };
    s.start_child(&parent, &admission("c"), &link, 0).await?;
    s.register_wait(&parent, &["answer".into()], 1).await?;
    s.describe_wait(&parent, "listen", &control_wait(), 1).await?;
    s.deliver(&event("mail", "other", json!(1)), Some("p"), 1).await?;
    s.finish(&parent, &Status::Waiting { wake_at_ms: 100 }, 1, 3).await?;
    let done = s.start(&admission("done"), 2).await?.unwrap();
    s.save_record(&done, "call#call", &call, 2).await?;
    s.finish(&done, &Status::Completed(json!(42)), 3, 3).await?;
    s.control(&control_request("cancel-child"), &Command::Cancel { run: "c".into() }, 4).await?;
    let worker = Worker {
        id: "w".into(),
        host: None,
        application: None,
        started_ms: 0,
        beat_ms: 4,
        expires_ms: 4 + LEASE_MS,
        held: 1,
    };
    s.beat(&worker).await?;

    let backend = (t.read_only)().await?;
    assert!(backend.is_read_only());
    let r = TransactionalStore::new(backend.clone());
    let before = t.backend.snapshot().await?;
    macro_rules! same {
        ($($query:ident($($arg:expr),*)),* $(,)?) => {$(
            assert_eq!(r.$query($($arg),*).await?, s.$query($($arg),*).await?, stringify!($query));
        )*};
    }
    let window = Runs { since_ms: Some(0), before_ms: Some(10), ..Runs::default() };
    let ingress = event("ingress", "start", json!(1));
    same!(
        get("p"),
        get("missing"),
        list(None, ""),
        list(Some("workflow"), "c"),
        inspect("p"),
        inspect_control("p"),
        inspect_control("missing"),
        query_runs(&Runs::default(), false, None, 10),
        query_runs(&window, true, None, 10),
        query_definitions(None, 10),
        definition_control("workflow"),
        pending_waits(&Waits::default(), None, 10),
        workers(None, 10),
        control_entry("cancel-child"),
        control_history(None, None, 10),
        control_history(Some("c"), None, 10),
        stopped(),
        definitions("workflow"),
        trigger("w#every"),
        due_triggers(i64::MAX, 10),
        triggers_for("start", 10),
        queued("w#every"),
        child_outcome("p", "c", "c"),
        ingested(&ingress, 4),
        notices(10),
        totals("workflow", 0, 10),
        totals("missing", 0, 10),
    );
    assert_eq!(r.inspect_control("p").await?.unwrap().calls.len(), 1);
    assert_eq!(r.workers(None, 10).await?.items, vec![worker.clone()]);
    assert_eq!(r.totals("workflow", 0, 10).await?.runs, 1);

    let fence = parent.clone();
    let types = vec!["answer".to_string()];
    let firing = Firing { key: "w#every", expected: Some(&slot), next: None, ..Firing::default() };
    let cancel = Command::Cancel { run: "p".into() };
    assert!(refused_read_only(r.control(&control_request("refused"), &cancel, 5).await));
    let typed = control::ControlError::from(r.control(&control_request("refused"), &cancel, 5).await.unwrap_err());
    assert!(matches!(typed, control::ControlError::ReadOnly), "{typed:?}");
    assert!(refused_read_only(r.beat(&worker).await));
    assert!(refused_read_only(r.describe_wait(&fence, "listen", &control_wait(), 5).await));
    assert!(refused_read_only(r.admit(&admission("new"), 5).await));
    assert!(refused_read_only(r.start(&admission("new"), 5).await));
    assert!(refused_read_only(r.claim("p", 100).await));
    assert!(refused_read_only(r.check_lease(&fence, 5).await));
    assert!(refused_read_only(r.renew(&fence, 5).await));
    assert!(refused_read_only(r.release(&fence, 5).await));
    assert!(refused_read_only(r.load_records(&fence, 5).await));
    assert!(refused_read_only(r.save_record(&fence, "t", &Record::Timer { deadline_ms: 1 }, 5).await));
    assert!(refused_read_only(r.register_wait(&fence, &types, 5).await));
    assert!(refused_read_only(r.unregister_wait(&fence, "listen", &types, 5).await));
    assert!(refused_read_only(r.deliver(&event("new", "answer", json!(1)), Some("p"), 5).await));
    assert!(refused_read_only(r.deliver(&event("new", "answer", json!(1)), None, 5).await));
    assert!(refused_read_only(r.take_event(&fence, "listen#event[0]", &types, 5).await));
    assert!(refused_read_only(r.subscribe(&fence, "listen", &types, 5).await));
    assert!(refused_read_only(r.unsubscribe(&fence, "listen", 5).await));
    assert!(refused_read_only(r.publish(&event("new", "answer", json!(1)), None, 5).await));
    assert!(refused_read_only(r.fire(&firing, 5).await));
    assert!(refused_read_only(r.finish(&fence, &Status::Cancelled, 5, 3).await));
    let link = ChildLink { parent: "p".into(), key: "new".into(), reference: "new".into() };
    assert!(refused_read_only(r.start_child(&fence, &admission("new"), &link, 5).await));
    assert!(refused_read_only(r.spend(&fence, "jump", 10, 5).await));
    assert!(refused_read_only(r.due(i64::MAX, 10).await));
    assert!(refused_read_only(r.cancel("p", 5).await));
    assert!(refused_read_only(r.cancel("missing", 5).await));
    assert!(refused_read_only(r.sweep(i64::MAX, 0).await));
    assert!(refused_read_only(r.save_definition("workflow", "yaml").await));
    assert!(refused_read_only(r.activate("workflow", d.version, 0).await));
    assert!(refused_read_only(r.activate_with("workflow", d.version, 1, &arming, &Activation::at(0)).await));
    assert!(refused_read_only(r.acknowledge("notice").await));
    // The backend refuses a write in its own transaction before sending it.
    let mut tx = backend.begin().await?;
    assert!(tx.get("run", "p").await?.is_some());
    assert!(refused_read_only(tx.put("run", "p", &json!({})).await));
    assert!(refused_read_only(tx.delete("run", "p").await));
    tx.rollback().await;
    assert_eq!(t.backend.snapshot().await?, before, "a read-only handle changes nothing");
    // Each query is a fresh transaction: it sees what the writer commits.
    assert!(s.admit(&admission("later"), 6).await?);
    assert_eq!(r.get("later").await?, s.get("later").await?);
    assert!(r.get("later").await?.is_some());
    report.operations += 60;
    report.witnesses.insert("a read-only handle answers every query and refuses every write");
    Ok(())
}

/// The persisted budget counter of `root`'s tree.
async fn counter(t: &TestStore, root: &str) -> anyhow::Result<Option<Value>> {
    Ok(t.backend.snapshot().await?.rows.get(&("budget".to_string(), root.to_string())).cloned())
}

/// Operations whose future is dropped (#7): mid-transaction, after some of
/// its writes and before its commit, or while a statement is in flight.
/// Nothing it wrote is visible, and what it held (SQLite's one connection,
/// a pooled session) is free for the next operation.
async fn dropped(factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    let t = factory("dropped").await?;
    report.cases += 1;
    let f = t.store.start(&admission("r"), 0).await?.unwrap();
    let before = t.backend.snapshot().await?;
    let stalling = Arc::new(Stalling { inner: t.backend.clone(), after: 1, reached: tokio::sync::Notify::new() });
    let s = TransactionalStore::new(stalling.clone());
    let deadline = std::time::Duration::from_secs(30);
    let fresh = admission("dropped");
    for operation in 0..3 {
        let pending: BoxFuture<'_, anyhow::Result<Option<Fence>>> = match operation {
            0 => s.start(&fresh, 0),
            1 => Box::pin(async { s.save_record(&f, "k", &Record::Timer { deadline_ms: 1 }, 1).await.map(|()| None) }),
            _ => Box::pin(async { s.finish(&f, &Status::Completed(json!(0)), 1, 3).await.map(|_| None) }),
        };
        tokio::select! {
            _ = pending => panic!("a stalled operation completed"),
            _ = stalling.reached.notified() => {}
        }
        assert_eq!(t.backend.snapshot().await?, before, "a dropped operation left writes behind");
    }
    // Dropped after its first poll: a statement (a session's wait, BEGIN)
    // may still be in flight. An operation that completed in that poll
    // (memory) committed whole instead.
    let mut first = t.store.save_record(&f, "first", &Record::Timer { deadline_ms: 1 }, 1);
    let completed = futures::poll!(first.as_mut()).is_ready();
    drop(first);
    let records = tokio::time::timeout(deadline, t.store.load_records(&f, 1)).await??;
    assert_eq!(records.contains_key("first"), completed);
    let next = tokio::time::timeout(deadline, t.store.save_record(&f, "after", &Record::Timer { deadline_ms: 1 }, 1));
    next.await.map_err(|_| anyhow::anyhow!("a dropped operation kept its connection"))??;
    assert!(t.store.get("dropped").await?.is_none());
    t.store.finish(&f, &Status::Completed(json!(0)), 1, 3).await?;
    audit(&*t.store).await?;
    report.operations += 6;
    report.witnesses.insert("a dropped operation leaves nothing and holds nothing");
    Ok(())
}
/// Suspends every transaction forever before its write number `after`,
/// signalling `reached` instead of writing.
struct Stalling {
    inner: Arc<dyn AtomicBackend>,
    after: usize,
    reached: tokio::sync::Notify,
}
struct Stalled<'a> {
    inner: Box<dyn Transaction + 'a>,
    backend: &'a Stalling,
    writes: usize,
}
impl Stalled<'_> {
    async fn write(&mut self) {
        if self.writes == self.backend.after {
            self.backend.reached.notify_one();
            std::future::pending::<()>().await;
        }
        self.writes += 1;
    }
}
#[async_trait::async_trait]
impl AtomicBackend for Stalling {
    async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        Ok(Box::new(Stalled { inner: self.inner.begin().await?, backend: self, writes: 0 }))
    }
    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        self.inner.snapshot().await
    }
}
#[async_trait::async_trait]
impl Transaction for Stalled<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.inner.get(kind, id).await
    }
    async fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        self.inner.scan(kind, start, end, limit).await
    }
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        self.write().await;
        self.inner.put(kind, id, value).await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.write().await;
        self.inner.delete(kind, id).await
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.inner.commit().await
    }
    async fn rollback(self: Box<Self>) {
        self.inner.rollback().await
    }
}

/// A run tree's shared budget (issue #8): one counter per root, spent by
/// the root and its owned descendants in the commit that saves each
/// decision, never reset by a restart, refused past the limit, and gone
/// with the root.
async fn budgets(factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    // A refusal names the run's first refused key, decided once.
    let budget =
        |granted: bool, spent: u64| Record::Budget { granted, spent, first: (!granted).then(|| "c#jump".to_string()) };
    let t = factory("budget").await?;
    report.cases += 1;
    let s = &t.store;
    let f = s.start(&admission("root"), 0).await?.unwrap();
    assert_eq!(s.spend(&f, "a#jump", 2, 1).await?, budget(true, 1));
    assert_eq!(s.spend(&f, "a#jump", 2, 1).await?, budget(true, 1), "repeating returns the saved decision");
    assert_eq!(s.spend(&f, "b#jump", 2, 1).await?, budget(true, 2));
    assert_eq!(s.spend(&f, "c#jump", 2, 1).await?, budget(false, 2));
    assert_eq!(s.spend(&f, "c2#jump", 2, 1).await?, budget(false, 2), "a later refusal names the first");
    assert_eq!(
        s.load_records(&f, 1).await?.get(ows::durable::EXCEEDED),
        Some(&Record::Exceeded { by: "c#jump".into() })
    );
    assert!(s.save_record(&f, ows::durable::EXCEEDED, &Record::Exceeded { by: "a#jump".into() }, 1).await.is_err());
    assert_eq!(counter(&t, "root").await?, Some(json!(2)));
    // Only `spend` commits a decision: a forged one is refused.
    assert!(s.save_record(&f, "d#jump", &budget(true, 0), 1).await.is_err());
    assert!(s.save_record(&f, "a#jump", &budget(true, 1), 1).await.is_ok(), "saving the same decision is a no-op");
    // A failed commit changes neither the record nor the counter.
    let before = t.backend.snapshot().await?;
    t.faults.arm(CommitFault::BeforeCommit);
    assert!(s.spend(&f, "e#jump", 5, 1).await.is_err());
    t.faults.clear();
    assert_eq!(t.backend.snapshot().await?, before);
    // A restart keeps the count: another worker resumes the run after the
    // lease ended; a stale owner cannot spend.
    let reopened = (t.reopen)().await?;
    let now = LEASE_MS + 2;
    let g = reopened.claim("root", now).await?.unwrap();
    assert!(s.spend(&f, "e#jump", 5, now).await.is_err(), "a replaced owner cannot spend");
    assert_eq!(reopened.load_records(&g, now).await?.get("b#jump"), Some(&budget(true, 2)));
    assert_eq!(reopened.spend(&g, "c#jump", 5, now).await?, budget(false, 2), "a refusal stays refused");
    assert_eq!(reopened.spend(&g, "e#jump", 3, now).await?, budget(true, 3), "the count resumes, not restarts");
    report.operations += 11;
    report.witnesses.insert("budget decisions survive restarts and failed commits");

    // Parent, child and grandchild spend the root's one counter.
    let child = |id: &str, parent: &str| {
        (admission(id), ChildLink { parent: parent.into(), key: id.into(), reference: format!("/{id}") })
    };
    let (a, link) = child("child", "root");
    let c = reopened.start_child(&g, &a, &link, now).await?.unwrap();
    assert_eq!(reopened.get("child").await?.unwrap().root.as_deref(), Some("root"));
    let (a, link) = child("grandchild", "child");
    let gc = reopened.start_child(&c, &a, &link, now).await?.unwrap();
    assert_eq!(reopened.get("grandchild").await?.unwrap().root.as_deref(), Some("root"));
    assert_eq!(reopened.spend(&c, "x#jump", 5, now).await?, budget(true, 4));
    assert_eq!(reopened.spend(&gc, "x#jump", 5, now).await?, budget(true, 5));
    assert_eq!(reopened.spend(&g, "f#jump", 5, now).await?, budget(false, 5), "the children spent the root's budget");
    assert_eq!(reopened.spend(&c, "y#jump", 9, now).await?, budget(true, 6), "each spender passes its own limit");
    assert_eq!(counter(&t, "root").await?, Some(json!(6)));
    assert_eq!(counter(&t, "child").await?, None, "a child keeps no counter of its own");
    // A child's unit and the root's count are one commit.
    let before = t.backend.snapshot().await?;
    t.faults.arm(CommitFault::BeforeCommit);
    assert!(reopened.spend(&gc, "z#jump", 9, now).await.is_err());
    t.faults.clear();
    assert_eq!(t.backend.snapshot().await?, before);
    // Concurrent spenders of one tree at its last unit: exactly one wins.
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let workers: Vec<_> = [(g.clone(), "race-root"), (c.clone(), "race-child"), (gc.clone(), "race-grandchild")]
        .into_iter()
        .map(|(fence, key)| {
            let reopen = t.reopen.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let store = reopen().await?;
                barrier.wait().await;
                anyhow::Ok(matches!(store.spend(&fence, key, 7, now).await?, Record::Budget { granted: true, .. }))
            })
        })
        .collect();
    let mut granted = 0;
    for worker in workers {
        granted += usize::from(worker.await.expect("spend worker panicked")?);
    }
    assert_eq!(granted, 1);
    assert_eq!(counter(&t, "root").await?, Some(json!(7)));
    // The counter ends with the root, whose descendants end with it.
    reopened.finish(&g, &Status::Completed(json!(0)), now, 3).await?;
    assert_eq!(counter(&t, "root").await?, None);
    assert!(reopened.spend(&c, "late#jump", 99, now).await.is_err(), "a cancelled child cannot spend");
    audit(&*reopened).await?;
    report.operations += 12;
    report.witnesses.insert("a run tree spends one budget, atomically");
    Ok(())
}

/// The settled decisions of `docs/durable-invariants.md` §8 and issue #3:
/// event identity, the mailbox, ingress tombstones, activation epochs and the
/// breaker, exclusive firing, the clock skew bound and compatibility.
async fn decisions(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let conflict = |e: anyhow::Error| e.is::<IdentityConflict>();

    // Event identity: required, deduplicated per recipient, conflicts refused.
    let t = factory("identity").await?;
    report.cases += 1;
    let s = &t.store;
    let r = s.start(&admission("r"), 0).await?.unwrap();
    assert!(s.deliver(&json!({"type":"answer","data":1}), Some("r"), 0).await.is_err(), "an identity is required");
    let first = event("1", "answer", json!(1));
    assert_eq!(s.deliver(&first, Some("r"), 0).await?, Some("r".into()));
    assert_eq!(s.deliver(&first, Some("r"), 1).await?, Some("r".into()), "accepting it again is idempotent");
    let mut timed = first.clone();
    timed["time"] = json!("1970-01-01T00:00:00.002Z");
    assert_eq!(s.deliver(&timed, Some("r"), 2).await?, Some("r".into()), "a filled-in time is not the payload");
    assert!(conflict(s.deliver(&event("1", "answer", json!(2)), Some("r"), 2).await.unwrap_err()));
    assert!(conflict(s.publish(&event("1", "answer", json!(2)), Some("r"), 2).await.unwrap_err()));
    assert_eq!(s.take_event(&r, "k", &["answer".into()], 3).await?.unwrap()["data"], 1);
    assert_eq!(s.deliver(&first, Some("r"), 4).await?, Some("r".into()), "still a duplicate once consumed");
    assert_eq!(s.inspect("r").await?.unwrap().events.len(), 1);
    // A routed redelivery returns to the run that accepted it first.
    let q = s.start(&admission("q"), 0).await?.unwrap();
    s.register_wait(&q, &["routed".into()], 5).await?;
    let routed = event("2", "routed", json!(2));
    assert_eq!(s.deliver(&routed, None, 6).await?, Some("q".into()));
    s.register_wait(&r, &["routed".into()], 7).await?;
    s.take_event(&q, "taken", &["routed".into()], 8).await?.unwrap();
    assert_eq!(s.deliver(&routed, None, 9).await?, Some("q".into()), "not the next waiting run");
    assert!(conflict(s.deliver(&event("2", "routed", json!(3)), None, 9).await.unwrap_err()));
    assert!(s.inspect("r").await?.unwrap().events.iter().all(|e| e.type_ != "routed"));
    s.finish(&r, &Status::Completed(json!(0)), 10, 3).await?;
    assert_eq!(
        s.deliver(&event("3", "answer", json!(3)), Some("r"), 10).await?,
        None,
        "a terminal run accepts nothing"
    );
    assert_eq!(s.deliver(&first, Some("r"), 10).await?, Some("r".into()), "but keeps its identities");
    audit(&**s).await?;
    report.operations += 18;
    report.witnesses.insert("event identity deduplicates per recipient and refuses conflicts");

    // The mailbox: kept before any listen, read first, FIFO per type, and an
    // observable expiry at its configured lifetime.
    let t = factory("mailbox").await?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { mailbox_ms: 100, ..Policy::default() });
    let r = s.start(&admission("r"), 0).await?.unwrap();
    for (id, t, n) in [("a1", "a", 1), ("b1", "b", 2), ("a2", "a", 3)] {
        assert_eq!(
            s.deliver(&event(id, t, json!(n)), Some("r"), 0).await?,
            Some("r".into()),
            "kept without a listener"
        );
    }
    assert_eq!(s.publish(&event("a3", "a", json!(4)), Some("r"), 0).await?, vec!["r".to_string()]);
    for (k, t, n) in [("k1", "a", 1), ("k2", "a", 3), ("k3", "b", 2)] {
        assert_eq!(s.take_event(&r, k, &[t.into()], 1).await?.unwrap()["data"], n);
    }
    assert!(s.take_event(&r, "wild", &["*".into()], 1).await?.is_some(), "an addressed publish is still published");
    s.deliver(&event("late", "a", json!(5)), Some("r"), 1).await?;
    s.finish(&r, &Status::Waiting { wake_at_ms: 101 }, 1, 3).await?;
    let r = s.claim("r", 101).await?.unwrap();
    assert!(s.take_event(&r, "k4", &["a".into()], 101).await?.is_none(), "an expired copy is never taken");
    let snapshot = s.inspect("r").await?.unwrap();
    let late = snapshot.events.iter().find(|e| e.data == json!(5)).unwrap();
    let key = late.consumed_key.clone().unwrap();
    assert!(matches!(&snapshot.records[&key], Record::Expired { event } if event["id"] == "late"));
    // An unfinished run that never listens: `sweep` expires it, observably.
    s.admit(&admission("idle"), 0).await?;
    s.deliver(&event("idle", "a", json!(6)), Some("idle"), 0).await?;
    assert_eq!(s.sweep(99, RECORDS_KEPT_MS).await?.expired, 0);
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS).await?.expired, 1);
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS).await?.expired, 0);
    assert_eq!(s.inspect("idle").await?.unwrap().events[0].consumed_key.as_deref(), Some("mailbox#expired[0]"));
    assert_eq!(s.deliver(&event("idle", "a", json!(6)), Some("idle"), 200).await?, Some("idle".into()));
    assert_eq!(s.inspect("idle").await?.unwrap().events.len(), 1, "identity outlives expiry");
    // Broadcast publish is unchanged: subscribers only, no mailbox time.
    s.admit(&admission("unsubscribed"), 0).await?;
    assert!(s.publish(&event("b", "a", json!(7)), None, 1).await?.is_empty());
    audit(&s).await?;
    report.operations += 22;
    report.witnesses.insert("mailbox keeps, orders and observably expires addressed events");

    // Ingress: one event is taken by at most one trigger until its tombstone ends.
    let t = factory("ingress").await?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { ingress_ms: 100, ..Policy::default() });
    let on = Trigger { at_ms: None, types: vec!["order".into()], state: json!({ "events": [] }) };
    for k in ["a", "b"] {
        assert!(s.swap_trigger(k, None, Some(&on), &[], None, 0).await?);
    }
    let order = event("1", "order", json!(1));
    let one = [admission("by-a")];
    let two = [admission("by-b")];
    let firing = |k, admit| Firing {
        key: k,
        expected: Some(&on),
        next: Some(&on),
        admit,
        ingress: Some(&order),
        ..Firing::default()
    };
    assert!(!s.ingested(&order, 0).await?);
    assert_eq!(s.fire(&firing("a", &one), 0).await?, Fired::Committed);
    assert!(s.ingested(&order, 1).await?);
    assert_eq!(s.fire(&firing("b", &two), 1).await?, Fired::Ingested);
    assert!(s.get("by-b").await?.is_none(), "one event starts at most one run");
    assert!(conflict(s.ingested(&event("1", "order", json!(2)), 1).await.unwrap_err()));
    assert!(!s.ingested(&order, 100).await?, "the tombstone has its own lifetime");
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS).await?.tombstones, 1);
    assert_eq!(s.fire(&firing("b", &two), 100).await?, Fired::Committed);
    report.operations += 10;
    report.witnesses.insert("ingress identities start one run per tombstone lifetime");

    // Epochs: only an activation starts one, arming atomically. The breaker
    // trips in the terminal commit and reports; it never picks a version.
    let t = factory("epochs").await?;
    report.cases += 1;
    let s = &t.store;
    let one = s.save_definition("w", "one").await?;
    let two = s.save_definition("w", "two").await?;
    let trigger = |n: i64| Trigger { at_ms: Some(n), types: vec![], state: json!({ "n": n }) };
    let arming = |triggers: &[(&str, i64)]| Arming {
        triggers: triggers.iter().map(|(k, n)| ((*k).into(), trigger(*n))).collect(),
    };
    let d = s.activate_with("w", one.version, 0, &arming(&[("w#every", 10)]), &Activation::at(0)).await?.unwrap();
    assert_eq!((d.epoch, d.triggers.clone()), (1, vec!["w#every".to_string()]));
    assert!(
        s.activate_with("w", two.version, 0, &arming(&[]), &Activation::at(0)).await?.is_none(),
        "a stale epoch is refused"
    );
    let d = s.activate_with("w", two.version, 1, &arming(&[("w#after", 20)]), &Activation::at(0)).await?.unwrap();
    assert_eq!(d.epoch, 2);
    assert!(s.trigger("w#every").await?.is_none() && s.trigger("w#after").await?.is_some(), "replaced in one commit");
    let run_of = |id: &str, version: u64| {
        let mut a = admission(id);
        a.name = "w".into();
        a.version = version;
        a
    };
    let old = s.start(&run_of("before", two.version), 0).await?.unwrap();
    assert_eq!(s.get("before").await?.unwrap().epoch, Some(2));
    let late = s.start(&run_of("late", two.version), 0).await?.unwrap();
    let late_success = s.start(&run_of("late-success", two.version), 0).await?.unwrap();
    // Cancellation and running out of interruptions do not count.
    let cancelled = s.start(&run_of("cancelled", two.version), 0).await?.unwrap();
    s.finish(&cancelled, &Status::Cancelled, 0, 2).await?;
    let mut exhausted = s.start(&run_of("exhausted", two.version), 0).await?.unwrap();
    for n in 1..=MAX_INTERRUPTIONS {
        let now = s.get("exhausted").await?.unwrap().wake_at_ms;
        let status = s.finish(&exhausted, &Status::Interrupted(WorkflowError::runtime("storage")), now, 2).await?;
        if n < MAX_INTERRUPTIONS {
            exhausted = s.claim("exhausted", s.get("exhausted").await?.unwrap().wake_at_ms).await?.unwrap();
        } else {
            assert!(matches!(status, Status::Faulted(_)));
        }
    }
    assert_eq!(s.definitions("w").await?.consecutive_faults, 0);
    assert!(s.notices(10).await?.is_empty());
    for n in 0..2 {
        let f = s.start(&run_of(&format!("fault{n}"), two.version), 0).await?.unwrap();
        s.finish(&f, &Status::Faulted(WorkflowError::runtime(format!("fault {n}"))), 5, 2).await?;
    }
    let d = s.definitions("w").await?;
    assert_eq!((d.active, d.epoch, d.consecutive_faults), (Some(two.version), 2, 2), "no rollback, no new epoch");
    let trip = Trip {
        manual: None,
        version: two.version,
        digest: digest_of("two"),
        epoch: 2,
        faults: 2,
        threshold: 2,
        run: "fault1".into(),
        reason: WorkflowError::runtime("fault 1"),
        at_ms: 5,
    };
    assert_eq!(d.tripped.as_ref(), Some(&trip), "the reason and the counts, visible in the runs API");
    assert!(d.triggers.is_empty() && s.trigger("w#after").await?.is_none(), "the schedule stops in the trip's commit");
    // The trip's notice is committed with it: an identified CloudEvent.
    let notices = s.notices(10).await?;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1, trip.event("w"));
    assert_eq!((notices[0].1["type"].as_str(), notices[0].1["id"].as_str()), (Some(TRIPPED), Some("tripped:2")));
    // Tripped, no new run of the tripped content is admitted, by any path, with
    // a typed refusal in the admission's commit; an existing run is joined.
    let tripped = |e: anyhow::Error| e.downcast_ref::<DefinitionTripped>().map(|t| (t.name.clone(), t.epoch));
    let expected = Some(("w".to_string(), 2));
    assert_eq!(tripped(s.admit(&run_of("refused", two.version), 6).await.unwrap_err()), expected);
    // Other content (version one) is admitted, uncounted; this is the digest rule.
    assert!(s.start(&run_of("other-content", one.version), 6).await?.is_some());
    assert_eq!(s.get("other-content").await?.unwrap().epoch, None);
    let parent = s.start(&admission("parent"), 6).await?.unwrap();
    let link = ChildLink { parent: "parent".into(), key: "child".into(), reference: "/child".into() };
    let before = s.inspect("parent").await?;
    assert_eq!(tripped(s.start_child(&parent, &run_of("refused", two.version), &link, 6).await.unwrap_err()), expected);
    assert_eq!(s.inspect("parent").await?, before, "the parent's side of a refused child is not committed");
    let fired = [run_of("refused", two.version)];
    let firing = Firing { key: "generic", next: Some(&trigger(50)), admit: &fired, ..Firing::default() };
    assert_eq!(tripped(s.fire(&firing, 6).await.unwrap_err()), expected);
    assert!(s.get("refused").await?.is_none() && s.trigger("generic").await?.is_none());
    assert!(!s.admit(&run_of("late", two.version), 6).await?, "an admitted run is joined, not refused");
    s.finish(&parent, &Status::Completed(json!(0)), 6, 2).await?;
    // Tripped, the breaker observes nothing more until an activation.
    s.finish(&late, &Status::Faulted(WorkflowError::runtime("late")), 6, 2).await?;
    s.finish(&late_success, &Status::Completed(json!(0)), 6, 2).await?;
    let d = s.definitions("w").await?;
    assert_eq!((d.consecutive_faults, d.tripped.as_ref()), (2, Some(&trip)));
    assert_eq!(s.notices(10).await?.len(), 1, "one trip, one notice");
    // Only an activation clears it, in a new epoch, with the version the
    // host chooses (here the earlier one). The notice stays until handed on.
    let d = s.activate_with("w", one.version, 2, &arming(&[("w#every", 40)]), &Activation::at(0)).await?.unwrap();
    assert!(d.tripped.is_none() && d.epoch == 3 && d.consecutive_faults == 0, "a new activation clears it");
    assert_eq!(s.trigger("w#every").await?, Some(trigger(40)));
    assert_eq!(s.notices(10).await?, notices);
    assert!(s.acknowledge(&notices[0].0).await?);
    assert!(!s.acknowledge(&notices[0].0).await?, "already acknowledged");
    assert!(s.notices(10).await?.is_empty());
    // An earlier epoch's fault cannot touch the new one.
    s.finish(&old, &Status::Faulted(WorkflowError::runtime("late")), 0, 2).await?;
    assert_eq!(s.definitions("w").await?.consecutive_faults, 0);
    // A success resets the count.
    let f = s.start(&run_of("fault", one.version), 0).await?.unwrap();
    s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 2).await?;
    let f = s.start(&run_of("success", one.version), 0).await?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 0, 2).await?;
    assert_eq!(s.definitions("w").await?.consecutive_faults, 0);
    audit(&**s).await?;
    report.operations += 60;
    report.witnesses.insert("epochs arm atomically and isolate the breaker");
    report.witnesses.insert("a trip reports with a committed notice and only activation clears it");

    // Exclusive firing: a slot whose previous run is unfinished is skipped.
    let t = factory("overlap").await?;
    report.cases += 1;
    let s = &t.store;
    let slot = |at: i64, skips: i64| Trigger { at_ms: Some(at), types: vec![], state: json!({ "skips": skips }) };
    let fire = async |expected: &Trigger, next: &Trigger, busy: &Trigger, id: &str, now: i64| {
        s.fire(
            &Firing {
                key: "every",
                expected: Some(expected),
                next: Some(next),
                admit: &[admission(id)],
                busy: Some(busy),
                ..Firing::default()
            },
            now,
        )
        .await
    };
    assert!(s.swap_trigger("every", None, Some(&slot(10, 0)), &[], None, 0).await?);
    assert_eq!(fire(&slot(10, 0), &slot(20, 0), &slot(20, 1), "slot10", 10).await?, Fired::Committed);
    assert_eq!(fire(&slot(20, 0), &slot(30, 0), &slot(30, 1), "slot20", 20).await?, Fired::Busy("slot10".into()));
    assert!(s.get("slot20").await?.is_none());
    assert_eq!(s.trigger("every").await?, Some(slot(30, 1)), "the skip is recorded");
    let f = s.claim("slot10", 21).await?.unwrap();
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 22, 3).await?;
    assert!(matches!(fire(&slot(30, 1), &slot(40, 1), &slot(40, 2), "slot30", 30).await?, Fired::Busy(_)), "waiting");
    s.cancel("slot10", 31).await?;
    assert_eq!(fire(&slot(40, 2), &slot(50, 2), &slot(50, 3), "slot40", 40).await?, Fired::Committed);
    report.operations += 8;
    report.witnesses.insert("a live previous run makes the next slot an explicit skip");

    // A trigger's run the host refused to admit: the slot is consumed with a
    // run that ended faulted in that commit, counted by the breaker, whose
    // threshold refusal trips it and removes the trigger in the same commit.
    let t = factory("refused").await?;
    report.cases += 1;
    let s = &t.store;
    let v = s.save_definition("w", "yaml").await?;
    let slot = |at: i64| Trigger { at_ms: Some(at), types: vec![], state: json!({ "at": at }) };
    let arming = Arming { triggers: BTreeMap::from([("w#every".into(), slot(10))]) };
    s.activate_with("w", v.version, 0, &arming, &Activation::at(0)).await?.unwrap();
    let run_of = |id: &str| Admission { name: "w".into(), version: v.version, ..admission(id) };
    let invalid = WorkflowError::validation("invalid");
    let refused = |id: &str| Refused { admission: run_of(id), error: invalid.clone(), breaker_threshold: 2 };
    let fire = async |at: i64, admit: &[Admission], refused: Option<&Refused>| {
        let firing = Firing {
            key: "w#every",
            expected: Some(&slot(at)),
            next: Some(&slot(at + 10)),
            admit,
            refused,
            busy: Some(&slot(at + 10)),
            ..Firing::default()
        };
        s.fire(&firing, at).await
    };
    // Overlap applies to a refused slot as to an admitted one.
    assert_eq!(fire(10, &[run_of("live")], None).await?, Fired::Committed);
    assert_eq!(fire(20, &[], Some(&refused("slot20"))).await?, Fired::Busy("live".into()));
    assert!(s.get("slot20").await?.is_none());
    s.cancel("live", 25).await?;
    assert_eq!(fire(30, &[], Some(&refused("slot30"))).await?, Fired::Committed);
    let r = s.get("slot30").await?.unwrap();
    assert_eq!(
        (r.state, r.outcome.clone(), r.ended_ms, r.epoch),
        (RunState::Failed, Some(Status::Faulted(invalid.clone())), Some(30), Some(1))
    );
    assert_eq!((r.admitted_ms, r.admission), (30, run_of("slot30")), "visible in the runs API as it was refused");
    assert!(
        s.due(i64::MAX, 10).await?.is_empty() && s.claim("slot30", 30).await?.is_none(),
        "never due, never claimed"
    );
    assert_eq!(s.trigger("w#every").await?, Some(slot(40)), "the slot is consumed");
    assert_eq!(s.definitions("w").await?.consecutive_faults, 1);
    assert_eq!(fire(30, &[], Some(&refused("slot30"))).await?, Fired::Changed, "a repeated firing changes nothing");
    assert_eq!(s.definitions("w").await?.consecutive_faults, 1);
    assert!(
        s.fire(
            &Firing { key: "w#every", admit: &[run_of("a")], refused: Some(&refused("b")), ..Firing::default() },
            40
        )
        .await
        .is_err(),
        "admit or refuse, not both"
    );
    assert!(s.get("a").await?.is_none() && s.get("b").await?.is_none());
    assert_eq!(fire(40, &[], Some(&refused("slot40"))).await?, Fired::Committed);
    let d = s.definitions("w").await?;
    let trip = d.tripped.expect("the threshold refusal trips");
    assert_eq!((trip.run.as_str(), &trip.reason, trip.faults, trip.threshold), ("slot40", &invalid, 2, 2));
    assert!(d.triggers.is_empty() && s.trigger("w#every").await?.is_none(), "its schedule stops in that commit");
    assert_eq!(s.notices(10).await?, vec![(s.notices(10).await?[0].0.clone(), trip.event("w"))]);
    assert_eq!(fire(40, &[], Some(&refused("slot50"))).await?, Fired::Changed);
    audit(&**s).await?;
    report.operations += 16;
    report.witnesses.insert("a refused trigger run consumes its slot, counts and may trip");

    // The skew bound: an owner stops acting ε before its lease ends, and a
    // lease is taken over only after it ends.
    let t = factory("skew").await?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { skew_ms: 100, ..Policy::default() });
    let f = s.start(&admission("r"), 0).await?.unwrap();
    s.save_record(&f, "early", &Record::Timer { deadline_ms: 1 }, LEASE_MS - 101).await?;
    assert!(s.save_record(&f, "late", &Record::Timer { deadline_ms: 1 }, LEASE_MS - 100).await.is_err());
    assert!(s.renew(&f, LEASE_MS - 100).await.is_err());
    assert!(s.claim("r", LEASE_MS - 1).await?.is_none());
    assert!(s.claim("r", LEASE_MS).await?.is_some());
    assert_eq!(s.get("r").await?.unwrap().admitted_ms, 0, "the admission instant survives takeover");
    report.operations += 6;
    report.witnesses.insert("owners stop acting a skew bound before takeover");

    // Compatibility: a declared run goes only to a compatible worker; the
    // rest wait, visibly due.
    let t = factory("compatibility").await?;
    report.cases += 1;
    let s = &t.store;
    for (id, version) in [("undeclared", None), ("one", Some("1")), ("two", Some("2"))] {
        let mut a = admission(id);
        if let Some(v) = version {
            a.runtime = json!({ APPLICATION_VERSION: v });
        }
        s.admit(&a, 0).await?;
    }
    let two = Compatibility { version: Some("2".into()), accepts: vec![] };
    assert_eq!(s.due_as(0, 10, &two).await?, vec!["two".to_string(), "undeclared".into()]);
    assert!(s.claim_as("one", 0, &two).await?.is_none());
    assert!(s.due(0, 10).await?.contains(&"one".to_string()), "waiting for a compatible worker, not lost");
    assert_eq!(s.due_as(0, 1, &two).await?.len(), 1, "one page");
    let upgraded = Compatibility { version: Some("2".into()), accepts: vec!["1".into()] };
    assert_eq!(s.due_as(0, 10, &upgraded).await?.len(), 3);
    assert!(s.claim_as("one", 0, &upgraded).await?.is_some());
    assert!(s.claim_as("two", 0, &Compatibility::default()).await?.is_some(), "an undeclared worker claims anything");
    assert!(s.claim_as("undeclared", 0, &two).await?.is_some(), "an undeclared run goes to any worker");
    report.operations += 9;
    report.witnesses.insert("only compatible workers claim declared runs");

    // The rearm of an old epoch cannot move its replacement.
    let t = factory("rearm").await?;
    report.cases += 1;
    let s = &t.store;
    let dormant = |epoch: i64| Trigger { at_ms: None, types: vec![], state: json!({ "epoch": epoch }) };
    let rearm = Rearm { trigger: "after".into(), delay_ms: 5 };
    assert!(s.swap_trigger("after", None, Some(&dormant(1)), &[admission("first")], Some(&rearm), 0).await?);
    let f = s.claim("first", 0).await?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 1, 3).await?;
    assert_eq!(s.trigger("after").await?.and_then(|t| t.at_ms), Some(6), "the same epoch re-arms");
    let due = s.trigger("after").await?.unwrap();
    assert!(s.swap_trigger("after", Some(&due), Some(&dormant(1)), &[admission("second")], Some(&rearm), 6).await?);
    assert!(s.swap_trigger("after", Some(&dormant(1)), Some(&dormant(2)), &[], None, 7).await?);
    let f = s.claim("second", 7).await?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 8, 3).await?;
    assert_eq!(s.trigger("after").await?, Some(dormant(2)), "a replaced trigger is not re-armed");
    report.operations += 8;
    report.witnesses.insert("an old epoch's completion cannot rearm its replacement");

    // A one-shot trigger: its only firing admits the run, with what the
    // trigger told it, and leaves the trigger dormant, with no rearm. Neither
    // the run's end nor a reopen makes it due again.
    let t = factory("one-shot").await?;
    report.cases += 1;
    let s = &t.store;
    let armed = Trigger { at_ms: Some(10), types: vec![], state: json!({ "kind": "at", "epoch": 1 }) };
    let done = Trigger {
        at_ms: None,
        types: vec![],
        state: json!({ "kind": "at", "epoch": 1, "done": true, "previous": 10 }),
    };
    let mut shot = admission("shot");
    shot.runtime = json!({ "schedule": { "kind": "at", "slot": 10, "previous": null } });
    assert!(s.swap_trigger("once", None, Some(&armed), &[], None, 0).await?);
    assert_eq!(s.due_triggers(9, 10).await?, vec![]);
    assert_eq!(s.due_triggers(10, 10).await?, vec![("once".to_string(), armed.clone())]);
    let firing =
        Firing { key: "once", expected: Some(&armed), next: Some(&done), admit: &[shot.clone()], ..Firing::default() };
    assert_eq!(s.fire(&firing, 10).await?, Fired::Committed);
    assert_eq!(s.fire(&firing, 10).await?, Fired::Changed, "the slot is consumed once");
    assert_eq!(s.get("shot").await?.unwrap().admission, shot, "the run keeps what its trigger told it");
    let f = s.claim("shot", 11).await?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 12, 3).await?;
    let reopened = (t.reopen)().await?;
    assert_eq!(reopened.trigger("once").await?, Some(done));
    assert!(reopened.due_triggers(i64::MAX, 10).await?.is_empty());
    assert_eq!(reopened.get("shot").await?.unwrap().admission, shot);
    audit(&*reopened).await?;
    report.operations += 8;
    report.witnesses.insert("a one-shot trigger fires once and stays dormant");
    Ok(())
}

// Interpreter-level recovery, ported from the checkpoint/hold properties.
// Every commit boundary is visited explicitly, rather than relying on random
// histories to reach it. Only the clock is virtual; database writes are real.
//
// The interpreter runs on a paused runtime of its own, so its holds and
// timeouts take no real time. A paused clock jumps to the next timer
// whenever its runtime idles, which waiting for a database's reply would be;
// so every store call goes back to the caller's runtime (`Offloaded`) from a
// blocking task, which keeps the paused clock still while the call is out.

/// One listen case on a fresh paused runtime, in a blocking task of the
/// caller's runtime, which keeps serving the store meanwhile.
async fn paused(
    t: TestStore,
    mode: u8,
    fail: usize,
    after_save: bool,
    after_take: bool,
) -> anyhow::Result<(usize, bool)> {
    let outer = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build()?;
        let reopen = t.reopen.clone();
        let inner = outer.clone();
        let t = TestStore {
            store: Arc::new(Offloaded { store: t.store.clone(), outer: outer.clone() }),
            reopen: Arc::new(move || {
                let (reopen, outer) = (reopen.clone(), inner.clone());
                Box::pin(async move {
                    let store = offload(&outer, async move { reopen().await }).await?;
                    Ok(Arc::new(Offloaded { store, outer }) as Arc<dyn RunStore>)
                })
            }),
            ..t
        };
        rt.block_on(listen_case(t, mode, fail, after_save, after_take))
    })
    .await?
}
/// Awaits `call` on `outer` from a blocking task of the current runtime.
async fn offload<T: Send + 'static>(
    outer: &tokio::runtime::Handle,
    call: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
) -> anyhow::Result<T> {
    let outer = outer.clone();
    tokio::task::spawn_blocking(move || outer.block_on(call)).await?
}
/// A store whose every operation runs on another runtime (`offload`).
struct Offloaded {
    store: Arc<dyn RunStore>,
    outer: tokio::runtime::Handle,
}
macro_rules! offloaded {
    ($self:ident, |$s:ident| $call:expr) => {{
        let $s = $self.store.clone();
        offload(&$self.outer, async move { $call.await }).await
    }};
}
#[async_trait::async_trait]
impl RunStore for Offloaded {
    fn policy(&self) -> Policy {
        self.store.policy()
    }
    async fn admit(&self, a: &Admission, now: i64) -> anyhow::Result<bool> {
        let a = a.clone();
        offloaded!(self, |s| s.admit(&a, now))
    }
    async fn start(&self, a: &Admission, now: i64) -> anyhow::Result<Option<Fence>> {
        let a = a.clone();
        offloaded!(self, |s| s.start(&a, now))
    }
    async fn claim_as(&self, id: &str, now: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>> {
        let (id, worker) = (id.to_owned(), worker.clone());
        offloaded!(self, |s| s.claim_as(&id, now, &worker))
    }
    async fn renew(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        let f = f.clone();
        offloaded!(self, |s| s.renew(&f, now))
    }
    async fn release(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        let f = f.clone();
        offloaded!(self, |s| s.release(&f, now))
    }
    async fn load_records(&self, f: &Fence, now: i64) -> anyhow::Result<BTreeMap<String, Record>> {
        let f = f.clone();
        offloaded!(self, |s| s.load_records(&f, now))
    }
    async fn save_record(&self, f: &Fence, k: &str, r: &Record, now: i64) -> anyhow::Result<()> {
        let (f, k, r) = (f.clone(), k.to_owned(), r.clone());
        offloaded!(self, |s| s.save_record(&f, &k, &r, now))
    }
    async fn register_wait(&self, f: &Fence, types: &[String], now: i64) -> anyhow::Result<()> {
        let (f, types) = (f.clone(), types.to_vec());
        offloaded!(self, |s| s.register_wait(&f, &types, now))
    }
    async fn unregister_wait(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        let (f, k, types) = (f.clone(), k.to_owned(), types.to_vec());
        offloaded!(self, |s| s.unregister_wait(&f, &k, &types, now))
    }
    async fn deliver(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Option<String>> {
        let (event, target) = (event.clone(), target.map(str::to_owned));
        offloaded!(self, |s| s.deliver(&event, target.as_deref(), now))
    }
    async fn take_event(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<Option<Value>> {
        let (f, k, types) = (f.clone(), k.to_owned(), types.to_vec());
        offloaded!(self, |s| s.take_event(&f, &k, &types, now))
    }
    async fn subscribe(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        let (f, k, types) = (f.clone(), k.to_owned(), types.to_vec());
        offloaded!(self, |s| s.subscribe(&f, &k, &types, now))
    }
    async fn unsubscribe(&self, f: &Fence, k: &str, now: i64) -> anyhow::Result<()> {
        let (f, k) = (f.clone(), k.to_owned());
        offloaded!(self, |s| s.unsubscribe(&f, &k, now))
    }
    async fn finish(&self, f: &Fence, outcome: &Status, now: i64, threshold: u32) -> anyhow::Result<Status> {
        let (f, outcome) = (f.clone(), outcome.clone());
        offloaded!(self, |s| s.finish(&f, &outcome, now, threshold))
    }
    async fn start_child(&self, p: &Fence, a: &Admission, link: &ChildLink, now: i64) -> anyhow::Result<Option<Fence>> {
        let (p, a, link) = (p.clone(), a.clone(), link.clone());
        offloaded!(self, |s| s.start_child(&p, &a, &link, now))
    }
    async fn child_outcome(&self, parent: &str, k: &str, child: &str) -> anyhow::Result<Option<Status>> {
        let (parent, k, child) = (parent.to_owned(), k.to_owned(), child.to_owned());
        offloaded!(self, |s| s.child_outcome(&parent, &k, &child))
    }
    async fn spend(&self, f: &Fence, k: &str, limit: u64, now: i64) -> anyhow::Result<Record> {
        let (f, k) = (f.clone(), k.to_owned());
        offloaded!(self, |s| s.spend(&f, &k, limit, now))
    }
    async fn due_as(&self, now: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>> {
        let worker = worker.clone();
        offloaded!(self, |s| s.due_as(now, limit, &worker))
    }
    async fn cancel(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        let id = id.to_owned();
        offloaded!(self, |s| s.cancel(&id, now))
    }
    async fn sweep(&self, now: i64, kept: i64) -> anyhow::Result<Swept> {
        offloaded!(self, |s| s.sweep(now, kept))
    }
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>> {
        let id = id.to_owned();
        offloaded!(self, |s| s.get(&id))
    }
    async fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        let (name, prefix) = (name.map(str::to_owned), prefix.to_owned());
        offloaded!(self, |s| s.list(name.as_deref(), &prefix))
    }
    async fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>> {
        let id = id.to_owned();
        offloaded!(self, |s| s.inspect(&id))
    }
    async fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        let (name, yaml) = (name.to_owned(), yaml.to_owned());
        offloaded!(self, |s| s.save_definition(&name, &yaml))
    }
    async fn activate_with(
        &self,
        name: &str,
        version: u64,
        epoch: u64,
        arming: &Arming,
        activation: &Activation,
    ) -> anyhow::Result<Option<Definitions>> {
        let (name, arming, activation) = (name.to_owned(), arming.clone(), activation.clone());
        offloaded!(self, |s| s.activate_with(&name, version, epoch, &arming, &activation))
    }
    async fn definitions(&self, name: &str) -> anyhow::Result<Definitions> {
        let name = name.to_owned();
        offloaded!(self, |s| s.definitions(&name))
    }
    async fn control(
        &self,
        request: &control::Request,
        command: &control::Command,
        now: i64,
    ) -> anyhow::Result<control::Reply> {
        let (request, command) = (request.clone(), command.clone());
        offloaded!(self, |s| s.control(&request, &command, now))
    }
    async fn stopped(&self) -> anyhow::Result<bool> {
        offloaded!(self, |s| s.stopped())
    }
    async fn control_entry(&self, id: &str) -> anyhow::Result<Option<control::Entry>> {
        let id = id.to_owned();
        offloaded!(self, |s| s.control_entry(&id))
    }
    async fn control_history(
        &self,
        run: Option<&str>,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Entry>> {
        let (run, cursor) = (run.map(str::to_owned), cursor.cloned());
        offloaded!(self, |s| s.control_history(run.as_deref(), cursor.as_ref(), limit))
    }
    async fn query_runs(
        &self,
        filter: &control::Runs,
        history: bool,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<SavedRun>> {
        let (filter, cursor) = (filter.clone(), cursor.cloned());
        offloaded!(self, |s| s.query_runs(&filter, history, cursor.as_ref(), limit))
    }
    async fn query_definitions(
        &self,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::DefinitionSummary>> {
        let cursor = cursor.cloned();
        offloaded!(self, |s| s.query_definitions(cursor.as_ref(), limit))
    }
    async fn definition_control(&self, name: &str) -> anyhow::Result<Option<control::DefinitionSummary>> {
        let name = name.to_owned();
        offloaded!(self, |s| s.definition_control(&name))
    }
    async fn pending_waits(
        &self,
        filter: &control::Waits,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Wait>> {
        let (filter, cursor) = (filter.clone(), cursor.cloned());
        offloaded!(self, |s| s.pending_waits(&filter, cursor.as_ref(), limit))
    }
    async fn inspect_control(&self, id: &str) -> anyhow::Result<Option<control::Inspection>> {
        let id = id.to_owned();
        offloaded!(self, |s| s.inspect_control(&id))
    }
    async fn describe_wait(&self, f: &Fence, k: &str, wait: &ows::events::PendingWait, now: i64) -> anyhow::Result<()> {
        let (f, k, wait) = (f.clone(), k.to_owned(), wait.clone());
        offloaded!(self, |s| s.describe_wait(&f, &k, &wait, now))
    }
    async fn check_lease(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        let f = f.clone();
        offloaded!(self, |s| s.check_lease(&f, now))
    }
}
struct LeaseTime(std::sync::atomic::AtomicI64);
impl Clock for LeaseTime {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
struct WorkflowTime(tokio::time::Instant);
impl ows::run::Clock for WorkflowTime {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(1000 + i64::try_from(self.0.elapsed().as_millis()).unwrap()).unwrap()
    }
}
struct Inbox {
    lease: Arc<Lease>,
    saves: std::sync::atomic::AtomicUsize,
    /// Takes that found nothing: the listen goes on to hold for a delivery.
    missed: std::sync::atomic::AtomicUsize,
    fail: usize,
    after_save: bool,
    after_take: bool,
}
#[async_trait::async_trait]
impl Journal for Inbox {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.lease.load().await
    }
    async fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        let n = self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(self.after_save || n != self.fail, "crash before checkpoint {n}");
        self.lease.save(key, record).await?;
        anyhow::ensure!(!self.after_save || n != self.fail, "crash after checkpoint {n}");
        Ok(())
    }
}
#[async_trait::async_trait]
impl Events for Inbox {
    fn notification(&self, id: &str) -> futures::future::BoxFuture<'_, ()> {
        self.lease.notification(id)
    }
    async fn register(&self, id: &str, types: &[String]) -> anyhow::Result<()> {
        self.lease.register(id, types).await
    }
    async fn unregister(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        self.lease.unregister(id, key, types).await
    }
    async fn take(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        let event = self.lease.take(id, key, types).await?;
        if event.is_none() {
            self.missed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        anyhow::ensure!(!self.after_take || event.is_none(), "crash after atomic event consumption");
        Ok(event)
    }
}
#[async_trait::async_trait]
impl ows::run::Functions for Inbox {
    fn has(&self, _: &str) -> bool {
        false
    }
    fn events(&self) -> Option<&dyn Events> {
        Some(self)
    }
    async fn call(&self, _: &str, _: Value, _: &ows::run::CallInfo<'_>) -> Result<Value, WorkflowError> {
        unreachable!("listen-only workflow")
    }
}
async fn listen_case(
    t: TestStore,
    mode: u8,
    fail: usize,
    after_save: bool,
    after_take: bool,
) -> anyhow::Result<(usize, bool)> {
    use ows::run::{self, Host, Limits, Silent};
    use std::{
        sync::atomic::{AtomicI64, AtomicUsize, Ordering},
        task::Poll,
        time::Duration,
    };
    let time = Arc::new(WorkflowTime(tokio::time::Instant::now()));
    let lease_time = Arc::new(LeaseTime(AtomicI64::new(1000)));
    let fence = t.store.start(&admission("r"), 1000).await?.unwrap();
    let lease = Arc::new(Lease { store: t.store.clone(), fence, clock: lease_time.clone() });
    let inbox = Arc::new(Inbox {
        lease: lease.clone(),
        saves: AtomicUsize::new(0),
        missed: AtomicUsize::new(0),
        fail,
        after_save,
        after_take,
    });
    let timeout = if mode == 3 { 5 } else { 50 };
    let definition = format!(
        "document: {{ dsl: '1.0.4', namespace: test, name: listen, version: '1.0.0' }}\ndo:\n - attempt:\n     try:\n       - listen:\n           listen: {{ to: {{ one: {{ with: {{ type: answer }} }} }}, read: envelope }}\n           metadata: {{ hold: PT0.010S }}\n           timeout: {{ after: PT0.{timeout:03}S }}\n     catch:\n       errors: {{ with: {{ status: 408 }} }}\n       do: [ {{ expired: {{ set: {{ expired: true }} }} }} ]\n"
    );
    let wf = ows::model::parse_yaml(&definition)?;
    let host = Host {
        functions: inbox.clone(),
        observer: Arc::new(Silent),
        clock: time.clone(),
        limits: Limits::default(),
        runtime: json!({}),
    };
    let token = tokio_util::sync::CancellationToken::new();
    if mode == 0 {
        lease.register("r", &["answer".into()]).await?;
        t.store.deliver(&event("first", "answer", json!("answer")), None, 1000).await?;
    }
    let mut future = Box::pin(run::resume(&host, &wf, "r", json!({}), &token, &*inbox));
    // Mode 1 delivers while the listen holds: drive the run until its first
    // take found nothing and it waits on its hold's timers. Its store calls
    // run on another runtime, so it gets there over several polls; yielding
    // between them keeps the paused clock still.
    let mut polled = futures::poll!(future.as_mut());
    while mode == 1 && polled.is_pending() && inbox.missed.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
        polled = futures::poll!(future.as_mut());
    }
    if polled.is_pending() {
        polled = futures::poll!(future.as_mut());
    }
    let first = match polled {
        Poll::Ready(outcome) => outcome.status,
        Poll::Pending => {
            if mode == 1 {
                tokio::time::advance(Duration::from_millis(2)).await;
                t.store.deliver(&event("first", "answer", json!("answer")), None, 1000).await?;
            } else {
                tokio::time::advance(Duration::from_millis(10)).await;
            }
            future.await.status
        }
    };
    let saves = inbox.saves.load(Ordering::SeqCst);
    let interrupted = matches!(first, Status::Interrupted(_));
    audit(&*t.store).await?;
    if !interrupted {
        t.store.finish(&lease.fence, &first, 1000, 3).await?;
    }
    let reopened = (t.reopen)().await?;
    if reopened.get("r").await?.unwrap().outcome.is_none() {
        let now = LEASE_MS + 1001;
        lease_time.0.store(now, Ordering::SeqCst);
        let fence = reopened.claim("r", now).await?.unwrap();
        assert!(t.store.renew(&lease.fence, now).await.is_err());
        let current = Arc::new(Lease { store: reopened.clone(), fence, clock: lease_time });
        let next = Arc::new(Inbox {
            lease: current.clone(),
            saves: AtomicUsize::new(0),
            missed: AtomicUsize::new(0),
            fail: usize::MAX,
            after_save: false,
            after_take: false,
        });
        let host = Host {
            functions: next.clone(),
            observer: Arc::new(Silent),
            clock: time,
            limits: Limits::default(),
            runtime: json!({}),
        };
        if mode != 3 {
            current.register("r", &["answer".into()]).await?;
            reopened.deliver(&event("recovery", "answer", json!("answer")), None, now).await?;
        } else {
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        let mut status = run::resume(&host, &wf, "r", json!({}), &token, &*next).await.status;
        if mode == 3 && matches!(status, Status::Waiting { .. }) {
            tokio::time::advance(Duration::from_millis(100)).await;
            status = run::resume(&host, &wf, "r", json!({}), &token, &*next).await.status;
        }
        assert!(matches!(status, Status::Completed(_)), "recovery stalled: {mode}/{fail}/{after_save}: {status:?}");
        reopened.finish(&current.fence, &status, now, 3).await?;
    }
    let outcome = reopened.get("r").await?.unwrap().outcome.unwrap();
    if mode == 3 {
        assert_eq!(outcome, Status::Completed(json!({"expired":true})));
    } else {
        let Status::Completed(v) = outcome else { panic!("{outcome:?}") };
        assert_eq!(v[0]["data"], "answer");
    }
    audit(&*reopened).await?;
    assert_eq!(
        reopened.inspect("r").await?.unwrap().events.iter().filter(|e| e.consumed_key.is_some()).count(),
        usize::from(mode != 3)
    );
    Ok((saves, interrupted))
}

// How far a call got, as its function answers it (issue #25), persisted by
// the adapter: a refusal (`NotExecuted`) is saved and a new attempt may send
// the call again, at most once included; an unknown at-most-once outcome
// keeps its intent without an answer, and a crash between the dispatch and
// the commit of its answer leaves the same evidence. Every commit boundary is
// crashed before and after, then the run is recovered on another handle.

/// A shipment service: `create` (at most once) answers as its scenario
/// says, `lookup` finds what a create may have left, `check` (at most once)
/// refuses its first call before doing anything. Dispatches are counted
/// across executions, as an external recipient would.
struct Shipments {
    unknown: bool,
    dispatches: std::sync::Mutex<BTreeMap<String, usize>>,
}
impl Shipments {
    fn count(&self, name: &str) -> usize {
        self.dispatches.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}
#[async_trait::async_trait]
impl ows::run::Functions for Shipments {
    fn has(&self, _: &str) -> bool {
        true
    }
    fn at_most_once(&self, name: &str, _: &Value) -> bool {
        name != "lookup"
    }
    async fn call(&self, _: &str, _: Value, _: &ows::run::CallInfo<'_>) -> Result<Value, WorkflowError> {
        unreachable!("durable runs invoke")
    }
    async fn invoke(
        &self,
        name: &str,
        _: Value,
        _: &ows::run::CallInfo<'_>,
    ) -> Result<Value, ows::durable::CallFailure> {
        use ows::durable::CallFailure;
        let before = {
            let mut dispatches = self.dispatches.lock().unwrap();
            let n = dispatches.entry(name.into()).or_default();
            *n += 1;
            *n - 1
        };
        match name {
            "create" if self.unknown => Err(CallFailure::answer(WorkflowError::unknown_outcome("timed out"))),
            "create" => Ok(json!({ "shipment": 7 })),
            "lookup" => Ok(json!({ "found": 7 })),
            _ if before == 0 => Err(CallFailure::answer(WorkflowError::not_executed("busy, nothing done"))),
            _ => Ok(json!({ "checked": true })),
        }
    }
}
/// The run's journal, crashing at save `fail` (from 0) before or after its
/// commit.
struct Crashing {
    lease: Arc<Lease>,
    saves: std::sync::atomic::AtomicUsize,
    fail: usize,
    after: bool,
}
#[async_trait::async_trait]
impl Journal for Crashing {
    async fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.lease.load().await
    }
    async fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        let n = self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(self.after || n != self.fail, "crash before commit {n}");
        self.lease.save(key, record).await?;
        anyhow::ensure!(!self.after || n != self.fail, "crash after commit {n}");
        Ok(())
    }
}
struct FixedTime;
impl ows::run::Clock for FixedTime {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(1000).unwrap()
    }
}
const SHIPMENT: &str = "document: { dsl: '1.0.4', namespace: test, name: shipment, version: '1.0.0' }
do:
  - ship:
      try:
        - create: { call: create }
      catch:
        errors: { with: { type: urn:ouro:errors:call.unknown } }
        do: [ { lookup: { call: lookup } } ]
  - checked:
      try:
        - check: { call: check }
      catch:
        errors: { with: { type: urn:ouro:errors:call.not_executed } }
        retry: { limit: { attempt: { count: 2 } } }
";
/// One run of `SHIPMENT`, crashing at save `fail`, then recovered on a
/// reopened handle: the saves its first execution attempted, and whether
/// it crashed.
async fn shipment(t: &TestStore, unknown: bool, fail: usize, after: bool) -> anyhow::Result<(usize, bool)> {
    use ows::{
        durable::{CallState, CallStatus, Delivery},
        run::{self, Host, Limits, Silent},
    };
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    let wf = ows::model::parse_yaml(SHIPMENT)?;
    let shipments = Arc::new(Shipments { unknown, dispatches: Default::default() });
    let host = Host {
        functions: shipments.clone(),
        observer: Arc::new(Silent),
        clock: Arc::new(FixedTime),
        limits: Limits::default(),
        runtime: json!({}),
    };
    let token = tokio_util::sync::CancellationToken::new();
    let lease_time = Arc::new(LeaseTime(AtomicI64::new(1000)));
    let fence = t.store.start(&admission("r"), 1000).await?.unwrap();
    let lease = Arc::new(Lease { store: t.store.clone(), fence, clock: lease_time.clone() });
    let journal = Crashing { lease: lease.clone(), saves: AtomicUsize::new(0), fail, after };
    let first = run::resume(&host, &wf, "r", json!({}), &token, &journal).await.status;
    let saves = journal.saves.load(Ordering::SeqCst);
    let crashed = matches!(first, Status::Interrupted(_));
    let reopened = (t.reopen)().await?;
    let status = if crashed {
        let now = LEASE_MS + 1001;
        lease_time.0.store(now, Ordering::SeqCst);
        let fence = reopened.claim("r", now).await?.unwrap();
        let current = Arc::new(Lease { store: reopened.clone(), fence, clock: lease_time });
        let status = run::resume(&host, &wf, "r", json!({}), &token, &*current).await.status;
        reopened.finish(&current.fence, &status, now, 3).await?;
        status
    } else {
        t.store.finish(&lease.fence, &first, 1000, 3).await?;
        first
    };
    let records = reopened.inspect("r").await?.unwrap().records;
    // The adapter's own rows hold these call records, not only the engine's
    // reading of them.
    let rows = t.backend.snapshot().await?.rows;
    for (key, record) in records.iter().filter(|(_, r)| matches!(r, Record::Call { .. })) {
        let row = &rows[&(crate::engine::records_of("r"), key.clone())];
        assert_eq!(&serde_json::from_value::<Record>(row.clone())?, record, "persisted row {key}");
    }
    let calls = ows::durable::call_journal(&records);
    let state = |key: &str| calls.iter().find(|c| c.key == key).map(|c| c.state);
    let case = format!("unknown answer {unknown}, crash at {fail} (after commit: {after})");
    for call in &calls {
        let expected = if call.function == "lookup" { Delivery::AtLeastOnce } else { Delivery::AtMostOnce };
        assert_eq!(call.delivery, expected, "{case}: the effective delivery is persisted");
    }
    // At most one dispatch of the create, and its outcome stays honest: a
    // lookup only follows an intent without an answer, which is what any
    // reader of the persisted journal sees too.
    assert!(shipments.count("create") <= 1, "{case}");
    let create = state("/do/0/ship/try/0/create#call");
    let looked_up = state("/do/0/ship/catch/do/0/lookup#call") == Some(CallStatus::Completed);
    assert_eq!(looked_up, create == Some(CallStatus::Unknown), "{case}: {create:?}");
    assert!(!unknown || create == Some(CallStatus::Unknown), "{case}: answered unknown");
    // Every dispatch of the at-most-once check beyond the first is licensed
    // by a persisted refusal of an earlier one.
    let refusals = calls.iter().filter(|c| c.function == "check" && c.state == CallStatus::NotExecuted).count();
    assert!(shipments.count("check") <= refusals + 1, "{case}: {calls:?}");
    let first_check = state("/do/1/checked/try/0/check#call");
    match &status {
        Status::Completed(output) => {
            assert_eq!(output, &json!({ "checked": true }), "{case}");
            assert_eq!(first_check, Some(CallStatus::NotExecuted), "{case}");
            assert_eq!(state("/do/1/checked/try[2]/0/check#call"), Some(CallStatus::Completed), "{case}");
            if let Some(Record::Call { state: CallState::NotExecuted(error), .. }) =
                records.get("/do/1/checked/try/0/check#call")
            {
                assert_eq!(error.type_, ows::durable::CALL_NOT_EXECUTED, "{case}: the refusal is kept as answered");
            }
        }
        // A crash after a check's intent was committed and before its
        // answer was (the refusal, or the retry's result): that check is an
        // intent without an answer, which no catch here names.
        Status::Faulted(error) => {
            assert_eq!(error.type_, ows::durable::CALL_UNKNOWN, "{case}");
            let unanswered = calls.iter().filter(|c| c.function == "check" && c.state == CallStatus::Unknown).count();
            assert_eq!(unanswered, 1, "{case}: {calls:?}");
            assert!(crashed, "{case}");
        }
        other => panic!("{case}: {other:?}"),
    }
    audit(&*reopened).await?;
    Ok((saves, crashed))
}
async fn call_outcomes(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    for unknown in [false, true] {
        let t = factory("outcomes-baseline").await?;
        let (saves, crashed) = shipment(&t, unknown, usize::MAX, false).await?;
        assert!(!crashed);
        report.cases += 1;
        for after in [false, true] {
            for fail in 0..saves {
                let t = factory("outcomes-crash").await?;
                let (_, crashed) = shipment(&t, unknown, fail, after).await?;
                assert!(crashed, "commit {fail} was not reached");
                report.cases += 1;
                report.execution_crashes += 1;
            }
        }
    }
    report.witnesses.extend([
        "a refusal is persisted and licenses one more at-most-once dispatch",
        "an unknown outcome and a crash between dispatch and record leave the same evidence",
    ]);
    Ok(())
}

/// Policy decisions, ingress consumption, cancellation and admission are one
/// transaction. Reopening after an ambiguous reply observes that transaction.
async fn overlap_policies(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let slot = |n| Trigger { at_ms: Some(n), types: vec!["tick".into()], state: json!(n) };
    let fire = async |s: &dyn RunStore, policy, n, max, ingress: Option<&Value>| {
        s.fire(
            &Firing {
                key: "policy",
                expected: Some(&slot(n)),
                next: Some(&slot(n + 1)),
                admit: &[admission(&format!("slot{n}"))],
                busy: Some(&slot(n + 1)),
                ingress,
                overlap: Some(OverlapGuard { policy, max_concurrent_runs: max, ceiling: &slot(n + 1) }),
                ..Firing::default()
            },
            n,
        )
        .await
    };
    for policy in [Overlap::Skip, Overlap::Allow, Overlap::Queue, Overlap::Replace] {
        let t = factory("overlap-policy").await?;
        report.cases += 1;
        let s = &t.store;
        s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0).await?;
        assert_eq!(fire(&**s, policy, 1, 10, None).await?, Fired::Committed);
        let f = s.claim("slot1", 1).await?.unwrap();
        let c = admission("owned");
        let link = ChildLink { parent: "slot1".into(), key: "child".into(), reference: "/child".into() };
        let child = s.start_child(&f, &c, &link, 1).await?.unwrap();
        s.save_record(
            &child,
            "protected",
            &Record::Call {
                logical: "call".into(),
                function: "f".into(),
                arguments: json!({}),
                delivery: ows::durable::Delivery::AtMostOnce,
                state: ows::durable::CallState::Pending,
                annotations: Default::default(),
            },
            1,
        )
        .await?;
        s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 1, 3).await?;
        let e = event("second", "tick", json!(2));
        let answer = fire(&**s, policy, 2, 10, Some(&e)).await?;
        assert_eq!(
            answer,
            match policy {
                Overlap::Skip => Fired::Busy("slot1".into()),
                Overlap::Queue => Fired::Queued,
                _ => Fired::Committed,
            }
        );
        let s = (t.reopen)().await?;
        assert!(s.ingested(&e, 2).await?, "even a skipped event is consumed");
        assert_eq!(
            s.fire(&Firing { key: "policy", expected: Some(&slot(2)), next: Some(&slot(3)), ..Firing::default() }, 2)
                .await?,
            Fired::Changed
        );
        if policy == Overlap::Queue {
            assert_eq!(s.queued("policy").await?, Some(admission("slot2")));
            assert!(s.get("slot2").await?.is_none(), "a buffer is not an admitted run");
            assert_eq!(fire(&*s, policy, 3, 10, None).await?, Fired::Busy("slot1".into()));
            assert!(s.get("slot3").await?.is_none(), "only one buffered slot");
        }
        if policy == Overlap::Replace {
            assert_eq!(s.get("slot1").await?.unwrap().outcome, Some(Status::Cancelled));
            assert_eq!(s.get("owned").await?.unwrap().outcome, Some(Status::Cancelled));
            assert!(s.save_record(&child, "late", &Record::Timer { deadline_ms: 0 }, 3).await.is_err());
            assert!(matches!(
                s.inspect("owned").await?.unwrap().records["protected"],
                Record::Call { state: ows::durable::CallState::Pending, .. }
            ));
        } else {
            s.cancel("slot1", 4).await?;
        }
        assert!(s.queued("policy").await?.is_none());
        assert_eq!(s.get("slot2").await?.is_some(), policy != Overlap::Skip);
        if policy == Overlap::Queue {
            assert_eq!(s.get("slot2").await?.unwrap().admission, admission("slot2"));
            assert_eq!(s.get("slot2").await?.unwrap().admitted_ms, 4, "timeout starts at promotion");
        }
        audit(&*s).await?;
        report.operations += 18;
        // Fail after the overlap decision at different write boundaries. A
        // rollback keeps every row; an ambiguous commit keeps all new rows.
        let points = (0..8).map(CommitFault::BeforeWrite).chain([CommitFault::BeforeCommit, CommitFault::AfterCommit]);
        for point in points {
            let t = factory("overlap-crash").await?;
            report.cases += 1;
            let s = &t.store;
            s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0).await?;
            fire(&**s, policy, 1, 10, None).await?;
            let before = t.backend.snapshot().await?;
            t.faults.arm(point);
            let result = fire(&**s, policy, 2, 10, Some(&e)).await;
            t.faults.clear();
            let reopened = (t.reopen)().await?;
            if result.is_err() && point != CommitFault::AfterCommit {
                assert_eq!(t.backend.snapshot().await?, before, "{policy:?} {point:?}");
                assert_eq!(reopened.trigger("policy").await?, Some(slot(2)));
            } else {
                assert_eq!(reopened.trigger("policy").await?, Some(slot(3)));
                assert!(reopened.ingested(&e, 2).await?);
                assert_eq!(reopened.queued("policy").await?.is_some(), policy == Overlap::Queue);
                assert_eq!(reopened.get("slot2").await?.is_some(), matches!(policy, Overlap::Allow | Overlap::Replace));
                assert_eq!(reopened.get("slot1").await?.unwrap().ended_ms.is_some(), policy == Overlap::Replace);
            }
            audit(&*reopened).await?;
            report.crash_points += 1;
        }
    }
    // A different trigger and manually admitted runs share the definition's
    // ceiling. Ended runs do not count, and cancellation releases capacity.
    let t = factory("overlap-ceiling").await?;
    report.cases += 1;
    let s = &t.store;
    s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0).await?;
    for n in 0..10 {
        s.admit(&admission(&format!("manual{n}")), 0).await?;
    }
    assert_eq!(fire(&**s, Overlap::Allow, 1, 10, None).await?, Fired::Ceiling);
    assert!(s.get("slot1").await?.is_none());
    s.cancel("manual0", 1).await?;
    assert_eq!(fire(&**s, Overlap::Allow, 2, 10, None).await?, Fired::Committed);
    assert_eq!(fire(&**s, Overlap::Allow, 3, 10, None).await?, Fired::Ceiling);
    // Promotion has its own crash boundary, atomic with the previous end.
    for point in [CommitFault::BeforeCommit, CommitFault::AfterCommit] {
        let t = factory("overlap-promotion-crash").await?;
        report.cases += 1;
        let s = &t.store;
        s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0).await?;
        fire(&**s, Overlap::Queue, 1, 10, None).await?;
        fire(&**s, Overlap::Queue, 2, 10, None).await?;
        let before = t.backend.snapshot().await?;
        t.faults.arm(point);
        assert!(s.cancel("slot1", 3).await.is_err());
        t.faults.clear();
        let s = (t.reopen)().await?;
        if point == CommitFault::BeforeCommit {
            assert_eq!(t.backend.snapshot().await?, before);
            s.cancel("slot1", 3).await?;
        }
        assert!(s.queued("policy").await?.is_none());
        assert_eq!(s.get("slot2").await?.unwrap().admission, admission("slot2"));
        assert!(s.get("slot1").await?.unwrap().ended_ms.is_some());
        report.crash_points += 1;
    }
    for trip in [false, true] {
        let t = factory("overlap-epoch").await?;
        report.cases += 1;
        let s = &t.store;
        let d = s.save_definition("workflow", "original").await?;
        let armed = Arming { triggers: BTreeMap::from([("policy".into(), slot(1))]) };
        s.activate_with("workflow", d.version, 0, &armed, &Activation::at(0)).await?;
        fire(&**s, Overlap::Queue, 1, 10, None).await?;
        fire(&**s, Overlap::Queue, 2, 10, None).await?;
        assert!(s.queued("policy").await?.is_some());
        if trip {
            let f = s.claim("slot1", 2).await?.unwrap();
            s.finish(&f, &Status::Faulted(WorkflowError::runtime("trip")), 3, 1).await?;
            assert!(s.trigger("policy").await?.is_none());
        } else {
            s.activate_with(
                "workflow",
                d.version,
                1,
                &Arming { triggers: BTreeMap::from([("policy".into(), slot(10))]) },
                &Activation::at(0),
            )
            .await?;
            s.cancel("slot1", 3).await?;
            assert_eq!(s.trigger("policy").await?, Some(slot(10)));
        }
        assert!(s.queued("policy").await?.is_none(), "a replaced epoch discards its buffer");
        assert!(s.get("slot2").await?.is_none(), "an old buffered slot cannot be resurrected");
    }
    // Independent trigger CAS operations still contend on the definition's
    // capacity range, so several workers cannot overbook its last permits.
    let t = factory("overlap-ceiling-race").await?;
    report.cases += 1;
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for n in 0..8 {
        let k = format!("race{n}");
        t.store.swap_trigger(&k, None, Some(&slot(1)), &[], None, 0).await?;
        let s = (t.reopen)().await?;
        let barrier = barrier.clone();
        workers.push(tokio::spawn(async move {
            let timer = Trigger { at_ms: Some(1), types: vec!["tick".into()], state: json!(1) };
            barrier.wait().await;
            s.fire(
                &Firing {
                    key: &k,
                    expected: Some(&timer),
                    next: Some(&timer),
                    admit: &[admission(&k)],
                    busy: Some(&timer),
                    overlap: Some(OverlapGuard { policy: Overlap::Allow, max_concurrent_runs: 2, ceiling: &timer }),
                    ..Firing::default()
                },
                1,
            )
            .await
        }));
    }
    let mut answers = Vec::new();
    for worker in workers {
        answers.push(worker.await.expect("firing worker panicked")?);
    }
    assert_eq!(answers.iter().filter(|a| **a == Fired::Committed).count(), 2);
    assert_eq!(answers.iter().filter(|a| **a == Fired::Ceiling).count(), 6);
    assert_eq!(t.store.list(None, "race").await?.len(), 2);
    let rows = t.backend.snapshot().await?.rows;
    assert_eq!(rows.keys().filter(|(kind, _)| kind.starts_with("live_name:")).count(), 2);
    assert_eq!(rows.keys().filter(|(kind, _)| kind.starts_with("trigger_live:")).count(), 2);
    // A host refusal is a slot outcome too (#15): overlap decides whether
    // it is skipped, recorded now, or pinned until queue promotion.
    for policy in [Overlap::Skip, Overlap::Allow, Overlap::Queue, Overlap::Replace] {
        let t = factory("overlap-refused").await?;
        report.cases += 1;
        let s = &t.store;
        let d = s.save_definition("workflow", "original").await?;
        s.activate_with(
            "workflow",
            d.version,
            0,
            &Arming { triggers: BTreeMap::from([("policy".into(), slot(1))]) },
            &Activation::at(0),
        )
        .await?;
        fire(&**s, policy, 1, 10, None).await?;
        let refused = Refused {
            admission: admission("slot2"),
            error: WorkflowError::runtime("pinned refusal"),
            breaker_threshold: 2,
        };
        let result = s
            .fire(
                &Firing {
                    key: "policy",
                    expected: Some(&slot(2)),
                    next: Some(&slot(3)),
                    busy: Some(&slot(3)),
                    refused: Some(&refused),
                    overlap: Some(OverlapGuard { policy, max_concurrent_runs: 10, ceiling: &slot(3) }),
                    ..Firing::default()
                },
                2,
            )
            .await?;
        assert_eq!(
            result,
            match policy {
                Overlap::Skip => Fired::Busy("slot1".into()),
                Overlap::Queue => Fired::Queued,
                _ => Fired::Committed,
            }
        );
        let s = (t.reopen)().await?;
        if policy == Overlap::Skip {
            assert!(s.get("slot2").await?.is_none());
            continue;
        }
        if policy == Overlap::Queue {
            assert_eq!(s.queued("policy").await?, Some(refused.admission.clone()));
            assert_eq!(s.definitions("workflow").await?.consecutive_faults, 0);
            let f = s.claim("slot1", 2).await?.unwrap();
            // Both observations commit together: the old run faults, then
            // promotion records the queued refusal and trips the breaker.
            t.faults.arm(CommitFault::BeforeCommit);
            let before = t.backend.snapshot().await?;
            assert!(s.finish(&f, &Status::Faulted(WorkflowError::runtime("first fault")), 3, 2).await.is_err());
            t.faults.clear();
            assert_eq!(t.backend.snapshot().await?, before);
            s.finish(&f, &Status::Faulted(WorkflowError::runtime("first fault")), 3, 2).await?;
            assert_eq!(s.definitions("workflow").await?.consecutive_faults, 2);
            assert!(s.definitions("workflow").await?.tripped.is_some());
            assert_eq!(s.notices(10).await?.len(), 1);
            assert!(s.queued("policy").await?.is_none());
        } else {
            assert_eq!(s.definitions("workflow").await?.consecutive_faults, 1);
        }
        assert_eq!(s.get("slot2").await?.unwrap().outcome, Some(Status::Faulted(refused.error)));
        assert!(s.get("slot2").await?.unwrap().ended_ms.is_some());
        assert!(s.claim("slot2", 4).await?.is_none(), "a refused buffer is never executable");
    }
    for policy in [Overlap::Skip, Overlap::Allow, Overlap::Queue, Overlap::Replace] {
        let t = factory("overlap-idempotency").await?;
        report.cases += 1;
        let s = &t.store;
        let timer = slot(1);
        s.swap_trigger("policy", None, Some(&timer), &[], None, 0).await?;
        let ingress = event("repeated-slot", "tick", json!(1));
        let a = admission("stable-slot");
        let firing = Firing {
            key: "policy",
            expected: Some(&timer),
            next: Some(&timer),
            admit: std::slice::from_ref(&a),
            ingress: Some(&ingress),
            busy: Some(&timer),
            overlap: Some(OverlapGuard { policy, max_concurrent_runs: 1, ceiling: &timer }),
            ..Firing::default()
        };
        assert_eq!(s.fire(&firing, 0).await?, Fired::Committed);
        let reopened = (t.reopen)().await?;
        assert_eq!(reopened.fire(&firing, reopened.policy().ingress_ms + 1).await?, Fired::Committed);
        assert_eq!(reopened.get("stable-slot").await?.unwrap().admission, a);
        assert!(
            reopened.get("stable-slot").await?.unwrap().ended_ms.is_none(),
            "replace must not cancel its own idempotent slot"
        );
        assert_eq!(reopened.list(None, "stable-slot").await?.len(), 1);
        assert!(reopened.queued("policy").await?.is_none());
    }
    report.witnesses.insert("all overlap policies and queue promotion survive decision/admission crashes");
    Ok(())
}

/// A listen with an expression, a literal filter and expected correlation.
/// Kept public so adapter scalability tests exercise the same matcher.
pub fn control_wait() -> ows::events::PendingWait {
    use ows::{
        events::Consumer,
        expr::Args,
        model::{Correlation, EventFilter, Strategy},
    };
    let strategy = Strategy::One(EventFilter {
        with: json!({"type":"answer", "data":{"approved":"${ . == true }"}}).as_object().unwrap().clone(),
        correlate: vec![(
            "ticket".into(),
            Correlation { from: ".ticket".into(), expect: Some("${ $input.ticket }".into()) },
        )],
    });
    let progress = Consumer::new(&strategy).progress();
    ows::events::PendingWait {
        types: vec!["answer".into()],
        strategy,
        input: json!({"ticket":42}),
        args: Args::new().with("input", json!({"ticket":42})),
        progress,
        position: "listen#event[0]".into(),
        since_ms: 2,
        timeout_ms: Some(100),
        redacted: false,
    }
}
fn control_request(id: &str) -> control::Request {
    control::Request { id: id.into(), actor: "operator".into(), reason: "maintenance".into() }
}
/// The typed refusal of a control operation that must fail.
fn refusal<T: std::fmt::Debug>(result: anyhow::Result<T>) -> control::ControlError {
    control::ControlError::from(result.expect_err("the control operation must be refused"))
}
async fn controls(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::{Command, ControlError, Entity, Reply, Runs, Waits};
    let t = factory("control").await?;
    report.cases += 1;
    let s = &t.store;
    let d = s.save_definition("workflow", "yaml").await?;
    let trigger = Trigger { at_ms: Some(4), types: vec!["start".into()], state: json!({}) };
    let arming = Arming { triggers: BTreeMap::from([("schedule".into(), trigger.clone())]) };
    s.activate_with("workflow", d.version, 0, &arming, &Activation::at(0)).await?;
    let parent = s.start(&admission("p"), 0).await?.unwrap();
    let child = s
        .start_child(
            &parent,
            &admission("c"),
            &ChildLink { parent: "p".into(), key: "c".into(), reference: "c".into() },
            0,
        )
        .await?
        .unwrap();
    s.start_child(
        &child,
        &admission("g"),
        &ChildLink { parent: "c".into(), key: "g".into(), reference: "g".into() },
        0,
    )
    .await?;
    let independent = s.start(&admission("independent"), 0).await?.unwrap();
    let pause = Command::Pause { definition: "workflow".into(), note: "investigate".into() };
    assert_eq!(s.control(&control_request("pause"), &pause, 1).await?, Reply::Changed(true));
    let remote = (t.reopen)().await?;
    let defs = remote.definitions("workflow").await?;
    let trip = defs.tripped.unwrap();
    assert_eq!(trip.reason.type_, "manual");
    assert_eq!(
        trip.manual.unwrap(),
        control::ManualTrip { actor: "operator".into(), reason: "maintenance".into(), note: "investigate".into() }
    );
    assert_eq!(defs.epoch, 1);
    assert!(remote.trigger("schedule").await?.is_none());
    assert!(remote.admit(&admission("blocked"), 1).await.unwrap_err().is::<DefinitionTripped>());
    assert!(remote.start(&admission("blocked"), 1).await.unwrap_err().is::<DefinitionTripped>());
    assert!(
        remote
            .start_child(
                &parent,
                &admission("blocked"),
                &ChildLink { parent: "p".into(), key: "blocked".into(), reference: "blocked".into() },
                1
            )
            .await
            .unwrap_err()
            .is::<DefinitionTripped>()
    );
    // A manual pause refuses every version, pinned or not: other content is
    // refused too (an automatic trip would admit it), until `resume`.
    let other = s.save_definition("workflow", "other content").await?;
    let mut elsewhere = admission("elsewhere");
    elsewhere.version = other.version;
    elsewhere.definition = other.yaml.clone();
    elsewhere.runtime = json!({ PINNED: true });
    let refused = remote.start(&elsewhere, 1).await.unwrap_err();
    assert!(refused.downcast_ref::<DefinitionTripped>().is_some_and(|t| t.paused), "{refused}");
    assert!(remote.admit(&elsewhere, 1).await.unwrap_err().is::<DefinitionTripped>());
    assert!(remote.get("elsewhere").await?.is_none());
    assert_eq!(
        s.control(&control_request("resume"), &Command::Resume { definition: "workflow".into() }, 2).await?,
        Reply::Changed(true)
    );
    assert!(s.start(&elsewhere, 2).await?.is_some(), "other content is admitted again after the resume");
    assert!(s.cancel("elsewhere", 2).await?);
    assert_eq!(s.definitions("workflow").await?.epoch, 1);
    assert_eq!(s.trigger("schedule").await?, Some(trigger.clone()));
    // A late retry must not undo the intervening resume.
    assert_eq!(remote.control(&control_request("pause"), &pause, 3).await?, Reply::Changed(true));
    assert!(s.definitions("workflow").await?.tripped.is_none());
    assert!(matches!(
        refusal(s.control(&control_request("pause"), &Command::Stop { enabled: true }, 3).await),
        ControlError::RequestConflict { id } if id == "pause"
    ));
    let mut impostor = control_request("pause");
    impostor.actor = "someone else".into();
    assert!(matches!(refusal(s.control(&impostor, &pause, 3).await), ControlError::RequestConflict { .. }));
    let blank = control::Request { reason: " ".into(), ..control_request("blank") };
    assert!(matches!(refusal(s.control(&blank, &pause, 3).await), ControlError::InvalidInput(_)));
    let missing = Command::Pause { definition: "missing".into(), note: String::new() };
    assert!(matches!(
        refusal(s.control(&control_request("missing"), &missing, 3).await),
        ControlError::NotFound { entity: Entity::Definition, id } if id == "missing"
    ));
    let resume_missing = Command::Resume { definition: "missing".into() };
    assert!(matches!(
        refusal(s.control(&control_request("resume-missing"), &resume_missing, 3).await),
        ControlError::NotFound { entity: Entity::Definition, .. }
    ));
    s.save_definition("inactive", "yaml").await?;
    let inactive = Command::Pause { definition: "inactive".into(), note: String::new() };
    assert!(matches!(
        refusal(s.control(&control_request("inactive"), &inactive, 3).await),
        ControlError::InvalidState(_)
    ));
    s.control(&control_request("pause-again"), &pause, 3).await?;
    s.activate_with("workflow", d.version, 1, &arming, &Activation::at(0)).await?;
    assert!(s.definitions("workflow").await?.tripped.is_none());
    assert_eq!(s.definitions("workflow").await?.epoch, 2);

    s.register_wait(&parent, &["answer".into()], 2).await?;
    s.subscribe(&parent, "listen", &["answer".into()], 2).await?;
    let mut wait = control_wait();
    s.describe_wait(&parent, "listen", &wait, 2).await?;
    wait.since_ms = 3;
    s.describe_wait(&parent, "listen", &wait, 3).await?;
    assert_eq!(
        remote
            .pending_waits(
                &Waits { definition: Some("workflow".into()), event_type: Some("answer".into()), ..Waits::default() },
                None,
                10
            )
            .await?
            .items[0]
            .waiting
            .since_ms,
        2
    );
    let inspected = remote.inspect_control("p").await?.unwrap();
    assert_eq!(inspected.snapshot.run.fence.as_ref(), Some(&parent));
    assert_eq!(inspected.snapshot.run.lease_until_ms, Some(LEASE_MS));
    assert_eq!(inspected.children.len(), 1);
    assert_eq!(inspected.waits.len(), 1);
    assert_eq!(inspected.budget_spent, 0);
    let response = |ticket, approved| {
        let mut e = event("response", "answer", json!({"approved":approved}));
        e["ticket"] = json!(ticket);
        e
    };
    for (id, event) in [("wrong-correlation", response(43, true)), ("wrong-filter", response(42, false))] {
        let respond = Command::Respond { run: "p".into(), wait: "listen".into(), event };
        assert!(matches!(
            refusal(remote.control(&control_request(id), &respond, 4).await),
            ControlError::WaitMismatch { run, wait } if run == "p" && wait == "listen"
        ));
    }
    let elsewhere = Command::Respond { run: "nobody".into(), wait: "listen".into(), event: response(42, true) };
    assert!(matches!(
        refusal(remote.control(&control_request("nobody"), &elsewhere, 4).await),
        ControlError::NotFound { entity: Entity::Run, id } if id == "nobody"
    ));
    let unopened = Command::Respond { run: "p".into(), wait: "other".into(), event: response(42, true) };
    assert!(matches!(
        refusal(remote.control(&control_request("unopened"), &unopened, 4).await),
        ControlError::WaitClosed { .. }
    ));
    let anonymous = Command::Respond { run: "p".into(), wait: "listen".into(), event: json!({"type": "answer"}) };
    assert!(matches!(
        refusal(remote.control(&control_request("anonymous"), &anonymous, 4).await),
        ControlError::InvalidInput(_)
    ));
    assert!(s.inspect("p").await?.unwrap().events.is_empty());
    let answer = Command::Respond { run: "p".into(), wait: "listen".into(), event: response(42, true) };
    let reply = remote.control(&control_request("answer"), &answer, 4).await?;
    assert_eq!(reply, Reply::Responded(Some("p".into())));
    assert_eq!(s.take_event(&parent, "listen#event[0]", &["answer".into()], 4).await?.unwrap()["ticket"], 42);
    assert!(s.pending_waits(&Waits::default(), None, 10).await?.items.is_empty());
    assert_eq!(s.control(&control_request("answer-retry"), &answer, 5).await?, reply);
    let mut conflict = answer.clone();
    if let Command::Respond { event, .. } = &mut conflict {
        event["ticket"] = json!(43);
    }
    assert!(matches!(
        refusal(s.control(&control_request("answer-conflict"), &conflict, 5).await),
        ControlError::ResponseConflict { .. }
    ));
    let mut late = answer.clone();
    if let Command::Respond { event, .. } = &mut late {
        event["id"] = json!("late-response");
    }
    assert!(matches!(
        refusal(s.control(&control_request("answer-late"), &late, 5).await),
        ControlError::WaitClosed { .. }
    ));
    assert_eq!(s.control_history(Some("p"), None, 10).await?.items.len(), 2);
    assert_eq!(s.control_entry("answer").await?.unwrap().request.actor, "operator");

    s.spend(&parent, "control-jump", 100, 5).await?;
    assert_eq!(s.inspect_control("p").await?.unwrap().budget_spent, 1);
    let stop = Command::Stop { enabled: true };
    s.control(&control_request("stop"), &stop, 5).await?;
    assert!(remote.stopped().await?);
    assert!(remote.due(LEASE_MS, 10).await?.is_empty());
    assert!(remote.due_triggers(10, 10).await?.is_empty());
    assert!(remote.triggers_for("start", 10).await?.is_empty());
    assert!(remote.claim("independent", LEASE_MS).await?.is_none());
    assert!(remote.admit(&admission("stopped"), 5).await.unwrap_err().is::<control::Stopped>());
    assert!(remote.start(&admission("stopped"), 5).await.unwrap_err().is::<control::Stopped>());
    assert!(remote.renew(&parent, 5).await.unwrap_err().is::<control::Stopped>());
    assert!(remote.save_record(&parent, "blocked", &Record::Timer { deadline_ms: 99 }, 5).await.is_err());
    assert!(remote.spend(&parent, "blocked-jump", 10, 5).await.is_err());
    assert_eq!(
        remote.fire(&Firing { key: "schedule", expected: Some(&trigger), next: None, ..Firing::default() }, 5).await?,
        Fired::Changed
    );
    assert_eq!(remote.trigger("schedule").await?, Some(trigger));
    remote.release(&independent, 5).await?;
    s.control(&control_request("clear"), &Command::Stop { enabled: false }, 6).await?;
    s.control(&control_request("stop"), &stop, 7).await?;
    assert!(!s.stopped().await?);
    assert!(s.claim("independent", 7).await?.is_some());

    let cancel = Command::Cancel { run: "p".into() };
    assert_eq!(remote.control(&control_request("cancel"), &cancel, 8).await?, Reply::Changed(true));
    for id in ["p", "c", "g"] {
        assert_eq!(s.get(id).await?.unwrap().state, RunState::Cancelled);
    }
    assert_eq!(remote.control(&control_request("cancel"), &cancel, 9).await?, Reply::Changed(true));
    assert_eq!(remote.control(&control_request("cancel-again"), &cancel, 9).await?, Reply::Changed(false));
    assert!(s.get("independent").await?.unwrap().ended_ms.is_none());
    assert_eq!(s.inspect_control("p").await?.unwrap().children[0].state, RunState::Cancelled);
    assert_eq!(s.inspect_control("p").await?.unwrap().budget_spent, 1);
    assert_eq!(s.inspect_control("c").await?.unwrap().budget_spent, 1);
    // All indexes retain terminal history and parent links.
    let filter = Runs {
        definition: Some("workflow".into()),
        state: Some(RunState::Cancelled),
        since_ms: Some(0),
        before_ms: Some(10),
        ..Runs::default()
    };
    let mut ids = BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = remote.query_runs(&filter, false, cursor.as_ref(), 1).await?;
        for r in page.items {
            assert!(ids.insert(r.admission.id));
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, BTreeSet::from(["p".into(), "c".into(), "g".into(), "elsewhere".into()]));
    assert_eq!(
        s.query_runs(&Runs { parent: Some("p".into()), ..filter.clone() }, false, None, 10).await?.items.len(),
        1
    );
    assert_eq!(s.query_runs(&filter, true, None, 10).await?.items.len(), 4);
    let page = s.query_runs(&filter, false, None, 1).await?;
    let invalid = |r| matches!(refusal(r), ControlError::InvalidInput(_));
    assert!(invalid(s.query_runs(&Runs::default(), false, page.next.as_ref(), 1).await.map(|_| ())));
    assert!(invalid(s.query_runs(&filter, false, None, 0).await.map(|_| ())));
    assert!(invalid(s.query_runs(&filter, false, None, control::MAX_PAGE + 1).await.map(|_| ())));
    let inverted = Runs { since_ms: Some(10), before_ms: Some(0), ..Runs::default() };
    assert!(invalid(s.query_runs(&inverted, true, None, 10).await.map(|_| ())));
    assert!(invalid(s.pending_waits(&Waits::default(), page.next.as_ref(), 10).await.map(|_| ())));
    assert!(invalid(s.control_history(None, None, 0).await.map(|_| ())));
    assert!(invalid(s.query_definitions(None, control::MAX_PAGE + 1).await.map(|_| ())));
    audit(&*remote).await?;
    audit_control(&t).await?;
    report.operations += 80;
    report.witnesses.insert("store-only attributed control, bounded pages, waits and matched responses");

    let t = factory("control-concurrent").await?;
    report.cases += 1;
    let f = t.store.start(&admission("r"), 0).await?.unwrap();
    t.store.describe_wait(&f, "listen", &control_wait(), 2).await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut racers = Vec::new();
    for _ in 0..2 {
        let store = (t.reopen)().await?;
        let barrier = barrier.clone();
        let answer = response(42, true);
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .control(
                    &control_request("race"),
                    &Command::Respond { run: "r".into(), wait: "listen".into(), event: answer },
                    3,
                )
                .await
        }));
    }
    for racer in racers {
        assert_eq!(racer.await.expect("control racer panicked")?, Reply::Responded(Some("r".into())));
    }
    assert_eq!(t.store.inspect("r").await?.unwrap().events.len(), 1);
    assert_eq!(t.store.control_history(Some("r"), None, 10).await?.items.len(), 1);
    audit_control(&t).await?;

    let t = factory("control-lifecycle-route").await?;
    report.cases += 1;
    let s = &t.store;
    let parent = s.start(&admission("p"), 0).await?.unwrap();
    s.start_child(
        &parent,
        &admission("c"),
        &ChildLink { parent: "p".into(), key: "c".into(), reference: "c".into() },
        0,
    )
    .await?;
    // Internal eligibility must still become addressable as soon as a host
    // explicitly registers a lifecycle type, even before its first take.
    s.register_wait(&parent, &["waiting:c".into()], 0).await?;
    assert_eq!(s.deliver(&event("explicit", "waiting:c", json!(true)), None, 0).await?, Some("p".into()));
    assert!(s.take_event(&parent, "explicit", &["waiting:c".into()], 0).await?.is_some());
    audit(&**s).await?;

    // The audit row, event receipt and each command's indexes commit together.
    for op in ["cancel", "pause", "resume", "stop", "clear", "hold", "release", "respond", "describe"] {
        for fault in [
            CommitFault::BeforeWrite(0),
            CommitFault::BeforeWrite(1),
            CommitFault::BeforeCommit,
            CommitFault::AfterCommit,
        ] {
            let t = factory("control-fault").await?;
            report.cases += 1;
            let s = &t.store;
            let d = s.save_definition("workflow", "yaml").await?;
            s.activate_with("workflow", d.version, 0, &arming, &Activation::at(0)).await?;
            let f = s.start(&admission("r"), 0).await?.unwrap();
            s.describe_wait(&f, "listen", &control_wait(), 2).await?;
            if op == "resume" {
                s.control(&control_request("prepare"), &pause, 2).await?;
            }
            if op == "clear" {
                s.control(&control_request("prepare"), &stop, 2).await?;
            }
            if op == "release" {
                let hold = Command::Hold { run: "r".into(), note: String::new() };
                s.control(&control_request("prepare"), &hold, 2).await?;
            }
            let command = match op {
                "cancel" => Command::Cancel { run: "r".into() },
                "hold" => Command::Hold { run: "r".into(), note: "later".into() },
                "release" => Command::Release { run: "r".into() },
                "pause" | "resume" => {
                    if op == "pause" {
                        pause.clone()
                    } else {
                        Command::Resume { definition: "workflow".into() }
                    }
                }
                "stop" => stop.clone(),
                "clear" => Command::Stop { enabled: false },
                _ => Command::Respond { run: "r".into(), wait: "listen".into(), event: response(42, true) },
            };
            let before = t.backend.snapshot().await?;
            t.faults.arm(fault);
            let result = if op == "describe" {
                let mut w = control_wait();
                w.position = "next".into();
                s.describe_wait(&f, "listen", &w, 3).await
            } else {
                s.control(&control_request("command"), &command, 3).await.map(|_| ())
            };
            t.faults.clear();
            // An injected storage failure is no operator refusal: it stays
            // `Storage`, displayed as the failure itself.
            let failure = result.as_ref().expect_err("an injected fault fails the command").to_string();
            let typed = refusal(result);
            assert!(matches!(&typed, control::ControlError::Storage(_)), "control {op} at {fault:?}: {typed:?}");
            assert_eq!(typed.to_string(), failure);
            let fresh = (t.reopen)().await?;
            if fault == CommitFault::AfterCommit {
                assert_ne!(t.backend.snapshot().await?, before);
                if op != "describe" {
                    assert_eq!(
                        fresh.control_entry("command").await?.unwrap().command,
                        normalize_test_command(&command)
                    );
                }
            } else {
                assert_eq!(t.backend.snapshot().await?, before, "atomic control {op}");
            }
            audit(&*fresh).await?;
            audit_control(&t).await?;
            report.crash_points += 1;
        }
    }
    Ok(())
}
fn normalize_test_command(command: &control::Command) -> control::Command {
    let mut command = command.clone();
    if let control::Command::Respond { event: Value::Object(m), .. } = &mut command {
        m.remove("time");
    }
    command
}

async fn audit_control(t: &TestStore) -> anyhow::Result<()> {
    // Derive expected index rows from persisted primary facts, independently
    // of query answers and of the transition engine's index generator.
    let rows = t.backend.snapshot().await?.rows;
    let hex = |v: &str| v.bytes().map(|b| format!("{b:02x}")).collect::<String>();
    let time = |ms: i64, id: &str| format!("{:016x}:{id}", u64::from_be_bytes((ms ^ i64::MIN).to_be_bytes()));
    let mut expected = Rows::new();
    for ((kind, _), value) in &rows {
        if kind == "run" {
            let r: SavedRun = serde_json::from_value(value["run"].clone())?;
            let id = &r.admission.id;
            // A held run is due to nobody: no row of the due indexes names it.
            if r.held.is_some() {
                assert!(
                    !rows
                        .iter()
                        .any(|((kind, _), v)| (kind == "due" || kind.starts_with("due_app:")) && v == &json!(id)),
                    "held run {id} is in a due index"
                );
            }
            for base in ["control_run".to_string(), format!("control_run:{}", hex(&r.admission.name))] {
                expected.insert((base, time(r.admitted_ms, id)), json!(id));
            }
            if let Some(end) = r.ended_ms {
                for base in ["control_end".to_string(), format!("control_end:{}", hex(&r.admission.name))] {
                    expected.insert((base, time(end, id)), json!(id));
                }
            }
            if let Some(link) = r.child {
                expected.insert((format!("control_parent:{}", hex(&link.parent)), time(r.admitted_ms, id)), json!(id));
            }
            let waits: BTreeMap<String, ows::events::PendingWait> = serde_json::from_value(value["waits"].clone())?;
            assert!(r.ended_ms.is_none() || waits.is_empty());
            for (key, waiting) in waits {
                let k = time(waiting.since_ms, &format!("{}:{}", hex(id), hex(&key)));
                let mut families = BTreeSet::from([
                    "control_wait".to_string(),
                    format!("control_wait_run:{}", hex(id)),
                    format!("control_wait_definition:{}", hex(&r.admission.name)),
                ]);
                for ty in &waiting.types {
                    families.insert(format!("control_wait_type:{}", hex(ty)));
                    families.insert(format!("control_wait_definition:{}:type:{}", hex(&r.admission.name), hex(ty)));
                }
                let v = serde_json::to_value(control::Wait {
                    run: id.clone(),
                    definition: r.admission.name.clone(),
                    key,
                    waiting,
                })?;
                for family in families {
                    expected.insert((family, k.clone()), v.clone());
                }
            }
        }
        if kind == "control_command" {
            let e: control::Entry = serde_json::from_value(value.clone())?;
            let k = time(e.at_ms, &hex(&e.request.id));
            expected.insert(("control_history".into(), k.clone()), value.clone());
            if let control::Command::Cancel { run }
            | control::Command::Respond { run, .. }
            | control::Command::Hold { run, .. }
            | control::Command::Release { run } = &e.command
            {
                expected.insert((format!("control_history_run:{}", hex(run)), k), value.clone());
            }
        }
    }
    let actual: Rows = rows
        .into_iter()
        .filter(|((kind, _), _)| {
            ["control_run", "control_end", "control_parent", "control_wait", "control_history"].iter().any(|prefix| {
                kind == prefix
                    || kind.starts_with(&format!("{prefix}:"))
                    || (*prefix == "control_wait" && kind.starts_with("control_wait_"))
                    || (*prefix == "control_history" && kind.starts_with("control_history_run:"))
            })
        })
        .collect();
    assert_eq!(actual, expected, "persisted control indexes disagree with primary facts");
    Ok(())
}

/// An operator's hold on one run and its owned subtree (#29): the run is
/// never claimed or due until released, its worker stops at the next fenced
/// step, the answer of a call already dispatched is still recorded, and a
/// release restores the run exactly.
async fn holds(factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    use control::{Command, ControlError, Entity, Reply};
    use ows::durable::{CallState, Delivery};
    let t = factory("hold").await?;
    report.cases += 1;
    let s = &t.store;
    let remote = (t.reopen)().await?;
    let call = |state: CallState| Record::Call {
        logical: "call".into(),
        function: "fetch".into(),
        arguments: json!({}),
        delivery: Delivery::AtLeastOnce,
        state,
        annotations: Default::default(),
    };
    let child_of = |parent: &str, id: &str| ChildLink { parent: parent.into(), key: id.into(), reference: id.into() };
    let hold = |run: &str| Command::Hold { run: run.into(), note: "look".into() };
    let release = |run: &str| Command::Release { run: run.into() };

    // A tree p > c > g being driven, a waiting run w with a timer, and a free run.
    let parent = s.start(&admission("p"), 0).await?.unwrap();
    s.start_child(&parent, &admission("c"), &child_of("p", "c"), 0).await?;
    let child = s.claim("c", LEASE_MS).await?.unwrap();
    s.start_child(&child, &admission("g"), &child_of("c", "g"), LEASE_MS).await?;
    s.admit(&admission("free"), 0).await?;
    let waiter = s.start(&admission("w"), 0).await?.unwrap();
    s.register_wait(&waiter, &["answer".into()], 1).await?;
    s.finish(&waiter, &Status::Waiting { wake_at_ms: 50 }, 1, 3).await?;
    let before = s.get("w").await?.unwrap();

    // One transaction freezes the run and its live descendants, with the
    // attribution; the free run and the waiter are untouched.
    let now = 10;
    assert_eq!(s.control(&control_request("hold-p"), &hold("p"), now).await?, Reply::Changed(true));
    let held =
        control::Hold { actor: "operator".into(), reason: "maintenance".into(), note: "look".into(), at_ms: now };
    for id in ["p", "c", "g"] {
        assert_eq!(remote.get(id).await?.unwrap().held.as_ref(), Some(&held), "{id}");
    }
    assert!(remote.get("free").await?.unwrap().held.is_none());
    assert_eq!(remote.inspect_control("p").await?.unwrap().snapshot.run.held, Some(held.clone()));
    // Idempotent: a repeat, by any request, changes nothing and keeps the first attribution.
    assert_eq!(s.control(&control_request("hold-p"), &hold("p"), now + 1).await?, Reply::Changed(true));
    assert_eq!(s.control(&control_request("hold-again"), &hold("p"), now + 1).await?, Reply::Changed(false));
    assert_eq!(remote.get("c").await?.unwrap().held.as_ref(), Some(&held));
    assert_eq!(s.control_history(Some("p"), None, 10).await?.items.len(), 2);
    assert!(matches!(
        refusal(s.control(&control_request("hold-none"), &hold("none"), now).await),
        ControlError::NotFound { entity: Entity::Run, id } if id == "none"
    ));
    audit_control(&t).await?;

    // The driving worker stops at its next fenced step; its lease is its own
    // to give back, and the lease renewal path is one of the boundaries.
    let is_held = |e: anyhow::Error| e.is::<control::Held>();
    assert!(is_held(remote.check_lease(&parent, now).await.unwrap_err()));
    assert!(is_held(remote.renew(&parent, now).await.unwrap_err()));
    assert!(is_held(remote.save_record(&parent, "new", &call(CallState::Pending), now).await.unwrap_err()));
    assert!(is_held(remote.spend(&parent, "jump", 10, now).await.unwrap_err()));
    assert!(is_held(remote.register_wait(&parent, &["x".into()], now).await.unwrap_err()));
    assert!(is_held(remote.take_event(&parent, "take", &["x".into()], now).await.unwrap_err()));
    assert!(is_held(remote.finish(&parent, &Status::Completed(json!(1)), now, 3).await.unwrap_err()));
    assert!(is_held(remote.start_child(&parent, &admission("late"), &child_of("p", "late"), now).await.unwrap_err()));
    assert!(remote.get("late").await?.is_none(), "a held run starts no child");
    assert_eq!(remote.get("p").await?.unwrap().fence, Some(parent.clone()), "refusals changed nothing");
    remote.release(&parent, now).await?;
    assert_eq!(s.get("p").await?.unwrap().state, RunState::Ready);

    // Held runs are due to nobody, whenever their wake-up or lease ends.
    let late = 3 * LEASE_MS;
    let mut due = remote.due(late, 10).await?;
    due.sort();
    assert_eq!(due, ["free", "w"], "p, c and g are held");
    assert!(remote.claim("p", late).await?.is_none() && remote.claim("c", late).await?.is_none());
    assert!(remote.claim("g", late).await?.is_none());
    assert!(is_held(remote.renew(&child, LEASE_MS + 5).await.unwrap_err()));
    // A held run still counts: it is live and listed in its state, which
    // `counts` reports as held.
    assert_eq!(s.get("c").await?.unwrap().state, RunState::Running);
    audit(&**s).await?;
    audit_control(&t).await?;

    // An event or a timer does not fire a held run; the work is kept and a
    // release restores the run exactly, due at once.
    assert_eq!(s.control(&control_request("hold-w"), &hold("w"), 20).await?, Reply::Changed(true));
    assert_eq!(s.deliver(&event("answer", "answer", json!(1)), Some("w"), 21).await?, Some("w".into()));
    assert!(!s.due(60, 10).await?.contains(&"w".to_string()));
    assert!(s.claim("w", 60).await?.is_none());
    assert_eq!(s.control(&control_request("release-w"), &release("w"), 61).await?, Reply::Changed(true));
    assert_eq!(s.control(&control_request("release-w-again"), &release("w"), 62).await?, Reply::Changed(false));
    assert!(s.due(61, 10).await?.contains(&"w".to_string()));
    let mut after = s.get("w").await?.unwrap();
    assert_eq!(after.wake_at_ms, 21, "the delivery woke the run; the hold kept it");
    after.wake_at_ms = before.wake_at_ms;
    assert_eq!(after, before, "release restores the run");
    assert!(s.claim("w", 61).await?.is_some());

    // The answer of a call already dispatched is recorded under a hold, and
    // it is the only step that is: nothing else commits.
    let q = s.start(&admission("q"), 0).await?.unwrap();
    s.save_record(&q, "fetch#call", &call(CallState::Pending), 1).await?;
    s.control(&control_request("hold-q"), &hold("q"), 2).await?;
    assert!(is_held(s.save_record(&q, "other#call", &call(CallState::Pending), 3).await.unwrap_err()));
    assert!(is_held(s.save_record(&q, "task", &Record::Timer { deadline_ms: 9 }, 3).await.unwrap_err()));
    s.save_record(&q, "fetch#call", &call(CallState::Done(Ok(json!("ok")))), 3).await?;
    assert!(matches!(
        s.inspect("q").await?.unwrap().records.get("fetch#call"),
        Some(Record::Call { state: CallState::Done(Ok(v)), .. }) if v == &json!("ok")
    ));
    assert!(is_held(s.finish(&q, &Status::Completed(json!(1)), 4, 3).await.unwrap_err()));
    // A pending call keeps its delivery rules: released, the run resumes
    // from the same evidence.
    s.release(&q, 4).await?;
    s.control(&control_request("release-q"), &release("q"), 5).await?;
    assert!(s.claim("q", 5).await?.is_some());

    // A cancel reaches a held run and its descendants, and ends the hold.
    assert_eq!(
        s.control(&control_request("cancel-p"), &Command::Cancel { run: "p".into() }, 70).await?,
        Reply::Changed(true)
    );
    for id in ["p", "c", "g"] {
        let r = s.get(id).await?.unwrap();
        assert!(r.state == RunState::Cancelled && r.held.is_none(), "{id}");
    }
    assert_eq!(s.control(&control_request("hold-ended"), &hold("p"), 71).await?, Reply::Changed(false));
    assert!(s.get("p").await?.unwrap().held.is_none());
    audit(&**s).await?;
    audit_control(&t).await?;

    // Releasing the root releases the subtree it holds.
    let t = factory("hold-release").await?;
    report.cases += 1;
    let s = &t.store;
    let parent = s.start(&admission("p"), 0).await?.unwrap();
    s.start_child(&parent, &admission("c"), &child_of("p", "c"), 0).await?;
    s.release(&parent, 1).await?;
    s.control(&control_request("hold"), &hold("p"), 2).await?;
    assert_eq!(s.control(&control_request("release"), &release("p"), 3).await?, Reply::Changed(true));
    for id in ["p", "c"] {
        assert!(s.get(id).await?.unwrap().held.is_none(), "{id}");
    }
    assert!(s.claim("p", 3).await?.is_some());
    audit_control(&t).await?;
    report.operations += 90;
    report
        .witnesses
        .insert("a hold freezes a run and its subtree, is due to nobody and refuses every step but an answer");
    report.witnesses.insert("a release restores the held run exactly");
    Ok(())
}

async fn control_overlap(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::Command;
    for pause in [false, true] {
        let t = factory("control-overlap").await?;
        report.cases += 1;
        let s = &t.store;
        let d = s.save_definition("workflow", "yaml").await?;
        let slot = |n| Trigger { at_ms: Some(n), types: vec![], state: json!(n) };
        s.activate_with(
            "workflow",
            d.version,
            0,
            &Arming { triggers: BTreeMap::from([("q".into(), slot(0))]) },
            &Activation::at(0),
        )
        .await?;
        let fire = async |n, id: &str| {
            s.fire(
                &Firing {
                    key: "q",
                    expected: Some(&slot(n)),
                    next: Some(&slot(n + 1)),
                    busy: Some(&slot(n + 1)),
                    admit: &[admission(id)],
                    overlap: Some(OverlapGuard {
                        policy: Overlap::Queue,
                        max_concurrent_runs: 2,
                        ceiling: &slot(n + 1),
                    }),
                    ..Firing::default()
                },
                n,
            )
            .await
        };
        assert_eq!(fire(0, "first").await?, Fired::Committed);
        assert_eq!(fire(1, "buffered").await?, Fired::Queued);
        let pinned = s.queued("q").await?.unwrap();
        let stopped = if pause {
            Command::Pause { definition: "workflow".into(), note: "retain buffer".into() }
        } else {
            Command::Stop { enabled: true }
        };
        s.control(&control_request("stop"), &stopped, 2).await?;
        s.control(&control_request("cancel"), &Command::Cancel { run: "first".into() }, 2).await?;
        assert!(s.get("buffered").await?.is_none(), "stopped work must not promote a buffered admission");
        let clear =
            if pause { Command::Resume { definition: "workflow".into() } } else { Command::Stop { enabled: false } };
        let fresh = (t.reopen)().await?;
        fresh.control(&control_request("clear"), &clear, 3).await?;
        assert_eq!(fresh.get("buffered").await?.unwrap().admission, pinned);
        assert!(fresh.queued("q").await?.is_none());
        assert_eq!(fresh.definitions("workflow").await?.epoch, 1);
        audit_control(&t).await?;
        audit(&*fresh).await?;
    }
    Ok(())
}

/// A buffered slot keeps its place when the emergency stop is cleared: a clear
/// drains a bounded batch of deferred definitions, and a definition past the
/// batch must still promote its buffered slot before a later slot of its own
/// trigger, or another start of the definition (an owned child included), is
/// admitted. A drain that trips the definition commits with the refusal it
/// causes, whichever admission path drained it.
async fn queue_order_after_clear(
    factory: &impl AsyncFn(&str) -> anyhow::Result<TestStore>,
    report: &mut Report,
) -> anyhow::Result<()> {
    use control::Command;
    let t = factory("queue-order-after-clear").await?;
    report.cases += 1;
    let s = &t.store;
    let slot = |n| Trigger { at_ms: Some(n), types: vec![], state: json!(n) };
    let named = |name: &str, id: &str| Admission { name: name.into(), ..admission(id) };
    let fire = async |name: &str, n, id: &str| {
        s.fire(
            &Firing {
                key: name,
                expected: Some(&slot(n)),
                next: Some(&slot(n + 1)),
                busy: Some(&slot(n + 1)),
                admit: &[named(name, id)],
                overlap: Some(OverlapGuard { policy: Overlap::Queue, max_concurrent_runs: 2, ceiling: &slot(n + 1) }),
                ..Firing::default()
            },
            n,
        )
        .await
    };
    // The fillers sort first and fill the batch; the last names keep their
    // markers after the clear.
    let mut names: Vec<String> = (0..control::MAX_PAGE).map(|n| format!("fill{n:04}")).collect();
    let late = ["zchild", "zlater", "zstart", "ztrip-admit", "ztrip-child", "ztrip-start"];
    names.extend(late.map(String::from));
    for name in &names {
        let d = s.save_definition(name, "yaml").await?;
        let armed = Arming { triggers: BTreeMap::from([(name.clone(), slot(0))]) };
        s.activate_with(name, d.version, 0, &armed, &Activation::at(0)).await?;
        if name.starts_with("ztrip") {
            // One fault short of the threshold, and a buffered slot whose run
            // the host refuses: its promotion is the fault that trips.
            let f = s.start(&named(name, &format!("{name}:fault")), 0).await?.unwrap();
            s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 2).await?;
        }
        assert_eq!(fire(name, 0, &format!("{name}:first")).await?, Fired::Committed);
        if name.starts_with("ztrip") {
            let refused = Refused {
                admission: named(name, &format!("{name}:buffered")),
                error: WorkflowError::runtime("cannot admit"),
                breaker_threshold: 2,
            };
            let firing = Firing {
                key: name,
                expected: Some(&slot(1)),
                next: Some(&slot(2)),
                busy: Some(&slot(2)),
                refused: Some(&refused),
                overlap: Some(OverlapGuard { policy: Overlap::Queue, max_concurrent_runs: 2, ceiling: &slot(2) }),
                ..Firing::default()
            };
            assert_eq!(s.fire(&firing, 1).await?, Fired::Queued);
        } else {
            assert_eq!(fire(name, 1, &format!("{name}:buffered")).await?, Fired::Queued);
        }
    }
    s.control(&control_request("stop"), &Command::Stop { enabled: true }, 2).await?;
    for name in &names {
        s.control(&control_request(&format!("{name}:cancel")), &Command::Cancel { run: format!("{name}:first") }, 2)
            .await?;
    }
    s.control(&control_request("clear"), &Command::Stop { enabled: false }, 3).await?;
    for name in late {
        assert!(s.get(&format!("{name}:buffered")).await?.is_none(), "the clear's batch skipped {name}");
    }
    // A later slot of the same trigger is buffered behind the older one.
    assert_eq!(fire("zlater", 2, "zlater:next").await?, Fired::Queued);
    assert!(s.get("zlater:buffered").await?.is_some_and(|r| r.ended_ms.is_none()));
    assert!(s.get("zlater:next").await?.is_none());
    assert_eq!(s.queued("zlater").await?, Some(named("zlater", "zlater:next")));
    // Another start of the definition does not take the room first.
    s.admit(&named("zstart", "zstart:manual"), 3).await?;
    assert!(s.get("zstart:buffered").await?.is_some_and(|r| r.ended_ms.is_none()));
    assert!(s.queued("zstart").await?.is_none());
    // Nor does an owned child of the definition, started by a run of another.
    let parent = s.start(&named("parent", "parent"), 3).await?.unwrap();
    let link = |key: &str| ChildLink { parent: "parent".into(), key: key.into(), reference: format!("/{key}") };
    s.start_child(&parent, &named("zchild", "zchild:owned"), &link("owned"), 3).await?.unwrap();
    assert!(s.get("zchild:buffered").await?.is_some_and(|r| r.ended_ms.is_none()));
    assert!(s.queued("zchild").await?.is_none());
    // Promoting the refused slot is the definition's second fault: the trip
    // commits with the drain, and the admission that drained it is refused
    // by that stored trip, its own changes (a child's parent side) rolled back.
    let tripped = |e: anyhow::Error| e.downcast_ref::<DefinitionTripped>().map(|t| t.name.clone());
    let before = s.inspect("parent").await?;
    for (name, refusal) in [
        ("ztrip-admit", s.admit(&named("ztrip-admit", "ztrip-admit:manual"), 3).await.map(drop)),
        (
            "ztrip-child",
            s.start_child(&parent, &named("ztrip-child", "ztrip-child:owned"), &link("trip"), 3).await.map(drop),
        ),
        ("ztrip-start", s.start(&named("ztrip-start", "ztrip-start:manual"), 3).await.map(drop)),
    ] {
        assert_eq!(refusal.map_err(tripped).unwrap_err(), Some(name.to_string()), "{name}");
        let d = s.definitions(name).await?;
        let trip = d.tripped.as_ref().unwrap_or_else(|| panic!("{name}: the drain's trip was rolled back"));
        assert_eq!((trip.faults, trip.run.as_str()), (2, format!("{name}:buffered").as_str()), "{name}");
        assert!(s.get(&format!("{name}:buffered")).await?.is_some_and(|r| r.ended_ms == Some(3)), "{name}");
        assert!(s.get(&format!("{name}:manual")).await?.is_none(), "{name}");
        assert!(s.queued(name).await?.is_none(), "{name}");
    }
    assert_eq!(s.inspect("parent").await?, before, "the parent's side of a refused child is not committed");
    let notices = s.notices(10).await?;
    for name in ["ztrip-admit", "ztrip-child", "ztrip-start"] {
        assert_eq!(notices.iter().filter(|(_, n)| n["subject"] == name).count(), 1, "{name}: one trip, one notice");
    }
    audit(&**s).await?;
    Ok(())
}
