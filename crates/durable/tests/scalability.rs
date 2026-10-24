use ouro_durable::{
    Arming, AtomicBackend, Compatibility, Conflict, Fence, Fired, Firing, IoCounts, Metrics, Policy, Rearm, RunState,
    RunStore, TransactionalStore, Trigger,
    conformance::{admission, event},
    memory::MemoryBackend,
    postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use owf::{
    durable::Record,
    run::{Status, WorkflowError},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Barrier},
};
mod support;

fn bounded<T>(
    metrics: &Metrics,
    label: &str,
    matches: u64,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    metrics.reset();
    let result = operation()?;
    let counts = metrics.counts();
    assert!(counts.rows_read <= 32 + 4 * matches, "{label}: {counts:?}");
    assert!(counts.rows_written <= 16 + 4 * matches, "{label}: {counts:?}");
    assert_eq!(counts.conflicts, 0, "single worker should not conflict");
    eprintln!("BOUND,{label},{},{},matches={matches}", counts.rows_read, counts.rows_written);
    Ok(result)
}
fn bounds(backend: Arc<dyn AtomicBackend>, metrics: Arc<Metrics>, validation: u64) -> anyhow::Result<()> {
    support::seed(&backend, 10_000)?;
    decided(&backend, &metrics)?;
    let s = TransactionalStore::new(backend.clone());
    let mut a = admission("target:r");
    a.name = "target".into();
    bounded(&metrics, "save_definition", 0, || s.save_definition("target", "one"))?;
    let d = bounded(&metrics, "save_definition_existing", 0, || s.save_definition("target", "two"))?;
    bounded(&metrics, "activate", 0, || s.activate("target", d.version))?;
    assert_eq!(bounded(&metrics, "definitions", 2, || s.definitions("target"))?.versions.len(), 2);
    a.version = d.version;
    let mut other = a.clone();
    other.id = "target:admitted".into();
    assert!(bounded(&metrics, "admit", 0, || s.admit(&other, 0))?);
    let f = bounded(&metrics, "start", 0, || s.start(&a, 0))?.unwrap();
    assert!(bounded(&metrics, "idempotent_start", 0, || s.start(&a, 0))?.is_none());
    let admitted = bounded(&metrics, "claim", 0, || s.claim(&other.id, 0))?.unwrap();
    bounded(&metrics, "renew", 0, || s.renew(&f, 1))?;
    bounded(&metrics, "save_record", 0, || s.save_record(&f, "timer", &Record::Timer { deadline_ms: 42 }, 1))?;
    // Full pending inboxes must not turn a missing eligible delivery into a
    // scan over every registration of the same event type.
    for n in 0..64 {
        let f = s.start(&admission(&format!("saturated:{n}")), 1)?.unwrap();
        s.register_wait(&f, &["crowded".into()], 1)?;
        s.deliver(&event(&format!("crowded{n}"), "crowded", json!(n)), None, 1)?.unwrap();
        s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 1, 3)?;
    }
    assert!(
        bounded(&metrics, "deliver_saturated", 0, || s.deliver(&event("crowded", "crowded", json!(0)), None, 1))?
            .is_none()
    );
    let types = vec!["answer".into()];
    bounded(&metrics, "register_wait", 0, || s.register_wait(&f, &types, 1))?;
    assert!(bounded(&metrics, "take_event_pending", 0, || s.take_event(&f, "answer", &types, 1))?.is_none());
    let answer = |id: &str| event(id, "answer", json!(42));
    assert_eq!(bounded(&metrics, "deliver", 1, || s.deliver(&answer("1"), None, 1))?, Some(a.id.clone()));
    assert_eq!(bounded(&metrics, "deliver_again", 1, || s.deliver(&answer("1"), None, 1))?, Some(a.id.clone()));
    assert!(bounded(&metrics, "deliver_full", 0, || s.deliver(&answer("2"), None, 1))?.is_none());
    assert!(bounded(&metrics, "deliver_absent", 0, || s.deliver(&event("3", "absent", json!(42)), None, 1))?.is_none());
    assert!(bounded(&metrics, "take_event", 0, || s.take_event(&f, "answer", &types, 1))?.is_some());
    bounded(&metrics, "take_event_replay", 0, || s.take_event(&f, "answer", &types, 1))?;
    bounded(&metrics, "unregister_wait", 0, || s.unregister_wait(&f, "answer", &types, 1))?;
    assert_eq!(bounded(&metrics, "load_records", 2, || s.load_records(&f, 1))?.len(), 2);
    // Published events: thousands of seeded runs subscribe to another type.
    let published = |id: &str, t: &str| json!({ "id": id, "source": "/bounds", "type": t, "data": 1 });
    bounded(&metrics, "subscribe", 0, || s.subscribe(&f, "listen", &["published".into()], 1))?;
    assert_eq!(bounded(&metrics, "publish", 1, || s.publish(&published("1", "published"), None, 1))?.len(), 1);
    assert!(
        bounded(&metrics, "publish_republished", 1, || s.publish(&published("1", "published"), None, 1))?.is_empty()
    );
    assert!(
        bounded(&metrics, "publish_unsubscribed_type", 0, || s.publish(&published("2", "nobody"), None, 1))?.is_empty()
    );
    assert_eq!(
        bounded(&metrics, "publish_target", 1, || s.publish(&published("3", "x"), Some(&other.id), 1))?.len(),
        1
    );
    assert!(
        bounded(&metrics, "take_event_published", 0, || s.take_event(&f, "published", &["*".into()], 1))?.is_some()
    );
    bounded(&metrics, "unsubscribe", 0, || s.unsubscribe(&f, "listen", 1))?;
    // Triggers: many scheduled entries far in the future and of other types.
    for n in 0..1_000 {
        let unrelated = Trigger { at_ms: Some(1_000_000 + n), types: vec!["unrelated".into()], state: json!(n) };
        s.swap_trigger(&format!("unrelated{n:04}"), None, Some(&unrelated), &[], None, 1)?;
    }
    let timer = Trigger { at_ms: Some(5), types: vec!["order".into()], state: json!({ "n": 1 }) };
    assert!(bounded(&metrics, "swap_trigger_create", 0, || s.swap_trigger("slot", None, Some(&timer), &[], None, 1))?);
    assert!(bounded(&metrics, "trigger", 0, || s.trigger("slot"))?.is_some());
    assert_eq!(bounded(&metrics, "due_triggers", 1, || s.due_triggers(5, 16))?.len(), 1);
    assert!(bounded(&metrics, "due_triggers_none", 0, || s.due_triggers(4, 16))?.is_empty());
    assert_eq!(bounded(&metrics, "triggers_for", 1, || s.triggers_for("order", 16))?.len(), 1);
    assert!(bounded(&metrics, "triggers_for_absent", 0, || s.triggers_for("absent", 16))?.is_empty());
    let mut slot_run = a.clone();
    slot_run.id = "target:slot".into();
    let parked = Trigger { at_ms: None, ..timer.clone() };
    let rearm = Rearm { trigger: "slot".into(), delay_ms: 10 };
    assert!(bounded(&metrics, "swap_trigger_fire", 1, || {
        s.swap_trigger("slot", Some(&timer), Some(&parked), &[slot_run.clone()], Some(&rearm), 1)
    })?);
    assert!(!bounded(&metrics, "swap_trigger_lost", 0, || s.swap_trigger("slot", Some(&timer), None, &[], None, 1))?);
    let fired = s.claim("target:slot", 1)?.unwrap();
    bounded(&metrics, "finish_rearm", 1, || s.finish(&fired, &Status::Completed(json!(0)), 2, 3))?;
    assert_eq!(s.trigger("slot")?.and_then(|t| t.at_ms), Some(12));
    bounded(&metrics, "finish_waiting", 0, || s.finish(&f, &Status::Waiting { wake_at_ms: 1 }, 1, 3))?;
    let f = bounded(&metrics, "claim_waiting", 0, || s.claim(&a.id, 1))?.unwrap();
    bounded(&metrics, "release", 0, || s.release(&f, 1))?;
    let f = s.claim(&a.id, 1)?.unwrap();
    assert_eq!(
        bounded(&metrics, "spend", 0, || s.spend(&f, "loop#jump", 100, 1))?,
        Record::Budget { granted: true, spent: 1, first: None }
    );
    bounded(&metrics, "spend_replayed", 0, || s.spend(&f, "loop#jump", 100, 1))?;
    assert!(matches!(
        bounded(&metrics, "spend_refused", 0, || s.spend(&f, "late#jump", 1, 1))?,
        Record::Budget { granted: false, .. }
    ));
    for id in ["target:c", "target:live"] {
        let mut child = a.clone();
        child.id = id.into();
        let link = ouro_durable::ChildLink { parent: a.id.clone(), key: id.into(), reference: "/child".into() };
        let c = bounded(&metrics, "start_child", 1, || s.start_child(&f, &child, &link, 1))?.unwrap();
        assert!(bounded(&metrics, "child_outcome", 0, || s.child_outcome(&a.id, id, id))?.is_none());
        // A child's unit commits with its root's counter: two runs' rows.
        let unit = format!("{id}#jump");
        let spent = bounded(&metrics, "spend_child", 0, || s.spend(&c, &unit, 100, 1))?;
        assert!(matches!(spent, Record::Budget { granted: true, .. }));
        bounded(&metrics, "spend_child_replayed", 0, || s.spend(&c, &unit, 100, 1))?;
        if id == "target:c" {
            bounded(&metrics, "finish_child", 1, || s.finish(&c, &Status::Completed(json!(42)), 1, 3))?;
        }
    }
    assert!(bounded(&metrics, "cancel_cascade", 1, || s.cancel(&a.id, 1))?);
    bounded(&metrics, "finish", 0, || s.finish(&admitted, &Status::Completed(json!(42)), 1, 3))?;
    assert!(bounded(&metrics, "get", 0, || s.get(&a.id))?.is_some());
    assert!(bounded(&metrics, "get_absent", 0, || s.get("missing"))?.is_none());
    bounded(&metrics, "inspect", 2, || s.inspect(&a.id))?;
    assert_eq!(bounded(&metrics, "list_name", 5, || s.list(Some("target"), "target:"))?.len(), 5);
    assert_eq!(bounded(&metrics, "list_prefix", 5, || s.list(None, "target:"))?.len(), 5);
    assert_eq!(bounded(&metrics, "due_page", 7, || s.due(1, 7))?.len(), 7);
    assert!(bounded(&metrics, "due_empty", 0, || s.due(-1, 7))?.is_empty());
    assert!(bounded(&metrics, "due_zero_limit", 0, || s.due(i64::MAX, 0))?.is_empty());
    sweep_bounded(&backend, &metrics, "sweep", || s.sweep(1, 0))?;
    assert_eq!(bounded(&metrics, "sweep_already_swept", 0, || s.sweep(1, 0))?, ouro_durable::Swept::default());
    assert_eq!(s.list(None, "seed").unwrap().len(), 10_000);
    cascade(&backend, &metrics)?;
    long_run(&backend, &metrics, validation)?;
    Ok(())
}

