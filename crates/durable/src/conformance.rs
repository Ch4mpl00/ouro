//! Adapter-independent safety and liveness checks. Every audit reloads persisted
//! state. Factories must return a fresh store for each case; reopening must
//! create an independent handle to the same database (memory may share state).
//! Based on the original admission, event-history, child and crash properties.
use crate::*;
use owf::run::WorkflowError;
use serde_json::json;
use std::collections::BTreeSet;

pub struct TestStore {
    pub store: Arc<dyn RunStore>,
    pub reopen: Arc<dyn Fn() -> anyhow::Result<Arc<dyn RunStore>> + Send + Sync>,
    pub faults: Arc<FaultInjector>,
    /// Read actual adapter rows, independently of the transition engine.
    pub backend: Arc<dyn AtomicBackend>,
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
fn audit(store: &dyn RunStore) -> anyhow::Result<()> {
    for r in store.list(None, "")? {
        let s = store.inspect(&r.admission.id)?.unwrap();
        if r.ended_ms.is_some() {
            assert!(s.awaits.is_empty() && s.listeners.is_empty() && s.subscriptions.is_empty());
            assert!(r.fence.is_none());
        }
        // An owned child never outlives its parent: the parent's end, of any
        // kind, cancels it in the same commit (C3).
        if let Some(link) = &r.child
            && let Some(end) = store.get(&link.parent)?.and_then(|p| p.ended_ms)
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
pub fn run_all(factory: impl Fn(&str) -> anyhow::Result<TestStore>) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let t = factory("leases")?;
    report.cases += 1;
    let s = &t.store;
    assert!(s.admit(&admission("r"), 0)?);
    let mut changed = admission("r");
    changed.definition = "changed".into();
    changed.input = json!(2);
    assert!(!s.admit(&changed, 0)?);
    changed.name = "other".into();
    assert!(s.admit(&changed, 0).is_err());
    assert_eq!(s.get("r")?.unwrap().admission, admission("r"));
    let old = s.claim("r", 0)?.unwrap();
    assert!(s.claim("r", 0)?.is_none());
    s.renew(&old, 1)?;
    let record = Record::Timer { deadline_ms: 100 };
    s.save_record(&old, "wait", &record, 2)?;
    assert!(s.save_record(&old, "wait", &Record::Timer { deadline_ms: 200 }, 2).is_err());
    let reopened = (t.reopen)()?;
    let current = reopened.claim("r", LEASE_MS + 2)?.unwrap();
    assert!(s.load_records(&old, LEASE_MS + 2).is_err());
    assert!(s.renew(&old, LEASE_MS + 2).is_err());
    assert!(s.release(&old, LEASE_MS + 2).is_err());
    assert!(s.save_record(&old, "new", &record, LEASE_MS + 2).is_err());
    assert!(s.finish(&old, &Status::Completed(json!(0)), LEASE_MS + 2, 3).is_err());
    assert_eq!(reopened.load_records(&current, LEASE_MS + 2)?.get("wait"), Some(&record));
    reopened.finish(&current, &Status::Completed(json!(42)), LEASE_MS + 2, 3)?;
    assert!(reopened.due(i64::MAX, 100)?.is_empty());
    assert!(reopened.claim("r", i64::MAX)?.is_none());
    audit(&*reopened)?;
    report.operations += 18;
    report.witnesses.insert("expired worker fenced after reopen");

    let t = factory("interruptions")?;
    report.cases += 1;
    let s = &t.store;
    let mut now = 0;
    let mut f = s.start(&admission("r"), now)?.unwrap();
    assert!(s.due(now, 100)?.is_empty());
    assert!(s.start(&admission("r"), now)?.is_none());
    for n in 1..=MAX_INTERRUPTIONS {
        let status = s.finish(&f, &Status::Interrupted(WorkflowError::runtime("corrupt")), now, 3)?;
        audit(&**s)?;
        report.operations += 1;
        if n == MAX_INTERRUPTIONS {
            assert!(matches!(status, Status::Faulted(_)));
            break;
        }
        let r = s.get("r")?.unwrap();
        assert_eq!(r.interruptions, n);
        assert!(r.wake_at_ms > now);
        assert!(s.claim("r", r.wake_at_ms - 1)?.is_none());
        now = r.wake_at_ms;
        f = s.claim("r", now)?.unwrap();
    }
    report.witnesses.insert("interruptions back off and terminate");

    let t = factory("events")?;
    report.cases += 1;
    let s = &t.store;
    let f = s.start(&admission("r"), 0)?.unwrap();
    let types = vec!["answer".into()];
    s.register_wait(&f, &types, 0)?;
    for k in ["first", "second"] {
        assert!(s.take_event(&f, k, &types, 0)?.is_none());
    }
    assert_eq!(s.deliver(&event("1", "answer", json!(1)), None, 1)?, Some("r".into()));
    assert!(s.deliver(&event("2", "answer", json!(2)), None, 1)?.is_none());
    let first = s.take_event(&f, "first", &types, 2)?.unwrap();
    assert_eq!(s.take_event(&f, "first", &types, 2)?, Some(first));
    assert!(s.deliver(&event("2", "answer", json!(2)), None, 2)?.is_some());
    // Delivery wins the race against the worker releasing as waiting.
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3)?;
    let f = s.claim("r", 3)?.unwrap();
    assert_eq!(s.take_event(&f, "second", &types, 3)?.unwrap()["data"], 2);
    s.register_wait(&f, &types, 3)?;
    s.take_event(&f, "timeout", &types, 3)?;
    s.take_event(&f, "sibling", &types, 3)?;
    s.unregister_wait(&f, "timeout", &types, 3)?;
    assert!(s.deliver(&event("3", "answer", json!(3)), None, 3)?.is_some());
    assert!(s.take_event(&f, "sibling", &types, 3)?.is_some());
    s.finish(&f, &Status::Completed(json!(3)), 3, 3)?;
    assert!(s.deliver(&event("4", "answer", json!(4)), None, 3)?.is_none());
    audit(&**s)?;
    report.operations += 20;
    report.witnesses.extend([
        "same-type concurrent listeners",
        "replayed consumption",
        "delivery wake survives release",
        "terminal cleanup",
    ]);

    let t = factory("published")?;
    report.cases += 1;
    let s = &t.store;
    let cloud = |id: &str, t: &str| json!({"specversion":"1.0","id":id,"source":"/test","type":t,"data":{"n":id}});
    let a = s.start(&admission("a"), 0)?.unwrap();
    let b = s.start(&admission("b"), 0)?.unwrap();
    s.start(&admission("idle"), 0)?.unwrap();
    s.subscribe(&a, "listen", &["reading".into()], 0)?;
    s.subscribe(&b, "listen", &["*".into()], 0)?;
    assert_eq!(s.publish(&cloud("1", "reading"), None, 1)?, vec!["a".to_string(), "b".into()]);
    assert!(s.publish(&cloud("1", "reading"), None, 1)?.is_empty(), "republishing is idempotent");
    assert_eq!(s.publish(&cloud("2", "reading"), None, 1)?.len(), 2, "events of one type queue");
    assert_eq!(s.publish(&cloud("3", "other"), None, 1)?, vec!["b".to_string()]);
    assert_eq!(s.publish(&cloud("4", "other"), Some("idle"), 1)?, vec!["idle".to_string()]);
    assert!(s.publish(&json!({"data":1}), None, 1).is_err());
    // Subscriptions outlive consumption; takes keep publication order.
    for (k, n) in [("e0", "1"), ("e1", "2")] {
        assert_eq!(s.take_event(&a, k, &["reading".into()], 2)?.unwrap()["data"]["n"], n);
    }
    assert_eq!(s.publish(&cloud("5", "reading"), None, 2)?, vec!["a".to_string(), "b".into()]);
    // A wildcard takes published events, never targeted deliveries.
    s.register_wait(&b, &["answer".into()], 2)?;
    s.deliver(&event("0", "answer", json!(0)), None, 2)?;
    for (k, n) in [("w0", "1"), ("w1", "2"), ("w2", "3"), ("w3", "5")] {
        let taken = s.take_event(&b, k, &["*".into()], 3)?.unwrap();
        assert_eq!((taken["data"]["n"].as_str(), taken["specversion"].as_str()), (Some(n), Some("1.0")));
    }
    assert!(s.take_event(&b, "w4", &["*".into()], 3)?.is_none());
    // A published event wakes a waiting subscriber, even one already releasing.
    s.finish(&a, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3)?;
    assert_eq!(s.get("a")?.unwrap().wake_at_ms, 3, "a pending subscribed event wakes it");
    let a = s.claim("a", 3)?.unwrap();
    s.take_event(&a, "e2", &["reading".into()], 3)?.unwrap();
    s.finish(&a, &Status::Waiting { wake_at_ms: i64::MAX }, 3, 3)?;
    assert_eq!(s.get("a")?.unwrap().wake_at_ms, i64::MAX);
    s.publish(&cloud("6", "reading"), None, 4)?;
    assert_eq!(s.get("a")?.unwrap().wake_at_ms, 4);
    let a = s.claim("a", 4)?.unwrap();
    s.unsubscribe(&a, "listen", 4)?;
    assert_eq!(s.publish(&cloud("7", "reading"), None, 4)?, vec!["b".to_string()], "unsubscribed");
    s.finish(&b, &Status::Completed(json!(0)), 5, 3)?;
    assert!(s.publish(&cloud("9", "reading"), Some("b"), 5)?.is_empty(), "terminal runs receive nothing");
    audit(&**s)?;
    report.operations += 33;
    report.witnesses.extend([
        "published events reach every subscriber once",
        "subscriptions outlive consumption",
        "wildcards skip targeted deliveries",
    ]);

    let t = factory("triggers")?;
    report.cases += 1;
    let s = &t.store;
    let timer = |at: Option<i64>, n: i64| Trigger { at_ms: at, types: Vec::new(), state: json!({ "n": n }) };
    let on = Trigger { at_ms: None, types: vec!["order".into(), "*".into()], state: json!({}) };
    assert!(s.swap_trigger("late", None, Some(&timer(Some(30), 0)), &[], None, 0)?);
    assert!(s.swap_trigger("early", None, Some(&timer(Some(10), 0)), &[], None, 0)?);
    assert!(!s.swap_trigger("early", None, Some(&timer(Some(5), 0)), &[], None, 0)?, "it exists already");
    assert!(s.swap_trigger("on", None, Some(&on), &[], None, 0)?);
    let due: Vec<String> = s.due_triggers(30, 10)?.into_iter().map(|(k, _)| k).collect();
    assert_eq!(due, vec!["early".to_string(), "late".into()]);
    assert_eq!(s.due_triggers(9, 10)?.len(), 0);
    assert_eq!(s.due_triggers(30, 1)?.len(), 1);
    assert_eq!(s.triggers_for("order", 10)?.len(), 1, "matched once though indexed twice");
    assert_eq!(s.triggers_for("other", 10)?[0].0, "on", "a wildcard reacts to every type");
    assert!(s.triggers_for("order", 0)?.is_empty());
    // Firing claims the slot and admits its run in one commit, once.
    let fired = admission("slot:10");
    let rearm = Rearm { trigger: "early".into(), delay_ms: 100 };
    let parked = timer(None, 1);
    assert!(s.swap_trigger(
        "early",
        Some(&timer(Some(10), 0)),
        Some(&parked),
        std::slice::from_ref(&fired),
        Some(&rearm),
        10
    )?);
    assert!(!s.swap_trigger(
        "early",
        Some(&timer(Some(10), 0)),
        Some(&parked),
        std::slice::from_ref(&fired),
        None,
        10
    )?);
    assert!(s.get("slot:10")?.is_some());
    assert!(s.due_triggers(i64::MAX, 10)?.iter().all(|(k, _)| k != "early"), "parked: not due");
    // The fired run's end makes the trigger due again after the delay.
    let f = s.claim("slot:10", 20)?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 50, 3)?;
    assert_eq!(s.trigger("early")?, Some(timer(Some(150), 1)));
    assert!(s.swap_trigger("on", Some(&on), None, &[], None, 60)?);
    assert!(s.trigger("on")?.is_none() && s.triggers_for("order", 10)?.is_empty());
    audit(&**s)?;
    report.operations += 18;
    report.witnesses.extend(["triggers fire once and re-arm at a run's end"]);

    let t = factory("children")?;
    report.cases += 1;
    let s = &t.store;
    let p = s.start(&admission("p"), 0)?.unwrap();
    let link = ChildLink { parent: "p".into(), key: "round:1".into(), reference: "/start".into() };
    let c = s.start_child(&p, &admission("c"), &link, 0)?.unwrap();
    s.finish(&c, &Status::Waiting { wake_at_ms: 0 }, 1, 3)?;
    assert!(s.take_event(&p, "waiting", &["waiting:c".into()], 1)?.is_some());
    let c = s.claim("c", 1)?.unwrap();
    s.finish(&c, &Status::Completed(json!(42)), 2, 3)?;
    assert!(s.take_event(&p, "done", &["done:c".into()], 2)?.is_some());
    assert!(s.start_child(&p, &admission("c"), &link, 2)?.is_none());
    assert!(s.take_event(&p, "again", &["done:c".into()], 2)?.is_none());
    assert!(s.child_outcome("other", "round:1", "c").is_err());
    assert_eq!(s.child_outcome("p", "round:1", "c")?, Some(Status::Completed(json!(42))));
    let c = s
        .start_child(
            &p,
            &admission("active"),
            &ChildLink { parent: "p".into(), key: "2".into(), reference: "/start".into() },
            2,
        )?
        .unwrap();
    s.start_child(
        &c,
        &admission("nested"),
        &ChildLink { parent: "active".into(), key: "3".into(), reference: "/start".into() },
        2,
    )?;
    s.finish(&p, &Status::Completed(json!(0)), 3, 3)?;
    for id in ["active", "nested"] {
        assert_eq!(s.get(id)?.unwrap().outcome, Some(Status::Cancelled));
    }
    audit(&**s)?;
    report.operations += 16;
    report.witnesses.insert("child completion and cascading cancellation");

    let t = factory("retention")?;
    report.cases += 1;
    let s = &t.store;
    for id in ["old", "recent", "waiting"] {
        let f = s.start(&admission(id), 0)?.unwrap();
        s.save_record(&f, "k", &record, 0)?;
        s.finish(
            &f,
            &if id == "waiting" { Status::Waiting { wake_at_ms: i64::MAX } } else { Status::Completed(json!(id)) },
            if id == "old" { 0 } else { 100 },
            3,
        )?;
    }
    assert_eq!(s.sweep(100, 50)?.records, 1);
    assert!(s.inspect("old")?.unwrap().records.is_empty());
    assert!(!s.inspect("recent")?.unwrap().records.is_empty());
    assert!(!s.inspect("waiting")?.unwrap().records.is_empty());
    assert!(s.claim("old", 100)?.is_none());
    audit(&**s)?;
    report.operations += 14;
    report.witnesses.insert("retention keeps terminal outcome");