/// A parent's end cancels its owned live descendants, transitively, in its
/// own commit (#14). The cascade walks only the `children` index of live
/// runs, so finished children cost nothing and each live descendant a
/// constant: its index entry and run row, its run row rewritten with the
/// index entries it loses, and its terminal notification to its parent.
fn cascade(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    let s = TransactionalStore::new(backend.clone());
    let root = s.start(&admission("tree:root"), 1)?.unwrap();
    let link = |parent: &str, key: &str| ouro_durable::ChildLink {
        parent: parent.into(),
        key: key.into(),
        reference: "/child".into(),
    };
    let mut live = Vec::new();
    for n in 0..64 {
        let id = format!("tree:child{n:02}");
        let c = s.start_child(&root, &admission(&id), &link("tree:root", &id), 1)?.unwrap();
        if n % 4 == 0 {
            s.finish(&c, &Status::Completed(json!(n)), 1, 3)?;
        } else {
            live.push(id);
        }
    }
    // A chain four levels below the root.
    let mut parent = s.start_child(&root, &admission("tree:chain1"), &link("tree:root", "chain"), 1)?.unwrap();
    live.push("tree:chain1".into());
    for depth in 2..=4 {
        let id = format!("tree:chain{depth}");
        let fence = s.start_child(&parent, &admission(&id), &link(&parent.run_id, "chain"), 1)?.unwrap();
        live.push(id);
        parent = fence;
    }
    let matches = live.len() as u64;
    metrics.reset();
    s.finish(&root, &Status::Completed(json!(0)), 2, 3)?;
    let counts = metrics.counts();
    eprintln!("BOUND,finish_cascade,{},{},descendants={matches}", counts.rows_read, counts.rows_written);
    assert!(counts.rows_read <= 32 + 16 * matches, "finish_cascade: {counts:?}");
    assert!(counts.rows_written <= 16 + 8 * matches, "finish_cascade: {counts:?}");
    assert_eq!(counts.conflicts, 0);
    for id in &live {
        let r = s.get(id)?.unwrap();
        assert_eq!((r.outcome, r.ended_ms), (Some(Status::Cancelled), Some(2)), "{id} outlived its parent");
    }
    for n in (0..64).step_by(4) {
        assert_eq!(s.get(&format!("tree:child{n:02}"))?.unwrap().outcome, Some(Status::Completed(json!(n))));
    }
    Ok(())
}

/// Retention reads what it removes: the swept runs' records, copies and their
/// index rows, one row apiece, counted from persisted rows outside the
/// measurement.
fn sweep_bounded(
    backend: &Arc<dyn AtomicBackend>,
    metrics: &Metrics,
    label: &str,
    sweep: impl FnOnce() -> anyhow::Result<ouro_durable::Swept>,
) -> anyhow::Result<ouro_durable::Swept> {
    let kept = backend.snapshot()?.rows.len();
    metrics.reset();
    let swept = sweep()?;
    let counts = metrics.counts();
    let removed = u64::try_from(kept - backend.snapshot()?.rows.len())?;
    assert!(removed > 0, "{label}: nothing swept");
    assert!(counts.rows_read <= 32 + 4 * removed && counts.rows_written <= 16 + 4 * removed, "{label}: {counts:?}");
    assert_eq!(counts.conflicts, 0);
    eprintln!("BOUND,{label},{},{},matches={removed}", counts.rows_read, counts.rows_written);
    Ok(swept)
}

/// One run holding 10,000 records (1,000 of them consumed events) and a
/// backlog of pending mailbox copies, among the 10,000 seeded runs: every
/// per-record operation reads and writes a constant number of rows, and
/// `load_records` reads exactly the run's row and its records. `validation`
/// is how often the adapter counts one read (memory validates every read at
/// commit).
fn long_run(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics, validation: u64) -> anyhow::Result<()> {
    const RECORDS: usize = 10_000;
    const EVENTS: usize = 1_000;
    const BACKLOG: usize = 100;
    let s = TransactionalStore::new(backend.clone()).with_policy(Policy { mailbox_ms: 1_000, ..Policy::default() });
    let mut a = admission("long:r");
    a.name = "long".into();
    let id = a.id.clone();
    let f = s.start(&a, 0)?.unwrap();
    let types = vec!["long".to_string()];
    for n in 0..EVENTS {
        s.deliver(&event(&format!("long{n}"), "long", json!(n)), Some(&id), 0)?;
        s.take_event(&f, &format!("/do/{n}/listen#event[0]"), &types, 0)?.unwrap();
    }
    for n in EVENTS..RECORDS {
        s.save_record(&f, &format!("/do/{n}/step#wait"), &Record::Timer { deadline_ms: 42 }, 0)?;
    }
    // Pending copies of a type nobody listens for, expiring at 1,000.
    for n in 0..BACKLOG {
        s.deliver(&event(&format!("backlog{n}"), "backlog", json!(n)), Some(&id), 0)?;
    }
    let timer = Record::Timer { deadline_ms: 42 };
    bounded(metrics, "long_save_record", 0, || s.save_record(&f, "/do/new/step#wait", &timer, 1))?;
    bounded(metrics, "long_save_record_again", 0, || s.save_record(&f, "/do/new/step#wait", &timer, 1))?;
    let call = |state| Record::Call {
        logical: "/do/call".into(),
        function: "f".into(),
        arguments: json!({}),
        delivery: owf::durable::Delivery::AtMostOnce,
        state,
    };
    bounded(metrics, "long_save_record_intent", 0, || {
        s.save_record(&f, "/do/call#call", &call(owf::durable::CallState::Pending), 1)
    })?;
    bounded(metrics, "long_save_record_result", 0, || {
        s.save_record(&f, "/do/call#call", &call(owf::durable::CallState::Done(Ok(json!(1)))), 1)
    })?;
    assert!(s.save_record(&f, "/do/1000/step#wait", &Record::Timer { deadline_ms: 43 }, 1).is_err());
    // Exactly the run's row and its records: nothing of other runs.
    let records = u64::try_from(RECORDS + 2)?;
    metrics.reset();
    assert_eq!(s.load_records(&f, 1)?.len(), RECORDS + 2);
    let counts = metrics.counts();
    assert_eq!((counts.rows_read, counts.rows_written), (validation * (records + 1), 0), "long_load_records");
    eprintln!("BOUND,long_load_records,{},{},records={records}", counts.rows_read, counts.rows_written);
    // A jump decision of a run holding 10,000 records: its record row and its
    // tree's counter, nothing that grows with the run.
    let spent = bounded(metrics, "long_spend", 0, || s.spend(&f, "/do/loop#jump", 100, 1))?;
    assert_eq!(spent, Record::Budget { granted: true, spent: 1, first: None });
    bounded(metrics, "long_spend_replayed", 0, || s.spend(&f, "/do/loop#jump", 100, 1))?;
    bounded(metrics, "long_register_wait", 0, || s.register_wait(&f, &types, 1))?;
    let next = event("long-next", "long", json!("next"));
    assert_eq!(bounded(metrics, "long_deliver", 1, || s.deliver(&next, Some(&id), 1))?, Some(id.clone()));
    assert_eq!(bounded(metrics, "long_deliver_again", 1, || s.deliver(&next, Some(&id), 1))?, Some(id.clone()));
    let taken = bounded(metrics, "long_take_event", 0, || s.take_event(&f, "/do/next#event[0]", &types, 1))?;
    assert_eq!(taken.unwrap()["data"], "next");
    bounded(metrics, "long_take_event_replay", 0, || s.take_event(&f, "/do/next#event[0]", &types, 1))?;
    assert!(
        bounded(metrics, "long_take_event_pending", 0, || s.take_event(&f, "/do/wait#event[0]", &types, 1))?.is_none()
    );
    bounded(metrics, "long_renew", 0, || s.renew(&f, 1))?;
    bounded(metrics, "long_finish_waiting", 0, || s.finish(&f, &Status::Waiting { wake_at_ms: 500 }, 1, 3))?;
    assert_eq!(s.get(&id)?.unwrap().wake_at_ms, 500, "the backlog is not waited for");
    let f = bounded(metrics, "long_claim", 0, || s.claim(&id, 1_000))?.unwrap();
    // The backlog's time is over: a take consumes it with expiry records,
    // reading the expired copies only.
    let expired = u64::try_from(BACKLOG)?;
    metrics.reset();
    assert!(s.take_event(&f, "/do/late#event[0]", &types, 1_000)?.is_none());
    let counts = metrics.counts();
    // Per expired copy: its index row and copy are read, its expiry record
    // and copy written and its three pending rows dropped.
    assert!(counts.rows_read <= 32 + 6 * validation * expired, "long_take_event_expired: {counts:?}");
    assert!(counts.rows_written <= 16 + 5 * expired, "long_take_event_expired: {counts:?}");
    assert_eq!(counts.conflicts, 0);
    eprintln!("BOUND,long_take_event_expired,{},{},matches={expired}", counts.rows_read, counts.rows_written);
    assert_eq!(s.load_records(&f, 1_000)?.len(), RECORDS + 4 + BACKLOG);
    assert!(bounded(metrics, "long_cancel", 0, || s.cancel(&id, 1_000))?);
    let swept = sweep_bounded(backend, metrics, "long_sweep", || s.sweep(2_000, 0))?;
    assert!(swept.records >= RECORDS + 4 + BACKLOG);
    assert!(s.inspect(&id)?.unwrap().records.is_empty());
    Ok(())
}