    let t = factory("definitions")?;
    report.cases += 1;
    let s = &t.store;
    let first = s.save_definition("workflow", "first")?;
    s.activate("workflow", first.version)?;
    let second = s.save_definition("workflow", "second")?;
    s.activate("workflow", second.version)?;
    let pinned = s.get("absent")?;
    assert!(pinned.is_none());
    for n in 0..3 {
        let mut a = admission(&format!("fault{n}"));
        a.version = second.version;
        a.definition = second.yaml.clone();
        let f = s.start(&a, 0)?.unwrap();
        s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3)?;
    }
    let d = s.definitions("workflow")?;
    assert_eq!((d.active, d.epoch), (Some(second.version), 2), "a trip keeps the active version and the epoch");
    let trip = d.tripped.expect("tripped at the threshold");
    assert_eq!((trip.version, trip.epoch, trip.faults, trip.threshold), (second.version, 2, 3, 3));
    assert_eq!(trip.run, "fault2");
    assert_eq!(s.get("fault0")?.unwrap().admission.version, second.version);
    assert!(s.activate("workflow", 99).is_err());
    report.operations += 13;
    report.witnesses.insert("breaker trips without changing the active version or pinned runs");
    decisions(&factory, &mut report)?;

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
            let t = factory("crash")?;
            report.cases += 1;
            let s = &t.store;
            let f = s.start(&admission("p"), 0)?.unwrap();
            s.register_wait(&f, &["answer".into()], 0)?;
            s.take_event(&f, "event", &["answer".into()], 0)?;
            s.subscribe(&f, "listen", &["answer".into()], 0)?;
            if operation == "take" {
                s.deliver(&event("42", "answer", json!(42)), None, 0)?;
            }
            if operation == "sweep" {
                s.save_record(&f, "timer", &record, 0)?;
                s.finish(&f, &Status::Completed(json!(0)), 0, 3)?;
            }
            if operation == "expire" {
                s.deliver(&event("expiring", "answer", json!(1)), Some("p"), 0)?;
            }
            let d = s.save_definition("workflow", "yaml")?;
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
                let epoch = s.definitions("workflow")?.epoch;
                s.activate_with("workflow", d.version, epoch, &arming)?;
                tripping = s.start(&admission("tripping"), 0)?;
                if operation == "acknowledge" {
                    s.finish(tripping.as_ref().unwrap(), &Status::Faulted(WorkflowError::runtime("fault")), 0, 1)?;
                    notice = s.notices(1)?.pop().map(|(k, _)| k);
                }
            }
            if matches!(operation, "finish" | "cancel") {
                s.activate("workflow", d.version)?;
                let child = s
                    .start_child(
                        &f,
                        &admission("active"),
                        &ChildLink { parent: "p".into(), key: "child".into(), reference: "/child".into() },
                        0,
                    )?
                    .unwrap();
                s.start_child(
                    &child,
                    &admission("nested"),
                    &ChildLink { parent: "active".into(), key: "nested".into(), reference: "/nested".into() },
                    0,
                )?;
            }
            let epoch = s.definitions("workflow")?.epoch;
            let before = t.backend.snapshot()?;
            t.faults.arm(point);
            let result: anyhow::Result<()> = match operation {
                "admit" => s.admit(&admission("new"), 0).map(|_| ()),
                "record" => s.save_record(&f, "timer", &record, 0),
                "register" => s.register_wait(&f, &["another".into()], 0),
                "take" => s.take_event(&f, "event", &["answer".into()], 0).map(|_| ()),
                "deliver" => s.deliver(&event("42", "answer", json!(42)), None, 0).map(|_| ()),
                "child" => s
                    .start_child(
                        &f,
                        &admission("c"),
                        &ChildLink { parent: "p".into(), key: "k".into(), reference: "/child".into() },
                        0,
                    )
                    .map(|_| ()),
                "finish" => s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 3).map(|_| ()),
                "cancel" => s.cancel("p", 0).map(|_| ()),
                "sweep" => s.sweep(1, 0).map(|_| ()),
                "release" => s.release(&f, 0),
                "renew" => s.renew(&f, 1),
                "definition" => s.save_definition("workflow", "next").map(|_| ()),
                "activate" => s.activate("workflow", d.version),
                "subscribe" => s.subscribe(&f, "other", &["*".into()], 0),
                "publish" => s.publish(&json!({"type":"answer","source":"/t","id":"1"}), None, 0).map(|_| ()),
                "trigger" => s
                    .swap_trigger(
                        "slot",
                        None,
                        Some(&Trigger { at_ms: Some(5), types: vec!["answer".into()], state: json!(1) }),
                        &[admission("fired")],
                        Some(&Rearm { trigger: "slot".into(), delay_ms: 1 }),
                        0,
                    )
                    .map(|_| ()),
                "mailbox" => s.deliver(&event("early", "early", json!(1)), Some("p"), 0).map(|_| ()),
                "expire" => s.sweep(MAILBOX_KEPT_MS, RECORDS_KEPT_MS).map(|_| ()),
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
                    )
                    .map(|_| ()),
                "trip" => s
                    .finish(tripping.as_ref().unwrap(), &Status::Faulted(WorkflowError::runtime("fault")), 0, 1)
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
                    .map(|_| ()),
                "acknowledge" => s.acknowledge(notice.as_ref().unwrap()).map(|_| ()),
                _ => unreachable!(),
            };
            t.faults.clear();
            let after = t.backend.snapshot()?;
            if result.is_err() && point != CommitFault::AfterCommit {
                assert_eq!(before, after, "partial commit: {operation} at {point:?}");
                report.crash_points += 1;
            }
            if point == CommitFault::AfterCommit {
                assert!(result.is_err());
                assert_ne!(before, after);
                report.crash_points += 1;
            }
            let reopened = (t.reopen)()?;
            audit(&*reopened)?;
            report.operations += 1;
        }
    }
    report.witnesses.insert("rollback at each write and ambiguous committed error");

    // Generated overlapping event histories. Check actual stored runs
    // after EVERY operation, then drain surviving listeners to completion.
    for seed in 1..=24u64 {
        let t = factory("history")?;
        report.cases += 1;
        let mut s = t.store.clone();
        let mut now = 0;
        let mut rng = seed;
        let mut leases = Vec::new();
        for id in ["r0", "r1", "r2"] {
            leases.push(s.start(&admission(id), now)?);
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
                        s.register_wait(f, &types, now)?;
                        s.take_event(f, &k, &types, now)?;
                    }
                }
                2 | 3 => {
                    s.deliver(&event(&format!("step{step}"), &types[0], json!({"step":step})), None, now)?;
                }
                4 => {
                    if let Some(f) = &leases[r] {
                        s.unregister_wait(f, &k, &types, now)?;
                    }
                }
                5 => {
                    s.cancel(&id, now)?;
                    leases[r] = None;
                }
                6 => {
                    s = (t.reopen)()?;
                    now += LEASE_MS + 1;
                    leases = (0..3).map(|r| s.claim(&format!("r{r}"), now)).collect::<anyhow::Result<_>>()?;
                    report.witnesses.insert("generated reopen and lease replacement");
                }
                7 => {
                    s.sweep(now, 0)?;
                }
                8 => {
                    let before = t.backend.snapshot()?;
                    t.faults.arm(CommitFault::BeforeCommit);
                    assert!(s.deliver(&event(&format!("failed{step}"), &types[0], json!(step)), None, now).is_err());
                    t.faults.clear();
                    assert_eq!(t.backend.snapshot()?, before);
                }
                _ => unreachable!(),
            }
            audit(&*s)?;
            report.operations += 1;
        }
        for f in leases.iter().flatten() {
            let listeners = s.inspect(&f.run_id)?.unwrap().listeners;
            for (key, types) in listeners {
                let mut taken = false;
                for attempt in 0..3 {
                    for t in &types {
                        let id = format!("drain:{}:{key}:{t}:{attempt}", f.run_id);
                        s.deliver(&event(&id, t, json!({"drain":true})), None, now)?;
                    }
                    taken = s.take_event(f, &key, &types, now)?.is_some();
                    audit(&*s)?;
                    report.operations += 1;
                    if taken {
                        break;
                    }
                }
                assert!(taken, "listener stranded after finite history");
            }
            s.finish(f, &Status::Completed(json!({})), now, 3)?;
        }
        assert!(s.due(i64::MAX, 100)?.is_empty());
        audit(&*s)?;
    }
    report.witnesses.insert("finite histories drain every live listener");
    assert_eq!(report.witnesses.len(), 26);
    let t = factory("concurrent-claims")?;
    report.cases += 1;
    t.store.admit(&admission("r"), 0)?;
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let reopen = t.reopen.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || -> anyhow::Result<bool> {
                let store = reopen()?;
                barrier.wait();
                Ok(store.claim("r", 0)?.is_some())
            })
        })
        .collect();
    let mut winners = 0;
    for worker in workers {
        winners += usize::from(worker.join().expect("claim worker panicked")?);
    }
    assert_eq!(winners, 1);
    audit(&*t.store)?;
    report.witnesses.insert("concurrent claims have one winner");
    let t = factory("corruption")?;
    report.cases += 1;
    t.store.start(&admission("r"), 0)?;
    let before = t.backend.snapshot()?;
    let mut corrupt = before.rows.clone();
    let bad = (crate::engine::records_of("r"), "bad".to_string());
    corrupt.insert(bad.clone(), json!("invalid record"));
    assert!(t.backend.compare_exchange(&before, &corrupt)?);
    let reopened = (t.reopen)()?;
    let fence = reopened.claim("r", LEASE_MS + 1)?.unwrap();
    assert!(reopened.load_records(&fence, LEASE_MS + 1).is_err());
    assert!(reopened.save_record(&fence, "bad", &Record::Timer { deadline_ms: 1 }, LEASE_MS + 1).is_err());
    assert!(reopened.cancel("r", LEASE_MS + 1)?);
    assert_eq!(reopened.get("r")?.unwrap().outcome, Some(Status::Cancelled));
    assert_eq!(t.backend.snapshot()?.rows[&bad], corrupt[&bad]);
    report.witnesses.insert("corruption fails closed without erasure");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build()?;
    for mode in 0..4 {
        let t = factory("listen-baseline")?;
        let saves = rt.block_on(listen_case(t, mode, usize::MAX, false, false))?.0;
        report.cases += 1;
        for after in [false, true] {
            for fail in 0..saves {
                let t = factory("listen-checkpoint")?;
                let (_, interrupted) = rt.block_on(listen_case(t, mode, fail, after, false))?;
                assert!(interrupted, "checkpoint was not reached: {mode}/{fail}/{after}");
                report.execution_crashes += 1;
                report.cases += 1;
            }
        }
        if mode < 2 {
            let t = factory("listen-atomic-take")?;
            assert!(rt.block_on(listen_case(t, mode, usize::MAX, false, true))?.1);
            report.execution_crashes += 1;
            report.cases += 1;
        }
    }
    report
        .witnesses
        .extend(["listen recovery before and after every interpreter commit", "held delivery and catchable timeout"]);
    budgets(&factory, &mut report)?;
    overlap_policies(&factory, &mut report)?;
    Ok(report)
}

/// The persisted budget counter of `root`'s tree.
fn counter(t: &TestStore, root: &str) -> anyhow::Result<Option<Value>> {
    Ok(t.backend.snapshot()?.rows.get(&("budget".to_string(), root.to_string())).cloned())
}