/// The paths the #3/#4 decisions added or changed, against the same 10,000
/// unrelated persisted runs (seeded, undeclared, subscribed elsewhere).
fn decided(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    // Short lifetimes so expiry happens at small clock values.
    let s = TransactionalStore::new(backend.clone()).with_policy(Policy {
        mailbox_ms: 10,
        ingress_ms: 10,
        ..Policy::default()
    });
    // Mailbox: an addressed event to a run that does not listen yet.
    let mut a = admission("decided:mailbox");
    a.name = "decided".into();
    s.admit(&a, 0)?;
    let early = event("early", "early", json!(1));
    assert_eq!(bounded(metrics, "deliver_addressed", 1, || s.deliver(&early, Some(&a.id), 0))?, Some(a.id.clone()));
    assert_eq!(
        bounded(metrics, "deliver_addressed_again", 1, || s.deliver(&early, Some(&a.id), 0))?,
        Some(a.id.clone())
    );
    let changed = event("early", "early", json!(2));
    assert!(bounded(metrics, "deliver_conflict", 1, || Ok(s.deliver(&changed, Some(&a.id), 0).is_err()))?);
    // Expiry by a later take, and by sweep for a run that never takes.
    let f = s.claim(&a.id, 10)?.unwrap();
    assert!(bounded(metrics, "take_event_expired", 1, || s.take_event(&f, "late", &["early".into()], 10))?.is_none());
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 10, 3)?;
    s.deliver(&event("idle", "early", json!(3)), Some(&a.id), 10)?;
    assert_eq!(bounded(metrics, "sweep_mailbox", 1, || s.sweep(20, i64::MAX / 2))?.expired, 1);
    // Ingress: one tombstone per identity, refused afterwards, then swept.
    let on = Trigger { at_ms: None, types: vec!["decided".into()], state: json!({ "events": [] }) };
    s.swap_trigger("decided#on", None, Some(&on), &[], None, 0)?;
    let order = event("order", "decided", json!(1));
    let mut started = a.clone();
    started.id = "decided:order".into();
    let admit = [started];
    let firing = Firing {
        key: "decided#on",
        expected: Some(&on),
        next: Some(&on),
        admit: &admit,
        ingress: Some(&order),
        ..Firing::default()
    };
    assert!(!bounded(metrics, "ingested", 0, || s.ingested(&order, 0))?);
    assert_eq!(bounded(metrics, "fire_ingress", 1, || s.fire(&firing, 0))?, Fired::Committed);
    assert_eq!(bounded(metrics, "fire_ingested", 1, || s.fire(&firing, 1))?, Fired::Ingested);
    assert_eq!(bounded(metrics, "sweep_tombstones", 1, || s.sweep(10, i64::MAX / 2))?.tombstones, 1);
    // Exclusive firing with the previous run still unfinished.
    // Far in the future: the bounds below read the due-trigger index.
    let slot = |at: i64| Trigger { at_ms: Some(2_000_000_000 + at), types: vec![], state: json!({ "at": at }) };
    s.swap_trigger("decided#every", None, Some(&slot(1)), &[], None, 0)?;
    let mut first = a.clone();
    first.id = "decided:slot1".into();
    let mut second = a.clone();
    second.id = "decided:slot2".into();
    let exclusive = |expected: &Trigger, next: &Trigger, admit: &[ouro_durable::Admission]| {
        s.fire(
            &Firing {
                key: "decided#every",
                expected: Some(expected),
                next: Some(next),
                admit,
                busy: Some(next),
                ..Firing::default()
            },
            1,
        )
    };
    let one = std::slice::from_ref(&first);
    let two = std::slice::from_ref(&second);
    assert_eq!(bounded(metrics, "fire_exclusive", 1, || exclusive(&slot(1), &slot(2), one))?, Fired::Committed);
    let busy = bounded(metrics, "fire_busy", 1, || exclusive(&slot(2), &slot(3), two))?;
    assert_eq!(busy, Fired::Busy(first.id.clone()));
    // Activation epochs: arm, re-arm, trip (removing the trigger and storing
    // the notice in the terminal commit), then hand the notice on.
    let v1 = s.save_definition("decided", "one")?;
    let v2 = s.save_definition("decided", "two")?;
    let arm = |keys: &[&str]| keys.iter().map(|k| ((*k).to_string(), slot(100))).collect();
    let arming = Arming { triggers: arm(&["decided#a", "decided#b"]) };
    let epoch = s.definitions("decided")?.epoch;
    bounded(metrics, "activate_with", 2, || s.activate_with("decided", v1.version, epoch, &arming))?.unwrap();
    let arming = Arming { triggers: arm(&["decided#c"]) };
    let epoch = s.definitions("decided")?.epoch;
    bounded(metrics, "activate_rearm", 3, || s.activate_with("decided", v2.version, epoch, &arming))?.unwrap();
    let mut faulty = a.clone();
    faulty.id = "decided:trip".into();
    faulty.version = v2.version;
    let f = s.start(&faulty, 0)?.unwrap();
    let fault = Status::Faulted(WorkflowError::runtime("fault"));
    bounded(metrics, "finish_trip", 2, || s.finish(&f, &fault, 0, 1))?;
    let d = s.definitions("decided")?;
    assert!(d.tripped.is_some() && d.active == Some(v2.version) && d.triggers.is_empty());
    let notices = bounded(metrics, "notices", 1, || s.notices(8))?;
    assert_eq!(notices.len(), 1);
    assert!(bounded(metrics, "acknowledge", 1, || s.acknowledge(&notices[0].0))?);
    // Tripped, it admits nothing, by a constant number of reads; the host's
    // activation admits again.
    let mut refused = a.clone();
    refused.id = "decided:refused".into();
    metrics.reset();
    assert!(s.admit(&refused, 0).unwrap_err().is::<ouro_durable::DefinitionTripped>());
    let counts = metrics.counts();
    assert!(counts.rows_read <= 32 && counts.rows_written == 0, "admit_tripped: {counts:?}");
    let epoch = s.definitions("decided")?.epoch;
    s.activate_with("decided", v2.version, epoch, &Arming::default())?.unwrap();
    // Version-aware claims page through their own partitions only.
    let mut versioned = a.clone();
    versioned.id = "decided:versioned".into();
    versioned.runtime = json!({ "application_version": "2" });
    s.admit(&versioned, 0)?;
    let worker = Compatibility { version: Some("2".into()), accepts: vec!["1".into()] };
    let due = bounded(metrics, "due_as", 8, || s.due_as(0, 8, &worker))?;
    assert!(due.contains(&versioned.id) && due.len() == 8);
    let other = Compatibility { version: Some("3".into()), accepts: vec![] };
    assert!(bounded(metrics, "claim_as_incompatible", 0, || s.claim_as(&versioned.id, 0, &other))?.is_none());
    assert!(bounded(metrics, "claim_as", 0, || s.claim_as(&versioned.id, 0, &worker))?.is_some());
    Ok(())
}

fn workers(
    store: Arc<dyn RunStore>,
    reopen: Arc<dyn Fn() -> anyhow::Result<Arc<dyn RunStore>> + Send + Sync>,
) -> anyhow::Result<()> {
    const RUNS: usize = 128;
    let d = store.save_definition("contention", "yaml")?;
    store.activate("contention", d.version)?;
    for n in 0..RUNS {
        let mut a = admission(&format!("load:{n:04}"));
        a.name = d.name.clone();
        a.version = d.version;
        store.admit(&a, 0)?;
    }
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let reopen = reopen.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || -> anyhow::Result<usize> {
                let s = reopen()?;
                barrier.wait();
                let mut completed = 0;
                for n in 0..RUNS {
                    let id = format!("load:{n:04}");
                    let Some(old) = s.claim(&id, 0)? else {
                        continue;
                    };
                    let now = ouro_durable::LEASE_MS + 1;
                    let f = s.claim(&id, now)?.unwrap();
                    assert_eq!(f.generation, old.generation + 1);
                    assert!(s.save_record(&old, "stale", &Record::Timer { deadline_ms: 42 }, now).is_err());
                    assert!(s.renew(&old, now).is_err());
                    assert!(s.finish(&old, &Status::Completed(json!(0)), now, 3).is_err());
                    for k in ["one", "two", "three"] {
                        s.save_record(&f, k, &Record::Timer { deadline_ms: 42 }, now)?;
                    }
                    let types = vec![format!("event:{id}")];
                    s.register_wait(&f, &types, now)?;
                    assert_eq!(s.deliver(&event(&id, &types[0], json!(n)), None, now)?, Some(id.clone()));
                    assert_eq!(s.take_event(&f, "event", &types, now)?.unwrap()["data"], json!(n));
                    s.finish(&f, &Status::Faulted(WorkflowError::runtime("counted")), now, u32::MAX)?;
                    completed += 1;
                }
                Ok(completed)
            })
        })
        .collect();
    let mut completed = 0;
    for worker in threads {
        completed += worker.join().expect("load worker panicked")?;
    }
    assert_eq!(completed, RUNS);
    assert_eq!(usize::try_from(store.definitions("contention")?.consecutive_faults)?, RUNS);
    for r in store.list(Some("contention"), "load:")? {
        assert_eq!(r.state, RunState::Failed);
        assert_eq!(r.generation, 2);
        let saved = store.inspect(&r.admission.id)?.unwrap();
        assert_eq!(saved.records.len(), 4);
        assert!(!saved.records.contains_key("stale"));
        assert_eq!(saved.events.len(), 1);
        assert_eq!(saved.events[0].consumed_key.as_deref(), Some("event"));
    }
    eprintln!("CONCURRENCY,workers=8,runs={completed},generations=2,records=512,breaker_faults={RUNS}");
    Ok(())
}