/// A run tree's shared budget (issue #8): one counter per root, spent by
/// the root and its owned descendants in the commit that saves each
/// decision, never reset by a restart, refused past the limit, and gone
/// with the root.
fn budgets(factory: &impl Fn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    // A refusal names the run's first refused key, decided once.
    let budget =
        |granted: bool, spent: u64| Record::Budget { granted, spent, first: (!granted).then(|| "c#jump".to_string()) };
    let t = factory("budget")?;
    report.cases += 1;
    let s = &t.store;
    let f = s.start(&admission("root"), 0)?.unwrap();
    assert_eq!(s.spend(&f, "a#jump", 2, 1)?, budget(true, 1));
    assert_eq!(s.spend(&f, "a#jump", 2, 1)?, budget(true, 1), "repeating returns the saved decision");
    assert_eq!(s.spend(&f, "b#jump", 2, 1)?, budget(true, 2));
    assert_eq!(s.spend(&f, "c#jump", 2, 1)?, budget(false, 2));
    assert_eq!(s.spend(&f, "c2#jump", 2, 1)?, budget(false, 2), "a later refusal names the first");
    assert_eq!(s.load_records(&f, 1)?.get(owf::durable::EXCEEDED), Some(&Record::Exceeded { by: "c#jump".into() }));
    assert!(s.save_record(&f, owf::durable::EXCEEDED, &Record::Exceeded { by: "a#jump".into() }, 1).is_err());
    assert_eq!(counter(&t, "root")?, Some(json!(2)));
    // Only `spend` commits a decision: a forged one is refused.
    assert!(s.save_record(&f, "d#jump", &budget(true, 0), 1).is_err());
    assert!(s.save_record(&f, "a#jump", &budget(true, 1), 1).is_ok(), "saving the same decision is a no-op");
    // A failed commit changes neither the record nor the counter.
    let before = t.backend.snapshot()?;
    t.faults.arm(CommitFault::BeforeCommit);
    assert!(s.spend(&f, "e#jump", 5, 1).is_err());
    t.faults.clear();
    assert_eq!(t.backend.snapshot()?, before);
    // A restart keeps the count: another worker resumes the run after the
    // lease ended; a stale owner cannot spend.
    let reopened = (t.reopen)()?;
    let now = LEASE_MS + 2;
    let g = reopened.claim("root", now)?.unwrap();
    assert!(s.spend(&f, "e#jump", 5, now).is_err(), "a replaced owner cannot spend");
    assert_eq!(reopened.load_records(&g, now)?.get("b#jump"), Some(&budget(true, 2)));
    assert_eq!(reopened.spend(&g, "c#jump", 5, now)?, budget(false, 2), "a refusal stays refused");
    assert_eq!(reopened.spend(&g, "e#jump", 3, now)?, budget(true, 3), "the count resumes, not restarts");
    report.operations += 11;
    report.witnesses.insert("budget decisions survive restarts and failed commits");

    // Parent, child and grandchild spend the root's one counter.
    let child = |id: &str, parent: &str| {
        (admission(id), ChildLink { parent: parent.into(), key: id.into(), reference: format!("/{id}") })
    };
    let (a, link) = child("child", "root");
    let c = reopened.start_child(&g, &a, &link, now)?.unwrap();
    assert_eq!(reopened.get("child")?.unwrap().root.as_deref(), Some("root"));
    let (a, link) = child("grandchild", "child");
    let gc = reopened.start_child(&c, &a, &link, now)?.unwrap();
    assert_eq!(reopened.get("grandchild")?.unwrap().root.as_deref(), Some("root"));
    assert_eq!(reopened.spend(&c, "x#jump", 5, now)?, budget(true, 4));
    assert_eq!(reopened.spend(&gc, "x#jump", 5, now)?, budget(true, 5));
    assert_eq!(reopened.spend(&g, "f#jump", 5, now)?, budget(false, 5), "the children spent the root's budget");
    assert_eq!(reopened.spend(&c, "y#jump", 9, now)?, budget(true, 6), "each spender passes its own limit");
    assert_eq!(counter(&t, "root")?, Some(json!(6)));
    assert_eq!(counter(&t, "child")?, None, "a child keeps no counter of its own");
    // A child's unit and the root's count are one commit.
    let before = t.backend.snapshot()?;
    t.faults.arm(CommitFault::BeforeCommit);
    assert!(reopened.spend(&gc, "z#jump", 9, now).is_err());
    t.faults.clear();
    assert_eq!(t.backend.snapshot()?, before);
    // Concurrent spenders of one tree at its last unit: exactly one wins.
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = [(g.clone(), "race-root"), (c.clone(), "race-child"), (gc.clone(), "race-grandchild")]
        .into_iter()
        .map(|(fence, key)| {
            let reopen = t.reopen.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || -> anyhow::Result<bool> {
                let store = reopen()?;
                barrier.wait();
                Ok(matches!(store.spend(&fence, key, 7, now)?, Record::Budget { granted: true, .. }))
            })
        })
        .collect();
    let mut granted = 0;
    for worker in workers {
        granted += usize::from(worker.join().expect("spend worker panicked")?);
    }
    assert_eq!(granted, 1);
    assert_eq!(counter(&t, "root")?, Some(json!(7)));
    // The counter ends with the root, whose descendants end with it.
    reopened.finish(&g, &Status::Completed(json!(0)), now, 3)?;
    assert_eq!(counter(&t, "root")?, None);
    assert!(reopened.spend(&c, "late#jump", 99, now).is_err(), "a cancelled child cannot spend");
    audit(&*reopened)?;
    report.operations += 12;
    report.witnesses.insert("a run tree spends one budget, atomically");
    Ok(())
}