/// Force a read/write race, then let the engine retry it. SQLite serializes
/// writers before the first read, so this gate applies to OCC/SSI adapters.
struct GatedBackend {
    inner: Arc<dyn AtomicBackend>,
    gate: Arc<Barrier>,
    first: std::sync::atomic::AtomicBool,
}
struct GatedTransaction<'a> {
    inner: Box<dyn ouro_durable::Transaction + 'a>,
    gate: &'a GatedBackend,
}
impl AtomicBackend for GatedBackend {
    fn begin(&self) -> anyhow::Result<Box<dyn ouro_durable::Transaction + '_>> {
        Ok(Box::new(GatedTransaction { inner: self.inner.begin()?, gate: self }))
    }
    fn snapshot(&self) -> anyhow::Result<ouro_durable::Snapshot> {
        self.inner.snapshot()
    }
}
impl ouro_durable::Transaction for GatedTransaction<'_> {
    fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let result = self.inner.get(kind, id)?;
        if kind == "run" && id == "race" && !self.gate.first.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.gate.gate.wait();
        }
        Ok(result)
    }
    fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<ouro_durable::Rows> {
        self.inner.scan(kind, start, end, limit)
    }
    fn put(&mut self, kind: &str, id: &str, value: &serde_json::Value) -> anyhow::Result<()> {
        self.inner.put(kind, id, value)
    }
    fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.inner.delete(kind, id)
    }
    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.inner.commit()
    }
}
fn retry_race(
    backend: Arc<dyn AtomicBackend>,
    reopen: impl Fn() -> anyhow::Result<Arc<dyn AtomicBackend>>,
    metrics: impl Fn() -> IoCounts,
) -> anyhow::Result<()> {
    let s = TransactionalStore::new(backend);
    s.admit(&admission("race"), 0)?;
    let before = metrics().conflicts;
    let gate = Arc::new(Barrier::new(2));
    let stores: Vec<_> = (0..2)
        .map(|_| {
            Ok(TransactionalStore::new(Arc::new(GatedBackend {
                inner: reopen()?,
                gate: gate.clone(),
                first: std::sync::atomic::AtomicBool::new(false),
            })))
        })
        .collect::<anyhow::Result<_>>()?;
    let threads: Vec<_> = stores.into_iter().map(|s| std::thread::spawn(move || s.claim("race", 0))).collect();
    let mut fences: Vec<Fence> = Vec::new();
    for t in threads {
        fences.extend(t.join().unwrap()?);
    }
    assert_eq!(fences.len(), 1);
    let conflicts = metrics().conflicts - before;
    assert!(conflicts > 0, "the forced race must be retried");
    eprintln!("RETRY_RACE,winners=1,conflicts={conflicts}");
    Ok(())
}

fn ordering(store: &dyn RunStore) -> anyhow::Result<()> {
    for (id, at) in [("ordering:later", -20), ("ordering:earlier", -100)] {
        store.admit(&admission(id), at)?;
    }
    assert_eq!(store.due(-21, 10)?, vec!["ordering:earlier"]);
    assert_eq!(store.due(-10, 2)?, vec!["ordering:earlier", "ordering:later"]);
    for id in ["ordering:earlier", "ordering:later"] {
        store.cancel(id, 0)?;
    }
    let name = "colon:name/名";
    let mut expected = vec!["unicode:é", "unicode:😀", "unicode:\u{e000}", "unicode:\u{d7ff}", "unicode:\u{10ffff}"];
    for id in &expected {
        let mut a = admission(id);
        a.name = name.into();
        store.admit(&a, 0)?;
    }
    expected.sort();
    assert_eq!(
        store.list(Some(name), "unicode:")?.iter().map(|r| r.admission.id.as_str()).collect::<Vec<_>>(),
        expected
    );
    for prefix in ["unicode:\u{d7ff}", "unicode:\u{10ffff}"] {
        assert_eq!(store.list(None, prefix)?.len(), 1);
        assert_eq!(store.list(Some(name), prefix)?.len(), 1);
    }
    Ok(())
}
/// Every range-scanned index kind, the #3/#4 ones and a run's own rows included.
const INDEX_KINDS: [&str; 17] = [
    "record:72",
    "event:72",
    "event_type:72",
    "pending:72:616e73776572",
    "live:72:616e73776572",
    "published:72",
    "expiry:72",
    "due",
    "due_app:",
    "wait:616e73776572",
    "children:70",
    "name:746172676574",
    "finished",
    "mailbox",
    "ingress_due",
    "delivered:2f",
    "trigger_due",
];
fn sqlite_query_plans(path: &std::path::Path) -> anyhow::Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    for kind in INDEX_KINDS {
        let mut query = connection.prepare(
            "EXPLAIN QUERY PLAN SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND id<?3 ORDER BY id LIMIT ?4",
        )?;
        let plan = query
            .query_map(rusqlite::params![kind, "", "g", 7], |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .join(" ");
        assert!(plan.contains("SEARCH ouro_rows USING PRIMARY KEY"), "{kind}: {plan}");
    }
    Ok(())
}
fn postgres_query_plans(config: &tokio_postgres::Config) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let (client,connection) = config.connect(tokio_postgres::NoTls).await?;
        let driver = tokio::spawn(connection);
        let result = async {
            client.batch_execute("ANALYZE ouro_rows").await?;
            for kind in INDEX_KINDS {
                let plan = client.query("EXPLAIN SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 AND id<$3 ORDER BY id LIMIT $4",&[&kind,&"",&"g",&7_i64]).await?.into_iter().map(|r| r.get::<_,String>(0)).collect::<Vec<_>>().join(" ");
                assert!((plan.contains("Index Scan using ouro_rows_pkey") || plan.contains("Bitmap Index Scan on ouro_rows_pkey")),"{kind}: {plan}");
                assert!(!plan.contains("Seq Scan"),"{kind}: {plan}");
            }
            Ok::<_,anyhow::Error>(())
        }.await;
        drop(client);
        driver.await??;
        result
    })
}