/// The settled decisions of `docs/durable-invariants.md` §8 and issue #3:
/// event identity, the mailbox, ingress tombstones, activation epochs and the
/// breaker, exclusive firing, the clock skew bound and compatibility.
fn decisions(factory: &impl Fn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    let conflict = |e: anyhow::Error| e.is::<IdentityConflict>();

    // Event identity: required, deduplicated per recipient, conflicts refused.
    let t = factory("identity")?;
    report.cases += 1;
    let s = &t.store;
    let r = s.start(&admission("r"), 0)?.unwrap();
    assert!(s.deliver(&json!({"type":"answer","data":1}), Some("r"), 0).is_err(), "an identity is required");
    let first = event("1", "answer", json!(1));
    assert_eq!(s.deliver(&first, Some("r"), 0)?, Some("r".into()));
    assert_eq!(s.deliver(&first, Some("r"), 1)?, Some("r".into()), "accepting it again is idempotent");
    let mut timed = first.clone();
    timed["time"] = json!("1970-01-01T00:00:00.002Z");
    assert_eq!(s.deliver(&timed, Some("r"), 2)?, Some("r".into()), "a filled-in time is not the payload");
    assert!(conflict(s.deliver(&event("1", "answer", json!(2)), Some("r"), 2).unwrap_err()));
    assert!(conflict(s.publish(&event("1", "answer", json!(2)), Some("r"), 2).unwrap_err()));
    assert_eq!(s.take_event(&r, "k", &["answer".into()], 3)?.unwrap()["data"], 1);
    assert_eq!(s.deliver(&first, Some("r"), 4)?, Some("r".into()), "still a duplicate once consumed");
    assert_eq!(s.inspect("r")?.unwrap().events.len(), 1);
    // A routed redelivery returns to the run that accepted it first.
    let q = s.start(&admission("q"), 0)?.unwrap();
    s.register_wait(&q, &["routed".into()], 5)?;
    let routed = event("2", "routed", json!(2));
    assert_eq!(s.deliver(&routed, None, 6)?, Some("q".into()));
    s.register_wait(&r, &["routed".into()], 7)?;
    s.take_event(&q, "taken", &["routed".into()], 8)?.unwrap();
    assert_eq!(s.deliver(&routed, None, 9)?, Some("q".into()), "not the next waiting run");
    assert!(conflict(s.deliver(&event("2", "routed", json!(3)), None, 9).unwrap_err()));
    assert!(s.inspect("r")?.unwrap().events.iter().all(|e| e.type_ != "routed"));
    s.finish(&r, &Status::Completed(json!(0)), 10, 3)?;
    assert_eq!(s.deliver(&event("3", "answer", json!(3)), Some("r"), 10)?, None, "a terminal run accepts nothing");
    assert_eq!(s.deliver(&first, Some("r"), 10)?, Some("r".into()), "but keeps its identities");
    audit(&**s)?;
    report.operations += 18;
    report.witnesses.insert("event identity deduplicates per recipient and refuses conflicts");

    // The mailbox: kept before any listen, read first, FIFO per type, and an
    // observable expiry at its configured lifetime.
    let t = factory("mailbox")?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { mailbox_ms: 100, ..Policy::default() });
    let r = s.start(&admission("r"), 0)?.unwrap();
    for (id, t, n) in [("a1", "a", 1), ("b1", "b", 2), ("a2", "a", 3)] {
        assert_eq!(s.deliver(&event(id, t, json!(n)), Some("r"), 0)?, Some("r".into()), "kept without a listener");
    }
    assert_eq!(s.publish(&event("a3", "a", json!(4)), Some("r"), 0)?, vec!["r".to_string()]);
    for (k, t, n) in [("k1", "a", 1), ("k2", "a", 3), ("k3", "b", 2)] {
        assert_eq!(s.take_event(&r, k, &[t.into()], 1)?.unwrap()["data"], n);
    }
    assert!(s.take_event(&r, "wild", &["*".into()], 1)?.is_some(), "an addressed publish is still published");
    s.deliver(&event("late", "a", json!(5)), Some("r"), 1)?;
    s.finish(&r, &Status::Waiting { wake_at_ms: 101 }, 1, 3)?;
    let r = s.claim("r", 101)?.unwrap();
    assert!(s.take_event(&r, "k4", &["a".into()], 101)?.is_none(), "an expired copy is never taken");
    let snapshot = s.inspect("r")?.unwrap();
    let late = snapshot.events.iter().find(|e| e.data == json!(5)).unwrap();
    let key = late.consumed_key.clone().unwrap();
    assert!(matches!(&snapshot.records[&key], Record::Expired { event } if event["id"] == "late"));
    // An unfinished run that never listens: `sweep` expires it, observably.
    s.admit(&admission("idle"), 0)?;
    s.deliver(&event("idle", "a", json!(6)), Some("idle"), 0)?;
    assert_eq!(s.sweep(99, RECORDS_KEPT_MS)?.expired, 0);
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS)?.expired, 1);
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS)?.expired, 0);
    assert_eq!(s.inspect("idle")?.unwrap().events[0].consumed_key.as_deref(), Some("mailbox#expired[0]"));
    assert_eq!(s.deliver(&event("idle", "a", json!(6)), Some("idle"), 200)?, Some("idle".into()));
    assert_eq!(s.inspect("idle")?.unwrap().events.len(), 1, "identity outlives expiry");
    // Broadcast publish is unchanged: subscribers only, no mailbox time.
    s.admit(&admission("unsubscribed"), 0)?;
    assert!(s.publish(&event("b", "a", json!(7)), None, 1)?.is_empty());
    audit(&s)?;
    report.operations += 22;
    report.witnesses.insert("mailbox keeps, orders and observably expires addressed events");

    // Ingress: one event is taken by at most one trigger until its tombstone ends.
    let t = factory("ingress")?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { ingress_ms: 100, ..Policy::default() });
    let on = Trigger { at_ms: None, types: vec!["order".into()], state: json!({ "events": [] }) };
    for k in ["a", "b"] {
        assert!(s.swap_trigger(k, None, Some(&on), &[], None, 0)?);
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
    assert!(!s.ingested(&order, 0)?);
    assert_eq!(s.fire(&firing("a", &one), 0)?, Fired::Committed);
    assert!(s.ingested(&order, 1)?);
    assert_eq!(s.fire(&firing("b", &two), 1)?, Fired::Ingested);
    assert!(s.get("by-b")?.is_none(), "one event starts at most one run");
    assert!(conflict(s.ingested(&event("1", "order", json!(2)), 1).unwrap_err()));
    assert!(!s.ingested(&order, 100)?, "the tombstone has its own lifetime");
    assert_eq!(s.sweep(100, RECORDS_KEPT_MS)?.tombstones, 1);
    assert_eq!(s.fire(&firing("b", &two), 100)?, Fired::Committed);
    report.operations += 10;
    report.witnesses.insert("ingress identities start one run per tombstone lifetime");

    // Epochs: only an activation starts one, arming atomically. The breaker
    // trips in the terminal commit and reports; it never picks a version.
    let t = factory("epochs")?;
    report.cases += 1;
    let s = &t.store;
    let one = s.save_definition("w", "one")?;
    let two = s.save_definition("w", "two")?;
    let trigger = |n: i64| Trigger { at_ms: Some(n), types: vec![], state: json!({ "n": n }) };
    let arming = |triggers: &[(&str, i64)]| Arming {
        triggers: triggers.iter().map(|(k, n)| ((*k).into(), trigger(*n))).collect(),
    };
    let d = s.activate_with("w", one.version, 0, &arming(&[("w#every", 10)]))?.unwrap();
    assert_eq!((d.epoch, d.triggers.clone()), (1, vec!["w#every".to_string()]));
    assert!(s.activate_with("w", two.version, 0, &arming(&[]))?.is_none(), "a stale epoch is refused");
    let d = s.activate_with("w", two.version, 1, &arming(&[("w#after", 20)]))?.unwrap();
    assert_eq!(d.epoch, 2);
    assert!(s.trigger("w#every")?.is_none() && s.trigger("w#after")?.is_some(), "replaced in one commit");
    let run_of = |id: &str, version: u64| {
        let mut a = admission(id);
        a.name = "w".into();
        a.version = version;
        a
    };
    let old = s.start(&run_of("before", two.version), 0)?.unwrap();
    assert_eq!(s.get("before")?.unwrap().epoch, Some(2));
    let late = s.start(&run_of("late", two.version), 0)?.unwrap();
    let late_success = s.start(&run_of("late-success", two.version), 0)?.unwrap();
    // Cancellation and running out of interruptions do not count.
    let cancelled = s.start(&run_of("cancelled", two.version), 0)?.unwrap();
    s.finish(&cancelled, &Status::Cancelled, 0, 2)?;
    let mut exhausted = s.start(&run_of("exhausted", two.version), 0)?.unwrap();
    for n in 1..=MAX_INTERRUPTIONS {
        let now = s.get("exhausted")?.unwrap().wake_at_ms;
        let status = s.finish(&exhausted, &Status::Interrupted(WorkflowError::runtime("storage")), now, 2)?;
        if n < MAX_INTERRUPTIONS {
            exhausted = s.claim("exhausted", s.get("exhausted")?.unwrap().wake_at_ms)?.unwrap();
        } else {
            assert!(matches!(status, Status::Faulted(_)));
        }
    }
    assert_eq!(s.definitions("w")?.consecutive_faults, 0);
    assert!(s.notices(10)?.is_empty());
    for n in 0..2 {
        let f = s.start(&run_of(&format!("fault{n}"), two.version), 0)?.unwrap();
        s.finish(&f, &Status::Faulted(WorkflowError::runtime(format!("fault {n}"))), 5, 2)?;
    }
    let d = s.definitions("w")?;
    assert_eq!((d.active, d.epoch, d.consecutive_faults), (Some(two.version), 2, 2), "no rollback, no new epoch");
    let trip = Trip {
        version: two.version,
        epoch: 2,
        faults: 2,
        threshold: 2,
        run: "fault1".into(),
        reason: WorkflowError::runtime("fault 1"),
        at_ms: 5,
    };
    assert_eq!(d.tripped.as_ref(), Some(&trip), "the reason and the counts, visible in the runs API");
    assert!(d.triggers.is_empty() && s.trigger("w#after")?.is_none(), "the schedule stops in the trip's commit");
    // The trip's notice is committed with it: an identified CloudEvent.
    let notices = s.notices(10)?;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1, trip.event("w"));
    assert_eq!((notices[0].1["type"].as_str(), notices[0].1["id"].as_str()), (Some(TRIPPED), Some("tripped:2")));
    // Tripped, no new run is admitted, by any path and of any version, with
    // a typed refusal in the admission's commit; an existing run is joined.
    let tripped = |e: anyhow::Error| e.downcast_ref::<DefinitionTripped>().map(|t| (t.name.clone(), t.epoch));
    let expected = Some(("w".to_string(), 2));
    assert_eq!(tripped(s.admit(&run_of("refused", two.version), 6).unwrap_err()), expected);
    assert_eq!(tripped(s.start(&run_of("refused", one.version), 6).unwrap_err()), expected);
    let parent = s.start(&admission("parent"), 6)?.unwrap();
    let link = ChildLink { parent: "parent".into(), key: "child".into(), reference: "/child".into() };
    let before = s.inspect("parent")?;
    assert_eq!(tripped(s.start_child(&parent, &run_of("refused", two.version), &link, 6).unwrap_err()), expected);
    assert_eq!(s.inspect("parent")?, before, "the parent's side of a refused child is not committed");
    let fired = [run_of("refused", two.version)];
    let firing = Firing { key: "generic", next: Some(&trigger(50)), admit: &fired, ..Firing::default() };
    assert_eq!(tripped(s.fire(&firing, 6).unwrap_err()), expected);
    assert!(s.get("refused")?.is_none() && s.trigger("generic")?.is_none());
    assert!(!s.admit(&run_of("late", two.version), 6)?, "an admitted run is joined, not refused");
    s.finish(&parent, &Status::Completed(json!(0)), 6, 2)?;
    // Tripped, the breaker observes nothing more until an activation.
    s.finish(&late, &Status::Faulted(WorkflowError::runtime("late")), 6, 2)?;
    s.finish(&late_success, &Status::Completed(json!(0)), 6, 2)?;
    let d = s.definitions("w")?;
    assert_eq!((d.consecutive_faults, d.tripped.as_ref()), (2, Some(&trip)));
    assert_eq!(s.notices(10)?.len(), 1, "one trip, one notice");
    // Only an activation clears it, in a new epoch, with the version the
    // host chooses (here the earlier one). The notice stays until handed on.
    let d = s.activate_with("w", one.version, 2, &arming(&[("w#every", 40)]))?.unwrap();
    assert!(d.tripped.is_none() && d.epoch == 3 && d.consecutive_faults == 0, "a new activation clears it");
    assert_eq!(s.trigger("w#every")?, Some(trigger(40)));
    assert_eq!(s.notices(10)?, notices);
    assert!(s.acknowledge(&notices[0].0)?);
    assert!(!s.acknowledge(&notices[0].0)?, "already acknowledged");
    assert!(s.notices(10)?.is_empty());
    // An earlier epoch's fault cannot touch the new one.
    s.finish(&old, &Status::Faulted(WorkflowError::runtime("late")), 0, 2)?;
    assert_eq!(s.definitions("w")?.consecutive_faults, 0);
    // A success resets the count.
    let f = s.start(&run_of("fault", one.version), 0)?.unwrap();
    s.finish(&f, &Status::Faulted(WorkflowError::runtime("fault")), 0, 2)?;
    let f = s.start(&run_of("success", one.version), 0)?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 0, 2)?;
    assert_eq!(s.definitions("w")?.consecutive_faults, 0);
    audit(&**s)?;
    report.operations += 60;
    report.witnesses.insert("epochs arm atomically and isolate the breaker");
    report.witnesses.insert("a trip reports with a committed notice and only activation clears it");

    // Exclusive firing: a slot whose previous run is unfinished is skipped.
    let t = factory("overlap")?;
    report.cases += 1;
    let s = &t.store;
    let slot = |at: i64, skips: i64| Trigger { at_ms: Some(at), types: vec![], state: json!({ "skips": skips }) };
    let fire = |expected: &Trigger, next: &Trigger, busy: &Trigger, id: &str, now: i64| {
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
    };
    assert!(s.swap_trigger("every", None, Some(&slot(10, 0)), &[], None, 0)?);
    assert_eq!(fire(&slot(10, 0), &slot(20, 0), &slot(20, 1), "slot10", 10)?, Fired::Committed);
    assert_eq!(fire(&slot(20, 0), &slot(30, 0), &slot(30, 1), "slot20", 20)?, Fired::Busy("slot10".into()));
    assert!(s.get("slot20")?.is_none());
    assert_eq!(s.trigger("every")?, Some(slot(30, 1)), "the skip is recorded");
    let f = s.claim("slot10", 21)?.unwrap();
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 22, 3)?;
    assert!(matches!(fire(&slot(30, 1), &slot(40, 1), &slot(40, 2), "slot30", 30)?, Fired::Busy(_)), "waiting");
    s.cancel("slot10", 31)?;
    assert_eq!(fire(&slot(40, 2), &slot(50, 2), &slot(50, 3), "slot40", 40)?, Fired::Committed);
    report.operations += 8;
    report.witnesses.insert("a live previous run makes the next slot an explicit skip");

    // A trigger's run the host refused to admit: the slot is consumed with a
    // run that ended faulted in that commit, counted by the breaker, whose
    // threshold refusal trips it and removes the trigger in the same commit.
    let t = factory("refused")?;
    report.cases += 1;
    let s = &t.store;
    let v = s.save_definition("w", "yaml")?;
    let slot = |at: i64| Trigger { at_ms: Some(at), types: vec![], state: json!({ "at": at }) };
    let arming = Arming { triggers: BTreeMap::from([("w#every".into(), slot(10))]) };
    s.activate_with("w", v.version, 0, &arming)?.unwrap();
    let run_of = |id: &str| Admission { name: "w".into(), version: v.version, ..admission(id) };
    let invalid = WorkflowError::validation("invalid");
    let refused = |id: &str| Refused { admission: run_of(id), error: invalid.clone(), breaker_threshold: 2 };
    let fire = |at: i64, admit: &[Admission], refused: Option<&Refused>| {
        let firing = Firing {
            key: "w#every",
            expected: Some(&slot(at)),
            next: Some(&slot(at + 10)),
            admit,
            refused,
            busy: Some(&slot(at + 10)),
            ..Firing::default()
        };
        s.fire(&firing, at)
    };
    // Overlap applies to a refused slot as to an admitted one.
    assert_eq!(fire(10, &[run_of("live")], None)?, Fired::Committed);
    assert_eq!(fire(20, &[], Some(&refused("slot20")))?, Fired::Busy("live".into()));
    assert!(s.get("slot20")?.is_none());
    s.cancel("live", 25)?;
    assert_eq!(fire(30, &[], Some(&refused("slot30")))?, Fired::Committed);
    let r = s.get("slot30")?.unwrap();
    assert_eq!(
        (r.state, r.outcome.clone(), r.ended_ms, r.epoch),
        (RunState::Failed, Some(Status::Faulted(invalid.clone())), Some(30), Some(1))
    );
    assert_eq!((r.admitted_ms, r.admission), (30, run_of("slot30")), "visible in the runs API as it was refused");
    assert!(s.due(i64::MAX, 10)?.is_empty() && s.claim("slot30", 30)?.is_none(), "never due, never claimed");
    assert_eq!(s.trigger("w#every")?, Some(slot(40)), "the slot is consumed");
    assert_eq!(s.definitions("w")?.consecutive_faults, 1);
    assert_eq!(fire(30, &[], Some(&refused("slot30")))?, Fired::Changed, "a repeated firing changes nothing");
    assert_eq!(s.definitions("w")?.consecutive_faults, 1);
    assert!(
        s.fire(
            &Firing { key: "w#every", admit: &[run_of("a")], refused: Some(&refused("b")), ..Firing::default() },
            40
        )
        .is_err(),
        "admit or refuse, not both"
    );
    assert!(s.get("a")?.is_none() && s.get("b")?.is_none());
    assert_eq!(fire(40, &[], Some(&refused("slot40")))?, Fired::Committed);
    let d = s.definitions("w")?;
    let trip = d.tripped.expect("the threshold refusal trips");
    assert_eq!((trip.run.as_str(), &trip.reason, trip.faults, trip.threshold), ("slot40", &invalid, 2, 2));
    assert!(d.triggers.is_empty() && s.trigger("w#every")?.is_none(), "its schedule stops in that commit");
    assert_eq!(s.notices(10)?, vec![(s.notices(10)?[0].0.clone(), trip.event("w"))]);
    assert_eq!(fire(40, &[], Some(&refused("slot50")))?, Fired::Changed);
    audit(&**s)?;
    report.operations += 16;
    report.witnesses.insert("a refused trigger run consumes its slot, counts and may trip");

    // The skew bound: an owner stops acting ε before its lease ends, and a
    // lease is taken over only after it ends.
    let t = factory("skew")?;
    report.cases += 1;
    let s = TransactionalStore::new(t.backend.clone()).with_policy(Policy { skew_ms: 100, ..Policy::default() });
    let f = s.start(&admission("r"), 0)?.unwrap();
    s.save_record(&f, "early", &Record::Timer { deadline_ms: 1 }, LEASE_MS - 101)?;
    assert!(s.save_record(&f, "late", &Record::Timer { deadline_ms: 1 }, LEASE_MS - 100).is_err());
    assert!(s.renew(&f, LEASE_MS - 100).is_err());
    assert!(s.claim("r", LEASE_MS - 1)?.is_none());
    assert!(s.claim("r", LEASE_MS)?.is_some());
    assert_eq!(s.get("r")?.unwrap().admitted_ms, 0, "the admission instant survives takeover");
    report.operations += 6;
    report.witnesses.insert("owners stop acting a skew bound before takeover");

    // Compatibility: a declared run goes only to a compatible worker; the
    // rest wait, visibly due.
    let t = factory("compatibility")?;
    report.cases += 1;
    let s = &t.store;
    for (id, version) in [("undeclared", None), ("one", Some("1")), ("two", Some("2"))] {
        let mut a = admission(id);
        if let Some(v) = version {
            a.runtime = json!({ APPLICATION_VERSION: v });
        }
        s.admit(&a, 0)?;
    }
    let two = Compatibility { version: Some("2".into()), accepts: vec![] };
    assert_eq!(s.due_as(0, 10, &two)?, vec!["two".to_string(), "undeclared".into()]);
    assert!(s.claim_as("one", 0, &two)?.is_none());
    assert!(s.due(0, 10)?.contains(&"one".to_string()), "waiting for a compatible worker, not lost");
    assert_eq!(s.due_as(0, 1, &two)?.len(), 1, "one page");
    let upgraded = Compatibility { version: Some("2".into()), accepts: vec!["1".into()] };
    assert_eq!(s.due_as(0, 10, &upgraded)?.len(), 3);
    assert!(s.claim_as("one", 0, &upgraded)?.is_some());
    assert!(s.claim_as("two", 0, &Compatibility::default())?.is_some(), "an undeclared worker claims anything");
    assert!(s.claim_as("undeclared", 0, &two)?.is_some(), "an undeclared run goes to any worker");
    report.operations += 9;
    report.witnesses.insert("only compatible workers claim declared runs");

    // The rearm of an old epoch cannot move its replacement.
    let t = factory("rearm")?;
    report.cases += 1;
    let s = &t.store;
    let dormant = |epoch: i64| Trigger { at_ms: None, types: vec![], state: json!({ "epoch": epoch }) };
    let rearm = Rearm { trigger: "after".into(), delay_ms: 5 };
    assert!(s.swap_trigger("after", None, Some(&dormant(1)), &[admission("first")], Some(&rearm), 0)?);
    let f = s.claim("first", 0)?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 1, 3)?;
    assert_eq!(s.trigger("after")?.and_then(|t| t.at_ms), Some(6), "the same epoch re-arms");
    let due = s.trigger("after")?.unwrap();
    assert!(s.swap_trigger("after", Some(&due), Some(&dormant(1)), &[admission("second")], Some(&rearm), 6)?);
    assert!(s.swap_trigger("after", Some(&dormant(1)), Some(&dormant(2)), &[], None, 7)?);
    let f = s.claim("second", 7)?.unwrap();
    s.finish(&f, &Status::Completed(json!(0)), 8, 3)?;
    assert_eq!(s.trigger("after")?, Some(dormant(2)), "a replaced trigger is not re-armed");
    report.operations += 8;
    report.witnesses.insert("an old epoch's completion cannot rearm its replacement");
    Ok(())
}