fn index_atomicity(
    backend: Arc<dyn AtomicBackend>,
    metrics: Arc<Metrics>,
    faults: Arc<ouro_durable::FaultInjector>,
) -> anyhow::Result<()> {
    use ouro_durable::CommitFault;
    let s = TransactionalStore::new(backend.clone());
    let mut checked = 0;
    for operation in [
        "start",
        "child",
        "finish",
        "cancel",
        "deliver",
        "take",
        "sweep",
        "release",
        "renew",
        "record",
        "register",
        "unregister",
        "definition",
        "activate",
        "subscribe",
        "publish",
        "trigger",
        "mailbox",
        "ingress",
        "arm",
        "trip",
    ] {
        let before = backend.snapshot()?;
        assert!(backend.compare_exchange(&before, &ouro_durable::Rows::new())?);
        let p = s.start(&admission("p"), 0)?.unwrap();
        s.register_wait(&p, &["answer".into(), "deliverable".into()], 0)?;
        s.take_event(&p, "event", &["answer".into()], 0)?;
        s.deliver(&event("42", "answer", json!(42)), None, 0)?;
        let c = s
            .start_child(
                &p,
                &admission("c"),
                &ouro_durable::ChildLink { parent: "p".into(), key: "child".into(), reference: "/child".into() },
                0,
            )?
            .unwrap();
        s.save_record(&c, "timer", &Record::Timer { deadline_ms: 42 }, 0)?;
        let first = s.save_definition("workflow", "first")?;
        s.activate("workflow", first.version)?;
        let second = s.save_definition("workflow", "second")?;
        let epoch = s.definitions("workflow")?.epoch;
        if operation == "sweep" {
            s.finish(&p, &Status::Completed(json!(42)), 0, 3)?;
        }
        // A run whose fault trips the breaker of an activation with a trigger.
        let mut tripping = None;
        if operation == "trip" {
            let trigger = Trigger { at_ms: Some(5), types: vec!["t".into()], state: json!(1) };
            let arming = Arming { triggers: BTreeMap::from([("armed".to_string(), trigger)]) };
            s.activate_with("workflow", second.version, epoch, &arming)?;
            let mut a = admission("tripping");
            a.version = second.version;
            tripping = s.start(&a, 0)?;
        }
        let action = || -> anyhow::Result<()> {
            match operation {
                "start" => s.start(&admission("new"), 0).map(|_| ()),
                "child" => s
                    .start_child(
                        &p,
                        &admission("new"),
                        &ouro_durable::ChildLink { parent: "p".into(), key: "new".into(), reference: "/new".into() },
                        0,
                    )
                    .map(|_| ()),
                "finish" => s.finish(&c, &Status::Completed(json!(42)), 0, 3).map(|_| ()),
                "cancel" => s.cancel("p", 0).map(|_| ()),
                "deliver" => s.deliver(&event("deliverable", "deliverable", json!(42)), None, 0).map(|_| ()),
                "take" => s.take_event(&p, "event", &["answer".into()], 0).map(|_| ()),
                "sweep" => s.sweep(1, 0).map(|_| ()),
                "release" => s.release(&p, 0),
                "renew" => s.renew(&p, 1),
                "record" => s.save_record(&p, "timer", &Record::Timer { deadline_ms: 42 }, 0),
                "register" => s.register_wait(&p, &["new".into()], 0),
                "unregister" => s.unregister_wait(&p, "event", &["answer".into()], 0),
                "definition" => s.save_definition("workflow", "third").map(|_| ()),
                "activate" => s.activate("workflow", second.version),
                "subscribe" => s.subscribe(&p, "listen", &["answer".into()], 0),
                "publish" => s.publish(&json!({"id":"1","source":"/s","type":"published"}), Some("p"), 0).map(|_| ()),
                "mailbox" => s.deliver(&event("early", "early", json!(1)), Some("p"), 0).map(|_| ()),
                "ingress" => s
                    .fire(
                        &Firing {
                            key: "on",
                            next: Some(&Trigger { at_ms: None, types: vec!["t".into()], state: json!(1) }),
                            admit: &[admission("ingress")],
                            ingress: Some(&event("ingress", "t", json!(1))),
                            ..Firing::default()
                        },
                        0,
                    )
                    .map(|_| ()),
                "arm" => s
                    .activate_with(
                        "workflow",
                        second.version,
                        epoch,
                        &Arming {
                            triggers: BTreeMap::from([(
                                "armed".to_string(),
                                Trigger { at_ms: Some(5), types: vec!["t".into()], state: json!(1) },
                            )]),
                        },
                    )
                    .map(|_| ()),
                "trigger" => s
                    .swap_trigger(
                        "slot",
                        None,
                        Some(&Trigger { at_ms: Some(5), types: vec!["t".into()], state: json!(1) }),
                        &[admission("fired")],
                        Some(&Rearm { trigger: "slot".into(), delay_ms: 1 }),
                        0,
                    )
                    .map(|_| ()),
                "trip" => {
                    let fault = Status::Faulted(WorkflowError::runtime("fault"));
                    s.finish(tripping.as_ref().unwrap(), &fault, 0, 1)?;
                    anyhow::ensure!(s.definitions("workflow")?.tripped.is_some(), "the fault did not trip");
                    Ok(())
                }
                _ => unreachable!(),
            }
        };
        let before = backend.snapshot()?;
        metrics.reset();
        action()?;
        let writes = usize::try_from(metrics.counts().rows_written)?;
        assert!(writes > 0);
        let after = backend.snapshot()?;
        assert!(backend.compare_exchange(&after, &before.rows)?);
        for fault in
            (0..writes).map(CommitFault::BeforeWrite).chain([CommitFault::BeforeCommit, CommitFault::AfterCommit])
        {
            faults.arm(fault);
            assert!(action().is_err(), "{operation} at {fault:?}");
            faults.clear();
            let actual = backend.snapshot()?;
            if fault == CommitFault::AfterCommit {
                assert_ne!(actual, before, "ambiguous commit must remain visible");
                assert!(backend.compare_exchange(&actual, &before.rows)?);
            } else {
                assert_eq!(actual, before, "primary/index partial write: {operation} at {fault:?}");
            }
            checked += 1;
        }
    }
    let before = backend.snapshot()?;
    assert!(backend.compare_exchange(&before, &ouro_durable::Rows::new())?);
    eprintln!("INDEX_ATOMICITY,checked_faults={checked}");
    Ok(())
}