// Interpreter-level recovery, ported from the checkpoint/hold properties.
// Every commit boundary is visited explicitly, rather than relying on random
// histories to reach it. Only the clock is virtual; database writes are real.
struct LeaseTime(std::sync::atomic::AtomicI64);
impl Clock for LeaseTime {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
struct WorkflowTime(tokio::time::Instant);
impl owf::run::Clock for WorkflowTime {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(1000 + i64::try_from(self.0.elapsed().as_millis()).unwrap()).unwrap()
    }
}
struct Inbox {
    lease: Arc<Lease>,
    saves: std::sync::atomic::AtomicUsize,
    fail: usize,
    after_save: bool,
    after_take: bool,
}
impl Journal for Inbox {
    fn load(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.lease.load()
    }
    fn save(&self, key: &str, record: &Record) -> anyhow::Result<()> {
        let n = self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(self.after_save || n != self.fail, "crash before checkpoint {n}");
        self.lease.save(key, record)?;
        anyhow::ensure!(!self.after_save || n != self.fail, "crash after checkpoint {n}");
        Ok(())
    }
}
impl Events for Inbox {
    fn notification(&self, id: &str) -> futures::future::BoxFuture<'_, ()> {
        self.lease.notification(id)
    }
    fn register(&self, id: &str, types: &[String]) -> anyhow::Result<()> {
        self.lease.register(id, types)
    }
    fn unregister(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<()> {
        self.lease.unregister(id, key, types)
    }
    fn take(&self, id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>> {
        let event = self.lease.take(id, key, types)?;
        anyhow::ensure!(!self.after_take || event.is_none(), "crash after atomic event consumption");
        Ok(event)
    }
}
#[async_trait::async_trait]
impl owf::run::Functions for Inbox {
    fn has(&self, _: &str) -> bool {
        false
    }
    fn events(&self) -> Option<&dyn Events> {
        Some(self)
    }
    async fn call(&self, _: &str, _: Value, _: &owf::run::CallInfo<'_>) -> Result<Value, WorkflowError> {
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
    use owf::run::{self, Host, Limits, Silent};
    use std::{
        sync::atomic::{AtomicI64, AtomicUsize, Ordering},
        task::Poll,
        time::Duration,
    };
    let time = Arc::new(WorkflowTime(tokio::time::Instant::now()));
    let lease_time = Arc::new(LeaseTime(AtomicI64::new(1000)));
    let fence = t.store.start(&admission("r"), 1000)?.unwrap();
    let lease = Arc::new(Lease { store: t.store.clone(), fence, clock: lease_time.clone() });
    let inbox = Arc::new(Inbox { lease: lease.clone(), saves: AtomicUsize::new(0), fail, after_save, after_take });
    let timeout = if mode == 3 { 5 } else { 50 };
    let definition = format!(
        "document: {{ dsl: '1.0.4', namespace: test, name: listen, version: '1.0.0' }}\ndo:\n - attempt:\n     try:\n       - listen:\n           listen: {{ to: {{ one: {{ with: {{ type: answer }} }} }}, read: envelope }}\n           metadata: {{ hold: PT0.010S }}\n           timeout: {{ after: PT0.{timeout:03}S }}\n     catch:\n       errors: {{ with: {{ status: 408 }} }}\n       do: [ {{ expired: {{ set: {{ expired: true }} }} }} ]\n"
    );
    let wf = owf::model::parse_yaml(&definition)?;
    let host = Host {
        functions: inbox.clone(),
        observer: Arc::new(Silent),
        clock: time.clone(),
        limits: Limits::default(),
        runtime: json!({}),
    };
    let token = tokio_util::sync::CancellationToken::new();
    if mode == 0 {
        lease.register("r", &["answer".into()])?;
        t.store.deliver(&event("first", "answer", json!("answer")), None, 1000)?;
    }
    let mut future = Box::pin(run::resume(&host, &wf, "r", json!({}), &token, &*inbox));
    let first = match futures::poll!(future.as_mut()) {
        Poll::Ready(outcome) => outcome.status,
        Poll::Pending => {
            if mode == 1 {
                tokio::time::advance(Duration::from_millis(2)).await;
                t.store.deliver(&event("first", "answer", json!("answer")), None, 1000)?;
            } else {
                tokio::time::advance(Duration::from_millis(10)).await;
            }
            future.await.status
        }
    };
    let saves = inbox.saves.load(Ordering::SeqCst);
    let interrupted = matches!(first, Status::Interrupted(_));
    audit(&*t.store)?;
    if !interrupted {
        t.store.finish(&lease.fence, &first, 1000, 3)?;
    }
    let reopened = (t.reopen)()?;
    if reopened.get("r")?.unwrap().outcome.is_none() {
        let now = LEASE_MS + 1001;
        lease_time.0.store(now, Ordering::SeqCst);
        let fence = reopened.claim("r", now)?.unwrap();
        assert!(t.store.renew(&lease.fence, now).is_err());
        let current = Arc::new(Lease { store: reopened.clone(), fence, clock: lease_time });
        let next = Arc::new(Inbox {
            lease: current.clone(),
            saves: AtomicUsize::new(0),
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
            current.register("r", &["answer".into()])?;
            reopened.deliver(&event("recovery", "answer", json!("answer")), None, now)?;
        } else {
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        let mut status = run::resume(&host, &wf, "r", json!({}), &token, &*next).await.status;
        if mode == 3 && matches!(status, Status::Waiting { .. }) {
            tokio::time::advance(Duration::from_millis(100)).await;
            status = run::resume(&host, &wf, "r", json!({}), &token, &*next).await.status;
        }
        assert!(matches!(status, Status::Completed(_)), "recovery stalled: {mode}/{fail}/{after_save}: {status:?}");
        reopened.finish(&current.fence, &status, now, 3)?;
    }
    let outcome = reopened.get("r")?.unwrap().outcome.unwrap();
    if mode == 3 {
        assert_eq!(outcome, Status::Completed(json!({"expired":true})));
    } else {
        let Status::Completed(v) = outcome else { panic!("{outcome:?}") };
        assert_eq!(v[0]["data"], "answer");
    }
    audit(&*reopened)?;
    assert_eq!(
        reopened.inspect("r")?.unwrap().events.iter().filter(|e| e.consumed_key.is_some()).count(),
        usize::from(mode != 3)
    );
    Ok((saves, interrupted))
}

/// Policy decisions, ingress consumption, cancellation and admission are one
/// transaction. Reopening after an ambiguous reply observes that transaction.
fn overlap_policies(factory: &impl Fn(&str) -> anyhow::Result<TestStore>, report: &mut Report) -> anyhow::Result<()> {
    let slot = |n| Trigger { at_ms: Some(n), types: vec!["tick".into()], state: json!(n) };
    let fire = |s: &dyn RunStore, policy, n, max, ingress: Option<&Value>| {
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
    };
    for policy in [Overlap::Skip, Overlap::Allow, Overlap::Queue, Overlap::Replace] {
        let t = factory("overlap-policy")?;
        report.cases += 1;
        let s = &t.store;
        s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0)?;
        assert_eq!(fire(&**s, policy, 1, 10, None)?, Fired::Committed);
        let f = s.claim("slot1", 1)?.unwrap();
        let c = admission("owned");
        let link = ChildLink { parent: "slot1".into(), key: "child".into(), reference: "/child".into() };
        let child = s.start_child(&f, &c, &link, 1)?.unwrap();
        s.save_record(
            &child,
            "protected",
            &Record::Call {
                logical: "call".into(),
                function: "f".into(),
                arguments: json!({}),
                delivery: owf::durable::Delivery::AtMostOnce,
                state: owf::durable::CallState::Pending,
            },
            1,
        )?;
        s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 1, 3)?;
        let e = event("second", "tick", json!(2));
        let answer = fire(&**s, policy, 2, 10, Some(&e))?;
        assert_eq!(
            answer,
            match policy {
                Overlap::Skip => Fired::Busy("slot1".into()),
                Overlap::Queue => Fired::Queued,
                _ => Fired::Committed,
            }
        );
        let s = (t.reopen)()?;
        assert!(s.ingested(&e, 2)?, "even a skipped event is consumed");
        assert_eq!(
            s.fire(&Firing { key: "policy", expected: Some(&slot(2)), next: Some(&slot(3)), ..Firing::default() }, 2)?,
            Fired::Changed
        );
        if policy == Overlap::Queue {
            assert_eq!(s.queued("policy")?, Some(admission("slot2")));
            assert!(s.get("slot2")?.is_none(), "a buffer is not an admitted run");
            assert_eq!(fire(&*s, policy, 3, 10, None)?, Fired::Busy("slot1".into()));
            assert!(s.get("slot3")?.is_none(), "only one buffered slot");
        }
        if policy == Overlap::Replace {
            assert_eq!(s.get("slot1")?.unwrap().outcome, Some(Status::Cancelled));
            assert_eq!(s.get("owned")?.unwrap().outcome, Some(Status::Cancelled));
            assert!(s.save_record(&child, "late", &Record::Timer { deadline_ms: 0 }, 3).is_err());
            assert!(matches!(
                s.inspect("owned")?.unwrap().records["protected"],
                Record::Call { state: owf::durable::CallState::Pending, .. }
            ));
        } else {
            s.cancel("slot1", 4)?;
        }
        assert!(s.queued("policy")?.is_none());
        assert_eq!(s.get("slot2")?.is_some(), policy != Overlap::Skip);
        if policy == Overlap::Queue {
            assert_eq!(s.get("slot2")?.unwrap().admission, admission("slot2"));
            assert_eq!(s.get("slot2")?.unwrap().admitted_ms, 4, "timeout starts at promotion");
        }
        audit(&*s)?;
        report.operations += 18;
        // Fail after the overlap decision at different write boundaries. A
        // rollback keeps every row; an ambiguous commit keeps all new rows.
        let points = (0..8).map(CommitFault::BeforeWrite).chain([CommitFault::BeforeCommit, CommitFault::AfterCommit]);
        for point in points {
            let t = factory("overlap-crash")?;
            report.cases += 1;
            let s = &t.store;
            s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0)?;
            fire(&**s, policy, 1, 10, None)?;
            let before = t.backend.snapshot()?;
            t.faults.arm(point);
            let result = fire(&**s, policy, 2, 10, Some(&e));
            t.faults.clear();
            let reopened = (t.reopen)()?;
            if result.is_err() && point != CommitFault::AfterCommit {
                assert_eq!(t.backend.snapshot()?, before, "{policy:?} {point:?}");
                assert_eq!(reopened.trigger("policy")?, Some(slot(2)));
            } else {
                assert_eq!(reopened.trigger("policy")?, Some(slot(3)));
                assert!(reopened.ingested(&e, 2)?);
                assert_eq!(reopened.queued("policy")?.is_some(), policy == Overlap::Queue);
                assert_eq!(reopened.get("slot2")?.is_some(), matches!(policy, Overlap::Allow | Overlap::Replace));
                assert_eq!(reopened.get("slot1")?.unwrap().ended_ms.is_some(), policy == Overlap::Replace);
            }
            audit(&*reopened)?;
            report.crash_points += 1;
        }
    }
    // A different trigger and manually admitted runs share the definition's
    // ceiling. Ended runs do not count, and cancellation releases capacity.
    let t = factory("overlap-ceiling")?;
    report.cases += 1;
    let s = &t.store;
    s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0)?;
    for n in 0..10 {
        s.admit(&admission(&format!("manual{n}")), 0)?;
    }
    assert_eq!(fire(&**s, Overlap::Allow, 1, 10, None)?, Fired::Ceiling);
    assert!(s.get("slot1")?.is_none());
    s.cancel("manual0", 1)?;
    assert_eq!(fire(&**s, Overlap::Allow, 2, 10, None)?, Fired::Committed);
    assert_eq!(fire(&**s, Overlap::Allow, 3, 10, None)?, Fired::Ceiling);
    // Promotion has its own crash boundary, atomic with the previous end.
    for point in [CommitFault::BeforeCommit, CommitFault::AfterCommit] {
        let t = factory("overlap-promotion-crash")?;
        report.cases += 1;
        let s = &t.store;
        s.swap_trigger("policy", None, Some(&slot(1)), &[], None, 0)?;
        fire(&**s, Overlap::Queue, 1, 10, None)?;
        fire(&**s, Overlap::Queue, 2, 10, None)?;
        let before = t.backend.snapshot()?;
        t.faults.arm(point);
        assert!(s.cancel("slot1", 3).is_err());
        t.faults.clear();
        let s = (t.reopen)()?;
        if point == CommitFault::BeforeCommit {
            assert_eq!(t.backend.snapshot()?, before);
            s.cancel("slot1", 3)?;
        }
        assert!(s.queued("policy")?.is_none());
        assert_eq!(s.get("slot2")?.unwrap().admission, admission("slot2"));
        assert!(s.get("slot1")?.unwrap().ended_ms.is_some());
        report.crash_points += 1;
    }
    for trip in [false, true] {
        let t = factory("overlap-epoch")?;
        report.cases += 1;
        let s = &t.store;
        let d = s.save_definition("workflow", "original")?;
        let armed = Arming { triggers: BTreeMap::from([("policy".into(), slot(1))]) };
        s.activate_with("workflow", d.version, 0, &armed)?;
        fire(&**s, Overlap::Queue, 1, 10, None)?;
        fire(&**s, Overlap::Queue, 2, 10, None)?;
        assert!(s.queued("policy")?.is_some());
        if trip {
            let f = s.claim("slot1", 2)?.unwrap();
            s.finish(&f, &Status::Faulted(WorkflowError::runtime("trip")), 3, 1)?;
            assert!(s.trigger("policy")?.is_none());
        } else {
            s.activate_with(
                "workflow",
                d.version,
                1,
                &Arming { triggers: BTreeMap::from([("policy".into(), slot(10))]) },
            )?;
            s.cancel("slot1", 3)?;
            assert_eq!(s.trigger("policy")?, Some(slot(10)));
        }
        assert!(s.queued("policy")?.is_none(), "a replaced epoch discards its buffer");
        assert!(s.get("slot2")?.is_none(), "an old buffered slot cannot be resurrected");
    }
    // Independent trigger CAS operations still contend on the definition's
    // capacity range, so several workers cannot overbook its last permits.
    let t = factory("overlap-ceiling-race")?;
    report.cases += 1;
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for n in 0..8 {
        let k = format!("race{n}");
        t.store.swap_trigger(&k, None, Some(&slot(1)), &[], None, 0)?;
        let s = (t.reopen)()?;
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let timer = Trigger { at_ms: Some(1), types: vec!["tick".into()], state: json!(1) };
            barrier.wait();
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
        }));
    }
    let answers = workers.into_iter().map(|w| w.join().unwrap()).collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(answers.iter().filter(|a| **a == Fired::Committed).count(), 2);
    assert_eq!(answers.iter().filter(|a| **a == Fired::Ceiling).count(), 6);
    assert_eq!(t.store.list(None, "race")?.len(), 2);
    let rows = t.backend.snapshot()?.rows;
    assert_eq!(rows.keys().filter(|(kind, _)| kind.starts_with("live_name:")).count(), 2);
    assert_eq!(rows.keys().filter(|(kind, _)| kind.starts_with("trigger_live:")).count(), 2);
    report.witnesses.insert("all overlap policies and queue promotion survive decision/admission crashes");
    Ok(())
}