#[test]
fn memory_scoped_bounds_and_concurrency() {
    let b = Arc::new(MemoryBackend::default());
    index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone()).unwrap();
    bounds(b.clone(), b.metrics.clone(), 2).unwrap();
    ordering(&TransactionalStore::new(b.clone())).unwrap();
    workers(
        Arc::new(TransactionalStore::new(b.clone())),
        Arc::new({
            let b = b.clone();
            move || Ok(Arc::new(TransactionalStore::new(b.clone())))
        }),
    )
    .unwrap();
    retry_race(b.clone(), || Ok(b.clone()), || b.metrics.counts()).unwrap();
}
#[test]
fn sqlite_scoped_bounds_and_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.sqlite");
    let b = Arc::new(SqliteBackend::open(&path).unwrap());
    index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone()).unwrap();
    bounds(b.clone(), b.metrics.clone(), 1).unwrap();
    ordering(&TransactionalStore::new(b.clone())).unwrap();
    sqlite_query_plans(&path).unwrap();
    workers(Arc::new(TransactionalStore::new(b)), Arc::new(move || Ok(Arc::new(ouro_durable::sqlite::open(&path)?))))
        .unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn postgres_scoped_bounds_and_concurrency() {
    support::postgres(|config| {
        let b = Arc::new(PostgresBackend::connect_config(config.clone())?);
        index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone())?;
        bounds(b.clone(), b.metrics.clone(), 1)?;
        ordering(&TransactionalStore::new(b.clone()))?;
        postgres_query_plans(&config)?;
        workers(
            Arc::new(TransactionalStore::new(b.clone())),
            Arc::new({
                let config = config.clone();
                move || {
                    Ok(Arc::new(TransactionalStore::new(Arc::new(PostgresBackend::connect_config(config.clone())?))))
                }
            }),
        )?;
        let peers: Vec<_> = (0..2)
            .map(|_| Ok(Arc::new(PostgresBackend::connect_config(config.clone())?)))
            .collect::<anyhow::Result<_>>()?;
        let next = std::sync::atomic::AtomicUsize::new(0);
        retry_race(
            b,
            || Ok(peers[next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)].clone()),
            || IoCounts { conflicts: peers.iter().map(|b| b.metrics.counts().conflicts).sum(), ..IoCounts::default() },
        )
    })
    .await;
}

#[test]
fn memory_transactions_validate_absence_ranges_and_independent_keys() {
    let b = MemoryBackend::default();
    let mut a = b.begin().unwrap();
    let mut c = b.begin().unwrap();
    a.put("test", "one", &json!(1)).unwrap();
    c.put("test", "two", &json!(2)).unwrap();
    a.commit().unwrap();
    c.commit().unwrap();
    let mut a = b.begin().unwrap();
    let mut c = b.begin().unwrap();
    assert!(a.get("test", "missing").unwrap().is_none());
    c.put("test", "missing", &json!(3)).unwrap();
    c.commit().unwrap();
    assert!(a.commit().unwrap_err().is::<Conflict>());
    let mut a = b.begin().unwrap();
    let mut c = b.begin().unwrap();
    assert!(a.scan("empty", "", None, 1).unwrap().is_empty());
    c.put("empty", "phantom", &json!(4)).unwrap();
    c.commit().unwrap();
    assert!(a.commit().unwrap_err().is::<Conflict>());
    let mut a = b.begin().unwrap();
    let mut c = b.begin().unwrap();
    a.scan("test", "", None, 1).unwrap();
    c.put("test", "0", &json!(0)).unwrap();
    c.commit().unwrap();
    assert!(a.commit().unwrap_err().is::<Conflict>());
}
