use ouro_durable::{
    Arming, AtomicBackend, Compatibility, Conflict, Fence, Fired, Firing, IoCounts, Metrics, Overlap, OverlapGuard,
    Policy, Rearm, Refused, RunState, RunStore, TransactionalStore, Trigger,
    conformance::{admission, event},
    memory::MemoryBackend,
    postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use ows::{
    durable::Record,
    run::{Status, WorkflowError},
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Barrier;
mod support;

async fn bounded<T>(
    metrics: &Metrics,
    label: &str,
    matches: u64,
    operation: impl AsyncFnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    metrics.reset();
    let result = operation().await?;
    let counts = metrics.counts();
    assert!(counts.rows_read <= 32 + 4 * matches, "{label}: {counts:?}");
    assert!(counts.rows_written <= 16 + 4 * matches, "{label}: {counts:?}");
    assert_eq!(counts.conflicts, 0, "single worker should not conflict");
    eprintln!("BOUND,{label},{},{},matches={matches}", counts.rows_read, counts.rows_written);
    Ok(result)
}
async fn bounds(backend: Arc<dyn AtomicBackend>, metrics: Arc<Metrics>, validation: u64) -> anyhow::Result<()> {
    support::seed(&backend, 10_000).await?;
    control_bounds(&backend, &metrics).await?;
    decided(&backend, &metrics).await?;
    overlap_bounds(&backend, &metrics).await?;
    let s = TransactionalStore::new(backend.clone());
    let mut a = admission("target:r");
    a.name = "target".into();
    bounded(&metrics, "save_definition", 0, async || s.save_definition("target", "one").await).await?;
    let d = bounded(&metrics, "save_definition_existing", 0, async || s.save_definition("target", "two").await).await?;
    bounded(&metrics, "activate", 0, async || s.activate("target", d.version).await).await?;
    assert_eq!(bounded(&metrics, "definitions", 2, async || s.definitions("target").await).await?.versions.len(), 2);
    a.version = d.version;
    let mut other = a.clone();
    other.id = "target:admitted".into();
    assert!(bounded(&metrics, "admit", 0, async || s.admit(&other, 0).await).await?);
    let f = bounded(&metrics, "start", 0, async || s.start(&a, 0).await).await?.unwrap();
    assert!(bounded(&metrics, "idempotent_start", 0, async || s.start(&a, 0).await).await?.is_none());
    let admitted = bounded(&metrics, "claim", 0, async || s.claim(&other.id, 0).await).await?.unwrap();
    bounded(&metrics, "renew", 0, async || s.renew(&f, 1).await).await?;
    bounded(&metrics, "save_record", 0, async || {
        s.save_record(&f, "timer", &Record::Timer { deadline_ms: 42 }, 1).await
    })
    .await?;
    // Full pending inboxes must not turn a missing eligible delivery into a
    // scan over every registration of the same event type.
    for n in 0..64 {
        let f = s.start(&admission(&format!("saturated:{n}")), 1).await?.unwrap();
        s.register_wait(&f, &["crowded".into()], 1).await?;
        s.deliver(&event(&format!("crowded{n}"), "crowded", json!(n)), None, 1).await?.unwrap();
        s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 1, 3).await?;
    }
    assert!(
        bounded(&metrics, "deliver_saturated", 0, async || s
            .deliver(&event("crowded", "crowded", json!(0)), None, 1)
            .await)
        .await?
        .is_none()
    );
    let types = vec!["answer".into()];
    bounded(&metrics, "register_wait", 0, async || s.register_wait(&f, &types, 1).await).await?;
    assert!(
        bounded(&metrics, "take_event_pending", 0, async || s.take_event(&f, "answer", &types, 1).await)
            .await?
            .is_none()
    );
    let answer = |id: &str| event(id, "answer", json!(42));
    assert_eq!(
        bounded(&metrics, "deliver", 1, async || s.deliver(&answer("1"), None, 1).await).await?,
        Some(a.id.clone())
    );
    assert_eq!(
        bounded(&metrics, "deliver_again", 1, async || s.deliver(&answer("1"), None, 1).await).await?,
        Some(a.id.clone())
    );
    assert!(bounded(&metrics, "deliver_full", 0, async || s.deliver(&answer("2"), None, 1).await).await?.is_none());
    assert!(
        bounded(&metrics, "deliver_absent", 0, async || s.deliver(&event("3", "absent", json!(42)), None, 1).await)
            .await?
            .is_none()
    );
    assert!(bounded(&metrics, "take_event", 0, async || s.take_event(&f, "answer", &types, 1).await).await?.is_some());
    bounded(&metrics, "take_event_replay", 0, async || s.take_event(&f, "answer", &types, 1).await).await?;
    bounded(&metrics, "unregister_wait", 0, async || s.unregister_wait(&f, "answer", &types, 1).await).await?;
    assert_eq!(bounded(&metrics, "load_records", 2, async || s.load_records(&f, 1).await).await?.len(), 2);
    // Published events: thousands of seeded runs subscribe to another type.
    let published = |id: &str, t: &str| json!({ "id": id, "source": "/bounds", "type": t, "data": 1 });
    bounded(&metrics, "subscribe", 0, async || s.subscribe(&f, "listen", &["published".into()], 1).await).await?;
    assert_eq!(
        bounded(&metrics, "publish", 1, async || s.publish(&published("1", "published"), None, 1).await).await?.len(),
        1
    );
    assert!(
        bounded(&metrics, "publish_republished", 1, async || s.publish(&published("1", "published"), None, 1).await)
            .await?
            .is_empty()
    );
    assert!(
        bounded(&metrics, "publish_unsubscribed_type", 0, async || s.publish(&published("2", "nobody"), None, 1).await)
            .await?
            .is_empty()
    );
    assert_eq!(
        bounded(&metrics, "publish_target", 1, async || s.publish(&published("3", "x"), Some(&other.id), 1).await)
            .await?
            .len(),
        1
    );
    assert!(
        bounded(&metrics, "take_event_published", 0, async || s.take_event(&f, "published", &["*".into()], 1).await)
            .await?
            .is_some()
    );
    bounded(&metrics, "unsubscribe", 0, async || s.unsubscribe(&f, "listen", 1).await).await?;
    // Triggers: many scheduled entries far in the future and of other types.
    for n in 0..1_000 {
        let unrelated = Trigger { at_ms: Some(1_000_000 + n), types: vec!["unrelated".into()], state: json!(n) };
        s.swap_trigger(&format!("unrelated{n:04}"), None, Some(&unrelated), &[], None, 1).await?;
    }
    let timer = Trigger { at_ms: Some(5), types: vec!["order".into()], state: json!({ "n": 1 }) };
    assert!(
        bounded(&metrics, "swap_trigger_create", 0, async || s
            .swap_trigger("slot", None, Some(&timer), &[], None, 1)
            .await)
        .await?
    );
    assert!(bounded(&metrics, "trigger", 0, async || s.trigger("slot").await).await?.is_some());
    assert_eq!(bounded(&metrics, "due_triggers", 1, async || s.due_triggers(5, 16).await).await?.len(), 1);
    assert!(bounded(&metrics, "due_triggers_none", 0, async || s.due_triggers(4, 16).await).await?.is_empty());
    assert_eq!(bounded(&metrics, "triggers_for", 1, async || s.triggers_for("order", 16).await).await?.len(), 1);
    assert!(bounded(&metrics, "triggers_for_absent", 0, async || s.triggers_for("absent", 16).await).await?.is_empty());
    let mut slot_run = a.clone();
    slot_run.id = "target:slot".into();
    let parked = Trigger { at_ms: None, ..timer.clone() };
    let rearm = Rearm { trigger: "slot".into(), delay_ms: 10 };
    assert!(
        bounded(&metrics, "swap_trigger_fire", 1, async || {
            s.swap_trigger("slot", Some(&timer), Some(&parked), &[slot_run.clone()], Some(&rearm), 1).await
        })
        .await?
    );
    assert!(
        !bounded(&metrics, "swap_trigger_lost", 0, async || s
            .swap_trigger("slot", Some(&timer), None, &[], None, 1)
            .await)
        .await?
    );
    let fired = s.claim("target:slot", 1).await?.unwrap();
    bounded(&metrics, "finish_rearm", 1, async || s.finish(&fired, &Status::Completed(json!(0)), 2, 3).await).await?;
    assert_eq!(s.trigger("slot").await?.and_then(|t| t.at_ms), Some(12));
    bounded(&metrics, "finish_waiting", 0, async || s.finish(&f, &Status::Waiting { wake_at_ms: 1 }, 1, 3).await)
        .await?;
    let f = bounded(&metrics, "claim_waiting", 0, async || s.claim(&a.id, 1).await).await?.unwrap();
    bounded(&metrics, "release", 0, async || s.release(&f, 1).await).await?;
    let f = s.claim(&a.id, 1).await?.unwrap();
    assert_eq!(
        bounded(&metrics, "spend", 0, async || s.spend(&f, "loop#jump", 100, 1).await).await?,
        Record::Budget { granted: true, spent: 1, first: None }
    );
    bounded(&metrics, "spend_replayed", 0, async || s.spend(&f, "loop#jump", 100, 1).await).await?;
    assert!(matches!(
        bounded(&metrics, "spend_refused", 0, async || s.spend(&f, "late#jump", 1, 1).await).await?,
        Record::Budget { granted: false, .. }
    ));
    for id in ["target:c", "target:live"] {
        let mut child = a.clone();
        child.id = id.into();
        let link = ouro_durable::ChildLink { parent: a.id.clone(), key: id.into(), reference: "/child".into() };
        let c = bounded(&metrics, "start_child", 1, async || s.start_child(&f, &child, &link, 1).await).await?.unwrap();
        assert!(bounded(&metrics, "child_outcome", 0, async || s.child_outcome(&a.id, id, id).await).await?.is_none());
        // A child's unit commits with its root's counter: two runs' rows.
        let unit = format!("{id}#jump");
        let spent = bounded(&metrics, "spend_child", 0, async || s.spend(&c, &unit, 100, 1).await).await?;
        assert!(matches!(spent, Record::Budget { granted: true, .. }));
        bounded(&metrics, "spend_child_replayed", 0, async || s.spend(&c, &unit, 100, 1).await).await?;
        if id == "target:c" {
            bounded(&metrics, "finish_child", 1, async || s.finish(&c, &Status::Completed(json!(42)), 1, 3).await)
                .await?;
        }
    }
    assert!(bounded(&metrics, "cancel_cascade", 1, async || s.cancel(&a.id, 1).await).await?);
    bounded(&metrics, "finish", 0, async || s.finish(&admitted, &Status::Completed(json!(42)), 1, 3).await).await?;
    assert!(bounded(&metrics, "get", 0, async || s.get(&a.id).await).await?.is_some());
    assert!(bounded(&metrics, "get_absent", 0, async || s.get("missing").await).await?.is_none());
    bounded(&metrics, "inspect", 2, async || s.inspect(&a.id).await).await?;
    assert_eq!(bounded(&metrics, "list_name", 5, async || s.list(Some("target"), "target:").await).await?.len(), 5);
    assert_eq!(bounded(&metrics, "list_prefix", 5, async || s.list(None, "target:").await).await?.len(), 5);
    assert_eq!(bounded(&metrics, "due_page", 7, async || s.due(1, 7).await).await?.len(), 7);
    assert!(bounded(&metrics, "due_empty", 0, async || s.due(-1, 7).await).await?.is_empty());
    assert!(bounded(&metrics, "due_zero_limit", 0, async || s.due(i64::MAX, 0).await).await?.is_empty());
    sweep_bounded(&backend, &metrics, "sweep", async || s.sweep(1, 0).await).await?;
    assert_eq!(
        bounded(&metrics, "sweep_already_swept", 0, async || s.sweep(1, 0).await).await?,
        ouro_durable::Swept::default()
    );
    assert_eq!(s.list(None, "seed").await.unwrap().len(), 10_000);
    cascade(&backend, &metrics).await?;
    long_run(&backend, &metrics, validation).await?;
    Ok(())
}

/// A parent's end cancels its owned live descendants, transitively, in its
/// own commit (#14). The cascade walks only the `children` index of live
/// runs, so finished children cost nothing and each live descendant a
/// constant: its index entry and run row, its run row rewritten with the
/// index entries it loses, and its terminal notification to its parent.
async fn cascade(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    let s = TransactionalStore::new(backend.clone());
    let root = s.start(&admission("tree:root"), 1).await?.unwrap();
    let link = |parent: &str, key: &str| ouro_durable::ChildLink {
        parent: parent.into(),
        key: key.into(),
        reference: "/child".into(),
    };
    let mut live = Vec::new();
    for n in 0..64 {
        let id = format!("tree:child{n:02}");
        let c = s.start_child(&root, &admission(&id), &link("tree:root", &id), 1).await?.unwrap();
        if n % 4 == 0 {
            s.finish(&c, &Status::Completed(json!(n)), 1, 3).await?;
        } else {
            live.push(id);
        }
    }
    // A chain four levels below the root.
    let mut parent = s.start_child(&root, &admission("tree:chain1"), &link("tree:root", "chain"), 1).await?.unwrap();
    live.push("tree:chain1".into());
    for depth in 2..=4 {
        let id = format!("tree:chain{depth}");
        let fence = s.start_child(&parent, &admission(&id), &link(&parent.run_id, "chain"), 1).await?.unwrap();
        live.push(id);
        parent = fence;
    }
    let matches = live.len() as u64;
    metrics.reset();
    s.finish(&root, &Status::Completed(json!(0)), 2, 3).await?;
    let counts = metrics.counts();
    eprintln!("BOUND,finish_cascade,{},{},descendants={matches}", counts.rows_read, counts.rows_written);
    assert!(counts.rows_read <= 32 + 16 * matches, "finish_cascade: {counts:?}");
    assert!(counts.rows_written <= 16 + 8 * matches, "finish_cascade: {counts:?}");
    assert_eq!(counts.conflicts, 0);
    for id in &live {
        let r = s.get(id).await?.unwrap();
        assert_eq!((r.outcome, r.ended_ms), (Some(Status::Cancelled), Some(2)), "{id} outlived its parent");
    }
    for n in (0..64).step_by(4) {
        assert_eq!(s.get(&format!("tree:child{n:02}")).await?.unwrap().outcome, Some(Status::Completed(json!(n))));
    }
    Ok(())
}

/// Every policy uses only this trigger's live members, this definition's
/// bounded capacity page and its single buffered admission, among 10,000 runs.
async fn overlap_bounds(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    let s = TransactionalStore::new(backend.clone());
    for policy in [Overlap::Skip, Overlap::Allow, Overlap::Queue, Overlap::Replace] {
        let name = format!("overlap:{policy:?}");
        let k = format!("{name}#every");
        let slot = |n| Trigger { at_ms: None, types: vec![], state: json!(n) };
        s.swap_trigger(&k, None, Some(&slot(1)), &[], None, 0).await?;
        let fire = async |n, max| {
            let mut a = admission(&format!("{name}:{n}"));
            a.name = name.clone();
            s.fire(
                &Firing {
                    key: &k,
                    expected: Some(&slot(n)),
                    next: Some(&slot(n + 1)),
                    admit: &[a],
                    busy: Some(&slot(n + 1)),
                    overlap: Some(OverlapGuard { policy, max_concurrent_runs: max, ceiling: &slot(n + 1) }),
                    ..Firing::default()
                },
                n,
            )
            .await
        };
        assert_eq!(
            bounded(metrics, &format!("overlap_{policy:?}_first"), 1, async || fire(1, 10).await).await?,
            Fired::Committed
        );
        let second = bounded(metrics, &format!("overlap_{policy:?}_second"), 2, async || fire(2, 10).await).await?;
        assert_eq!(
            second,
            match policy {
                Overlap::Skip => Fired::Busy(format!("{name}:1")),
                Overlap::Queue => Fired::Queued,
                _ => Fired::Committed,
            }
        );
        if policy == Overlap::Queue {
            assert!(bounded(metrics, "queue_lookup", 0, async || s.queued(&k).await).await?.is_some());
            assert!(matches!(bounded(metrics, "queue_full", 2, async || fire(3, 10).await).await?, Fired::Busy(_)));
            bounded(metrics, "queue_promote", 2, async || s.cancel(&format!("{name}:1"), 4).await).await?;
            assert!(s.get(&format!("{name}:2")).await?.is_some());
        }
    }
    // Capacity is a limited live-name range; 10,000 unrelated runs never
    // contribute rows, even when the definition already has ten live runs.
    let k = "ceiling-bound";
    let t = Trigger { at_ms: None, types: vec![], state: json!({}) };
    s.swap_trigger(k, None, Some(&t), &[], None, 0).await?;
    for n in 0..10 {
        let mut a = admission(&format!("ceiling-bound:{n}"));
        a.name = k.into();
        s.admit(&a, 0).await?;
    }
    let mut a = admission("ceiling-bound:refused");
    a.name = k.into();
    assert_eq!(
        bounded(metrics, "overlap_ceiling", 10, async || s
            .fire(
                &Firing {
                    key: k,
                    expected: Some(&t),
                    next: Some(&t),
                    busy: Some(&t),
                    admit: &[a],
                    overlap: Some(OverlapGuard { policy: Overlap::Allow, max_concurrent_runs: 10, ceiling: &t }),
                    ..Firing::default()
                },
                1
            )
            .await)
        .await?,
        Fired::Ceiling
    );
    Ok(())
}

/// Retention reads what it removes: the swept runs' records, copies and their
/// index rows, one row apiece, counted from persisted rows outside the
/// measurement.
async fn sweep_bounded(
    backend: &Arc<dyn AtomicBackend>,
    metrics: &Metrics,
    label: &str,
    sweep: impl AsyncFnOnce() -> anyhow::Result<ouro_durable::Swept>,
) -> anyhow::Result<ouro_durable::Swept> {
    let kept = backend.snapshot().await?.rows.len();
    metrics.reset();
    let swept = sweep().await?;
    let counts = metrics.counts();
    let removed = u64::try_from(kept - backend.snapshot().await?.rows.len())?;
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
async fn long_run(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics, validation: u64) -> anyhow::Result<()> {
    const RECORDS: usize = 10_000;
    const EVENTS: usize = 1_000;
    const BACKLOG: usize = 100;
    let s = TransactionalStore::new(backend.clone()).with_policy(Policy { mailbox_ms: 1_000, ..Policy::default() });
    let mut a = admission("long:r");
    a.name = "long".into();
    let id = a.id.clone();
    let f = s.start(&a, 0).await?.unwrap();
    let types = vec!["long".to_string()];
    for n in 0..EVENTS {
        s.deliver(&event(&format!("long{n}"), "long", json!(n)), Some(&id), 0).await?;
        s.take_event(&f, &format!("/do/{n}/listen#event[0]"), &types, 0).await?.unwrap();
    }
    for n in EVENTS..RECORDS {
        s.save_record(&f, &format!("/do/{n}/step#wait"), &Record::Timer { deadline_ms: 42 }, 0).await?;
    }
    // Pending copies of a type nobody listens for, expiring at 1,000.
    for n in 0..BACKLOG {
        s.deliver(&event(&format!("backlog{n}"), "backlog", json!(n)), Some(&id), 0).await?;
    }
    let timer = Record::Timer { deadline_ms: 42 };
    bounded(metrics, "long_save_record", 0, async || s.save_record(&f, "/do/new/step#wait", &timer, 1).await).await?;
    bounded(metrics, "long_save_record_again", 0, async || s.save_record(&f, "/do/new/step#wait", &timer, 1).await)
        .await?;
    let call = |state| Record::Call {
        logical: "/do/call".into(),
        function: "f".into(),
        arguments: json!({}),
        delivery: ows::durable::Delivery::AtMostOnce,
        state,
    };
    bounded(metrics, "long_save_record_intent", 0, async || {
        s.save_record(&f, "/do/call#call", &call(ows::durable::CallState::Pending), 1).await
    })
    .await?;
    bounded(metrics, "long_save_record_result", 0, async || {
        s.save_record(&f, "/do/call#call", &call(ows::durable::CallState::Done(Ok(json!(1)))), 1).await
    })
    .await?;
    assert!(s.save_record(&f, "/do/1000/step#wait", &Record::Timer { deadline_ms: 43 }, 1).await.is_err());
    // Exactly the run's row and its records: nothing of other runs.
    let records = u64::try_from(RECORDS + 2)?;
    metrics.reset();
    assert_eq!(s.load_records(&f, 1).await?.len(), RECORDS + 2);
    let counts = metrics.counts();
    assert_eq!((counts.rows_read, counts.rows_written), (validation * (records + 1), 0), "long_load_records");
    eprintln!("BOUND,long_load_records,{},{},records={records}", counts.rows_read, counts.rows_written);
    // A jump decision of a run holding 10,000 records: its record row and its
    // tree's counter, nothing that grows with the run.
    let spent = bounded(metrics, "long_spend", 0, async || s.spend(&f, "/do/loop#jump", 100, 1).await).await?;
    assert_eq!(spent, Record::Budget { granted: true, spent: 1, first: None });
    bounded(metrics, "long_spend_replayed", 0, async || s.spend(&f, "/do/loop#jump", 100, 1).await).await?;
    bounded(metrics, "long_register_wait", 0, async || s.register_wait(&f, &types, 1).await).await?;
    let next = event("long-next", "long", json!("next"));
    assert_eq!(
        bounded(metrics, "long_deliver", 1, async || s.deliver(&next, Some(&id), 1).await).await?,
        Some(id.clone())
    );
    assert_eq!(
        bounded(metrics, "long_deliver_again", 1, async || s.deliver(&next, Some(&id), 1).await).await?,
        Some(id.clone())
    );
    let taken =
        bounded(metrics, "long_take_event", 0, async || s.take_event(&f, "/do/next#event[0]", &types, 1).await).await?;
    assert_eq!(taken.unwrap()["data"], "next");
    bounded(metrics, "long_take_event_replay", 0, async || s.take_event(&f, "/do/next#event[0]", &types, 1).await)
        .await?;
    assert!(
        bounded(metrics, "long_take_event_pending", 0, async || s.take_event(&f, "/do/wait#event[0]", &types, 1).await)
            .await?
            .is_none()
    );
    bounded(metrics, "long_renew", 0, async || s.renew(&f, 1).await).await?;
    bounded(metrics, "long_finish_waiting", 0, async || s.finish(&f, &Status::Waiting { wake_at_ms: 500 }, 1, 3).await)
        .await?;
    assert_eq!(s.get(&id).await?.unwrap().wake_at_ms, 500, "the backlog is not waited for");
    let f = bounded(metrics, "long_claim", 0, async || s.claim(&id, 1_000).await).await?.unwrap();
    // The backlog's time is over: a take consumes it with expiry records,
    // reading the expired copies only.
    let expired = u64::try_from(BACKLOG)?;
    metrics.reset();
    assert!(s.take_event(&f, "/do/late#event[0]", &types, 1_000).await?.is_none());
    let counts = metrics.counts();
    // Per expired copy: its index row and copy are read, its expiry record
    // and copy written and its three pending rows dropped.
    assert!(counts.rows_read <= 32 + 6 * validation * expired, "long_take_event_expired: {counts:?}");
    assert!(counts.rows_written <= 16 + 5 * expired, "long_take_event_expired: {counts:?}");
    assert_eq!(counts.conflicts, 0);
    eprintln!("BOUND,long_take_event_expired,{},{},matches={expired}", counts.rows_read, counts.rows_written);
    assert_eq!(s.load_records(&f, 1_000).await?.len(), RECORDS + 4 + BACKLOG);
    assert!(bounded(metrics, "long_cancel", 0, async || s.cancel(&id, 1_000).await).await?);
    let swept = sweep_bounded(backend, metrics, "long_sweep", async || s.sweep(2_000, 0).await).await?;
    assert!(swept.records >= RECORDS + 4 + BACKLOG);
    assert!(s.inspect(&id).await?.unwrap().records.is_empty());
    Ok(())
}

/// The paths the #3/#4 decisions added or changed, against the same 10,000
/// unrelated persisted runs (seeded, undeclared, subscribed elsewhere).
async fn decided(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    // Short lifetimes so expiry happens at small clock values.
    let s = TransactionalStore::new(backend.clone()).with_policy(Policy {
        mailbox_ms: 10,
        ingress_ms: 10,
        ..Policy::default()
    });
    // Mailbox: an addressed event to a run that does not listen yet.
    let mut a = admission("decided:mailbox");
    a.name = "decided".into();
    s.admit(&a, 0).await?;
    let early = event("early", "early", json!(1));
    assert_eq!(
        bounded(metrics, "deliver_addressed", 1, async || s.deliver(&early, Some(&a.id), 0).await).await?,
        Some(a.id.clone())
    );
    assert_eq!(
        bounded(metrics, "deliver_addressed_again", 1, async || s.deliver(&early, Some(&a.id), 0).await).await?,
        Some(a.id.clone())
    );
    let changed = event("early", "early", json!(2));
    assert!(
        bounded(metrics, "deliver_conflict", 1, async || Ok(s.deliver(&changed, Some(&a.id), 0).await.is_err()))
            .await?
    );
    // Expiry by a later take, and by sweep for a run that never takes.
    let f = s.claim(&a.id, 10).await?.unwrap();
    assert!(
        bounded(metrics, "take_event_expired", 1, async || s.take_event(&f, "late", &["early".into()], 10).await)
            .await?
            .is_none()
    );
    s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 10, 3).await?;
    s.deliver(&event("idle", "early", json!(3)), Some(&a.id), 10).await?;
    assert_eq!(bounded(metrics, "sweep_mailbox", 1, async || s.sweep(20, i64::MAX / 2).await).await?.expired, 1);
    // Ingress: one tombstone per identity, refused afterwards, then swept.
    let on = Trigger { at_ms: None, types: vec!["decided".into()], state: json!({ "events": [] }) };
    s.swap_trigger("decided#on", None, Some(&on), &[], None, 0).await?;
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
    assert!(!bounded(metrics, "ingested", 0, async || s.ingested(&order, 0).await).await?);
    assert_eq!(bounded(metrics, "fire_ingress", 1, async || s.fire(&firing, 0).await).await?, Fired::Committed);
    assert_eq!(bounded(metrics, "fire_ingested", 1, async || s.fire(&firing, 1).await).await?, Fired::Ingested);
    assert_eq!(bounded(metrics, "sweep_tombstones", 1, async || s.sweep(10, i64::MAX / 2).await).await?.tombstones, 1);
    // Exclusive firing with the previous run still unfinished.
    // Far in the future: the bounds below read the due-trigger index.
    let slot = |at: i64| Trigger { at_ms: Some(2_000_000_000 + at), types: vec![], state: json!({ "at": at }) };
    s.swap_trigger("decided#every", None, Some(&slot(1)), &[], None, 0).await?;
    let mut first = a.clone();
    first.id = "decided:slot1".into();
    let mut second = a.clone();
    second.id = "decided:slot2".into();
    let exclusive = async |expected: &Trigger, next: &Trigger, admit: &[ouro_durable::Admission]| {
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
        .await
    };
    let one = std::slice::from_ref(&first);
    let two = std::slice::from_ref(&second);
    assert_eq!(
        bounded(metrics, "fire_exclusive", 1, async || exclusive(&slot(1), &slot(2), one).await).await?,
        Fired::Committed
    );
    let busy = bounded(metrics, "fire_busy", 1, async || exclusive(&slot(2), &slot(3), two).await).await?;
    assert_eq!(busy, Fired::Busy(first.id.clone()));
    // Activation epochs: arm, re-arm, trip (removing the trigger and storing
    // the notice in the terminal commit), then hand the notice on.
    let v1 = s.save_definition("decided", "one").await?;
    let v2 = s.save_definition("decided", "two").await?;
    let arm = |keys: &[&str]| keys.iter().map(|k| ((*k).to_string(), slot(100))).collect();
    let arming = Arming { triggers: arm(&["decided#a", "decided#b"]) };
    let epoch = s.definitions("decided").await?.epoch;
    bounded(metrics, "activate_with", 2, async || s.activate_with("decided", v1.version, epoch, &arming).await)
        .await?
        .unwrap();
    let arming = Arming { triggers: arm(&["decided#c"]) };
    let epoch = s.definitions("decided").await?.epoch;
    bounded(metrics, "activate_rearm", 3, async || s.activate_with("decided", v2.version, epoch, &arming).await)
        .await?
        .unwrap();
    let mut faulty = a.clone();
    faulty.id = "decided:trip".into();
    faulty.version = v2.version;
    let f = s.start(&faulty, 0).await?.unwrap();
    let fault = Status::Faulted(WorkflowError::runtime("fault"));
    bounded(metrics, "finish_trip", 2, async || s.finish(&f, &fault, 0, 1).await).await?;
    let d = s.definitions("decided").await?;
    assert!(d.tripped.is_some() && d.active == Some(v2.version) && d.triggers.is_empty());
    let notices = bounded(metrics, "notices", 1, async || s.notices(8).await).await?;
    assert_eq!(notices.len(), 1);
    assert!(bounded(metrics, "acknowledge", 1, async || s.acknowledge(&notices[0].0).await).await?);
    // Tripped, it admits nothing, by a constant number of reads; the host's
    // activation admits again.
    let mut refused = a.clone();
    refused.id = "decided:refused".into();
    metrics.reset();
    assert!(s.admit(&refused, 0).await.unwrap_err().is::<ouro_durable::DefinitionTripped>());
    let counts = metrics.counts();
    assert!(counts.rows_read <= 32 && counts.rows_written == 0, "admit_tripped: {counts:?}");
    let epoch = s.definitions("decided").await?.epoch;
    s.activate_with("decided", v2.version, epoch, &Arming::default()).await?.unwrap();
    // A refused trigger run: the slot moves, the run is stored ended and the
    // breaker counts it, by a constant number of rows; at the threshold the
    // same commit trips, removes the trigger and stores the notice.
    let armed = Arming { triggers: BTreeMap::from([("decided#refused".to_string(), slot(200))]) };
    let epoch = s.definitions("decided").await?.epoch;
    s.activate_with("decided", v2.version, epoch, &armed).await?.unwrap();
    let refusal = |id: &str| Refused {
        admission: ouro_durable::Admission { id: id.into(), version: v2.version, ..a.clone() },
        error: WorkflowError::validation("invalid"),
        breaker_threshold: 2,
    };
    let refuse = async |at: i64, refused: &Refused| {
        let firing = Firing {
            key: "decided#refused",
            expected: Some(&slot(at)),
            next: Some(&slot(at + 1)),
            refused: Some(refused),
            busy: Some(&slot(at + 1)),
            ..Firing::default()
        };
        s.fire(&firing, 0).await
    };
    let first = refusal("decided:refused1");
    assert_eq!(bounded(metrics, "fire_refused", 1, async || refuse(200, &first).await).await?, Fired::Committed);
    let second = refusal("decided:refused2");
    assert_eq!(bounded(metrics, "fire_refused_trip", 2, async || refuse(201, &second).await).await?, Fired::Committed);
    let d = s.definitions("decided").await?;
    assert!(d.tripped.is_some_and(|t| t.run == second.admission.id) && d.triggers.is_empty());
    assert_eq!(s.get(&first.admission.id).await?.map(|r| r.state), Some(RunState::Failed));
    let epoch = d.epoch;
    s.activate_with("decided", v2.version, epoch, &Arming::default()).await?.unwrap();
    // Version-aware claims page through their own partitions only.
    let mut versioned = a.clone();
    versioned.id = "decided:versioned".into();
    versioned.runtime = json!({ "application_version": "2" });
    s.admit(&versioned, 0).await?;
    let worker = Compatibility { version: Some("2".into()), accepts: vec!["1".into()] };
    let due = bounded(metrics, "due_as", 8, async || s.due_as(0, 8, &worker).await).await?;
    assert!(due.contains(&versioned.id) && due.len() == 8);
    let other = Compatibility { version: Some("3".into()), accepts: vec![] };
    assert!(
        bounded(metrics, "claim_as_incompatible", 0, async || s.claim_as(&versioned.id, 0, &other).await)
            .await?
            .is_none()
    );
    assert!(bounded(metrics, "claim_as", 0, async || s.claim_as(&versioned.id, 0, &worker).await).await?.is_some());
    Ok(())
}

async fn workers(store: Arc<dyn RunStore>, reopen: ouro_durable::conformance::Reopen) -> anyhow::Result<()> {
    const RUNS: usize = 128;
    let d = store.save_definition("contention", "yaml").await?;
    store.activate("contention", d.version).await?;
    for n in 0..RUNS {
        let mut a = admission(&format!("load:{n:04}"));
        a.name = d.name.clone();
        a.version = d.version;
        store.admit(&a, 0).await?;
    }
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let reopen = reopen.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let s = reopen().await?;
                barrier.wait().await;
                let mut completed = 0;
                for n in 0..RUNS {
                    let id = format!("load:{n:04}");
                    let Some(old) = s.claim(&id, 0).await.map_err(|e| e.context(format!("initial claim of {id}")))?
                    else {
                        continue;
                    };
                    let now = ouro_durable::LEASE_MS + 1;
                    let f = s.claim(&id, now).await.map_err(|e| e.context(format!("recovery claim of {id}")))?.unwrap();
                    assert_eq!(f.generation, old.generation + 1);
                    assert!(s.save_record(&old, "stale", &Record::Timer { deadline_ms: 42 }, now).await.is_err());
                    assert!(s.renew(&old, now).await.is_err());
                    assert!(s.finish(&old, &Status::Completed(json!(0)), now, 3).await.is_err());
                    for k in ["one", "two", "three"] {
                        s.save_record(&f, k, &Record::Timer { deadline_ms: 42 }, now)
                            .await
                            .map_err(|e| e.context(format!("save {id}/{k}")))?;
                    }
                    let types = vec![format!("event:{id}")];
                    s.register_wait(&f, &types, now).await.map_err(|e| e.context(format!("register {id}")))?;
                    assert_eq!(s.deliver(&event(&id, &types[0], json!(n)), None, now).await?, Some(id.clone()));
                    assert_eq!(s.take_event(&f, "event", &types, now).await?.unwrap()["data"], json!(n));
                    s.finish(&f, &Status::Faulted(WorkflowError::runtime("counted")), now, u32::MAX)
                        .await
                        .map_err(|e| e.context(format!("finish {id}")))?;
                    completed += 1;
                }
                anyhow::Ok(completed)
            })
        })
        .collect();
    let mut completed = 0;
    for worker in threads {
        completed += worker.await.expect("load worker panicked")?;
    }
    assert_eq!(completed, RUNS);
    assert_eq!(usize::try_from(store.definitions("contention").await?.consecutive_faults)?, RUNS);
    for r in store.list(Some("contention"), "load:").await? {
        assert_eq!(r.state, RunState::Failed);
        assert_eq!(r.generation, 2);
        let saved = store.inspect(&r.admission.id).await?.unwrap();
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
#[async_trait::async_trait]
impl AtomicBackend for GatedBackend {
    async fn begin(&self) -> anyhow::Result<Box<dyn ouro_durable::Transaction + '_>> {
        Ok(Box::new(GatedTransaction { inner: self.inner.begin().await?, gate: self }))
    }
    async fn snapshot(&self) -> anyhow::Result<ouro_durable::Snapshot> {
        self.inner.snapshot().await
    }
}
#[async_trait::async_trait]
impl ouro_durable::Transaction for GatedTransaction<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let result = self.inner.get(kind, id).await?;
        if kind == "run" && id == "race" && !self.gate.first.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.gate.gate.wait().await;
        }
        Ok(result)
    }
    async fn scan(
        &mut self,
        kind: &str,
        start: &str,
        end: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<ouro_durable::Rows> {
        self.inner.scan(kind, start, end, limit).await
    }
    async fn put(&mut self, kind: &str, id: &str, value: &serde_json::Value) -> anyhow::Result<()> {
        self.inner.put(kind, id, value).await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.inner.delete(kind, id).await
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.inner.commit().await
    }
}
async fn retry_race(
    backend: Arc<dyn AtomicBackend>,
    reopen: impl AsyncFn() -> anyhow::Result<Arc<dyn AtomicBackend>>,
    metrics: impl Fn() -> IoCounts,
) -> anyhow::Result<()> {
    let s = TransactionalStore::new(backend);
    s.admit(&admission("race"), 0).await?;
    let before = metrics().conflicts;
    let gate = Arc::new(Barrier::new(2));
    let mut stores = Vec::new();
    for _ in 0..2 {
        stores.push(TransactionalStore::new(Arc::new(GatedBackend {
            inner: reopen().await?,
            gate: gate.clone(),
            first: std::sync::atomic::AtomicBool::new(false),
        })));
    }
    let threads: Vec<_> = stores.into_iter().map(|s| tokio::spawn(async move { s.claim("race", 0).await })).collect();
    let mut fences: Vec<Fence> = Vec::new();
    for t in threads {
        fences.extend(t.await.unwrap()?);
    }
    assert_eq!(fences.len(), 1);
    let conflicts = metrics().conflicts - before;
    assert!(conflicts > 0, "the forced race must be retried");
    eprintln!("RETRY_RACE,winners=1,conflicts={conflicts}");
    Ok(())
}

async fn ordering(store: &dyn RunStore) -> anyhow::Result<()> {
    for (id, at) in [("ordering:later", -20), ("ordering:earlier", -100)] {
        store.admit(&admission(id), at).await?;
    }
    assert_eq!(store.due(-21, 10).await?, vec!["ordering:earlier"]);
    assert_eq!(store.due(-10, 2).await?, vec!["ordering:earlier", "ordering:later"]);
    for id in ["ordering:earlier", "ordering:later"] {
        store.cancel(id, 0).await?;
    }
    let name = "colon:name/名";
    let mut expected = vec!["unicode:é", "unicode:😀", "unicode:\u{e000}", "unicode:\u{d7ff}", "unicode:\u{10ffff}"];
    for id in &expected {
        let mut a = admission(id);
        a.name = name.into();
        store.admit(&a, 0).await?;
    }
    expected.sort();
    assert_eq!(
        store.list(Some(name), "unicode:").await?.iter().map(|r| r.admission.id.as_str()).collect::<Vec<_>>(),
        expected
    );
    for prefix in ["unicode:\u{d7ff}", "unicode:\u{10ffff}"] {
        assert_eq!(store.list(None, prefix).await?.len(), 1);
        assert_eq!(store.list(Some(name), prefix).await?.len(), 1);
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
async fn postgres_query_plans(config: &tokio_postgres::Config) -> anyhow::Result<()> {
    {
        let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
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
    }
}

async fn index_atomicity(
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
        let before = backend.snapshot().await?;
        assert!(backend.compare_exchange(&before, &ouro_durable::Rows::new()).await?);
        let p = s.start(&admission("p"), 0).await?.unwrap();
        s.register_wait(&p, &["answer".into(), "deliverable".into()], 0).await?;
        s.take_event(&p, "event", &["answer".into()], 0).await?;
        s.deliver(&event("42", "answer", json!(42)), None, 0).await?;
        let c = s
            .start_child(
                &p,
                &admission("c"),
                &ouro_durable::ChildLink { parent: "p".into(), key: "child".into(), reference: "/child".into() },
                0,
            )
            .await?
            .unwrap();
        s.save_record(&c, "timer", &Record::Timer { deadline_ms: 42 }, 0).await?;
        let first = s.save_definition("workflow", "first").await?;
        s.activate("workflow", first.version).await?;
        let second = s.save_definition("workflow", "second").await?;
        let epoch = s.definitions("workflow").await?.epoch;
        if operation == "sweep" {
            s.finish(&p, &Status::Completed(json!(42)), 0, 3).await?;
        }
        // A run whose fault trips the breaker of an activation with a trigger.
        let mut tripping = None;
        if operation == "trip" {
            let trigger = Trigger { at_ms: Some(5), types: vec!["t".into()], state: json!(1) };
            let arming = Arming { triggers: BTreeMap::from([("armed".to_string(), trigger)]) };
            s.activate_with("workflow", second.version, epoch, &arming).await?;
            let mut a = admission("tripping");
            a.version = second.version;
            tripping = s.start(&a, 0).await?;
        }
        let action = async || -> anyhow::Result<()> {
            match operation {
                "start" => s.start(&admission("new"), 0).await.map(|_| ()),
                "child" => s
                    .start_child(
                        &p,
                        &admission("new"),
                        &ouro_durable::ChildLink { parent: "p".into(), key: "new".into(), reference: "/new".into() },
                        0,
                    )
                    .await
                    .map(|_| ()),
                "finish" => s.finish(&c, &Status::Completed(json!(42)), 0, 3).await.map(|_| ()),
                "cancel" => s.cancel("p", 0).await.map(|_| ()),
                "deliver" => s.deliver(&event("deliverable", "deliverable", json!(42)), None, 0).await.map(|_| ()),
                "take" => s.take_event(&p, "event", &["answer".into()], 0).await.map(|_| ()),
                "sweep" => s.sweep(1, 0).await.map(|_| ()),
                "release" => s.release(&p, 0).await,
                "renew" => s.renew(&p, 1).await,
                "record" => s.save_record(&p, "timer", &Record::Timer { deadline_ms: 42 }, 0).await,
                "register" => s.register_wait(&p, &["new".into()], 0).await,
                "unregister" => s.unregister_wait(&p, "event", &["answer".into()], 0).await,
                "definition" => s.save_definition("workflow", "third").await.map(|_| ()),
                "activate" => s.activate("workflow", second.version).await,
                "subscribe" => s.subscribe(&p, "listen", &["answer".into()], 0).await,
                "publish" => {
                    s.publish(&json!({"id":"1","source":"/s","type":"published"}), Some("p"), 0).await.map(|_| ())
                }
                "mailbox" => s.deliver(&event("early", "early", json!(1)), Some("p"), 0).await.map(|_| ()),
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
                    .await
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
                    .await
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
                    .await
                    .map(|_| ()),
                "trip" => {
                    let fault = Status::Faulted(WorkflowError::runtime("fault"));
                    s.finish(tripping.as_ref().unwrap(), &fault, 0, 1).await?;
                    anyhow::ensure!(s.definitions("workflow").await?.tripped.is_some(), "the fault did not trip");
                    Ok(())
                }
                _ => unreachable!(),
            }
        };
        let before = backend.snapshot().await?;
        metrics.reset();
        action().await?;
        let writes = usize::try_from(metrics.counts().rows_written)?;
        assert!(writes > 0);
        let after = backend.snapshot().await?;
        assert!(backend.compare_exchange(&after, &before.rows).await?);
        for fault in
            (0..writes).map(CommitFault::BeforeWrite).chain([CommitFault::BeforeCommit, CommitFault::AfterCommit])
        {
            faults.arm(fault);
            assert!(action().await.is_err(), "{operation} at {fault:?}");
            faults.clear();
            let actual = backend.snapshot().await?;
            if fault == CommitFault::AfterCommit {
                assert_ne!(actual, before, "ambiguous commit must remain visible");
                assert!(backend.compare_exchange(&actual, &before.rows).await?);
            } else {
                assert_eq!(actual, before, "primary/index partial write: {operation} at {fault:?}");
            }
            checked += 1;
        }
    }
    let before = backend.snapshot().await?;
    assert!(backend.compare_exchange(&before, &ouro_durable::Rows::new()).await?);
    eprintln!("INDEX_ATOMICITY,checked_faults={checked}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn memory_scoped_bounds_and_concurrency() {
    let b = Arc::new(MemoryBackend::default());
    index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone()).await.unwrap();
    bounds(b.clone(), b.metrics.clone(), 2).await.unwrap();
    ordering(&TransactionalStore::new(b.clone())).await.unwrap();
    workers(
        Arc::new(TransactionalStore::new(b.clone())),
        Arc::new({
            let b = b.clone();
            move || {
                let store: Arc<dyn RunStore> = Arc::new(TransactionalStore::new(b.clone()));
                Box::pin(async move { Ok(store) })
            }
        }),
    )
    .await
    .unwrap();
    let backend: Arc<dyn AtomicBackend> = b.clone();
    retry_race(b.clone(), async || Ok(backend.clone()), || b.metrics.counts()).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sqlite_scoped_bounds_and_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.sqlite");
    let b = Arc::new(SqliteBackend::open(&path).unwrap());
    index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone()).await.unwrap();
    bounds(b.clone(), b.metrics.clone(), 1).await.unwrap();
    ordering(&TransactionalStore::new(b.clone())).await.unwrap();
    sqlite_query_plans(&path).unwrap();
    let reopen = path.clone();
    workers(
        Arc::new(TransactionalStore::new(b)),
        Arc::new(move || {
            let path = reopen.clone();
            Box::pin(async move { Ok(Arc::new(ouro_durable::sqlite::open(&path)?) as Arc<dyn RunStore>) })
        }),
    )
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn postgres_scoped_bounds_and_concurrency() {
    support::postgres(async |config| {
        let b = Arc::new(PostgresBackend::connect_config(config.clone()).await?);
        index_atomicity(b.clone(), b.metrics.clone(), b.faults.clone()).await?;
        bounds(b.clone(), b.metrics.clone(), 1).await?;
        ordering(&TransactionalStore::new(b.clone())).await?;
        postgres_query_plans(&config).await?;
        workers(
            Arc::new(TransactionalStore::new(b.clone())),
            Arc::new({
                let config = config.clone();
                move || {
                    let config = config.clone();
                    Box::pin(async move {
                        let backend = PostgresBackend::connect_config(config).await?;
                        Ok(Arc::new(TransactionalStore::new(Arc::new(backend))) as Arc<dyn RunStore>)
                    })
                }
            }),
        )
        .await?;
        let mut peers = Vec::new();
        for _ in 0..2 {
            peers.push(Arc::new(PostgresBackend::connect_config(config.clone()).await?));
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        retry_race(
            b,
            async || Ok(peers[next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)].clone() as Arc<dyn AtomicBackend>),
            || IoCounts { conflicts: peers.iter().map(|b| b.metrics.counts().conflicts).sum(), ..IoCounts::default() },
        )
        .await
    })
    .await;
}

#[tokio::test]
async fn memory_transactions_validate_absence_ranges_and_independent_keys() {
    let b = MemoryBackend::default();
    let mut a = b.begin().await.unwrap();
    let mut c = b.begin().await.unwrap();
    a.put("test", "one", &json!(1)).await.unwrap();
    c.put("test", "two", &json!(2)).await.unwrap();
    a.commit().await.unwrap();
    c.commit().await.unwrap();
    let mut a = b.begin().await.unwrap();
    let mut c = b.begin().await.unwrap();
    assert!(a.get("test", "missing").await.unwrap().is_none());
    c.put("test", "missing", &json!(3)).await.unwrap();
    c.commit().await.unwrap();
    assert!(a.commit().await.unwrap_err().is::<Conflict>());
    let mut a = b.begin().await.unwrap();
    let mut c = b.begin().await.unwrap();
    assert!(a.scan("empty", "", None, 1).await.unwrap().is_empty());
    c.put("empty", "phantom", &json!(4)).await.unwrap();
    c.commit().await.unwrap();
    assert!(a.commit().await.unwrap_err().is::<Conflict>());
    // Point/range validation may share a version check only when both
    // observations saw the same version. Intervening writes still abort.
    let mut a = b.begin().await.unwrap();
    a.get("test", "one").await.unwrap();
    a.scan("test", "one", Some("two"), 1).await.unwrap();
    let mut c = b.begin().await.unwrap();
    c.put("test", "one", &json!(10)).await.unwrap();
    c.commit().await.unwrap();
    assert!(a.commit().await.unwrap_err().is::<Conflict>());
    let mut a = b.begin().await.unwrap();
    a.get("test", "one").await.unwrap();
    let mut c = b.begin().await.unwrap();
    c.put("test", "one", &json!(11)).await.unwrap();
    c.commit().await.unwrap();
    a.scan("test", "one", Some("two"), 1).await.unwrap();
    assert!(a.commit().await.unwrap_err().is::<Conflict>());
    let mut a = b.begin().await.unwrap();
    let mut c = b.begin().await.unwrap();
    a.scan("test", "", None, 1).await.unwrap();
    c.put("test", "0", &json!(0)).await.unwrap();
    c.commit().await.unwrap();
    assert!(a.commit().await.unwrap_err().is::<Conflict>());
}

async fn control_bounds(backend: &Arc<dyn AtomicBackend>, metrics: &Metrics) -> anyhow::Result<()> {
    use ouro_durable::{
        conformance::control_wait,
        control::{Command, Request, Runs, Waits},
    };
    let s = TransactionalStore::new(backend.clone());
    let now = 2_000_000;
    let mut a = admission("operator:root");
    a.name = "operator".into();
    s.save_definition("operator", "yaml").await?;
    s.activate("operator", 1).await?;
    let f = s.start(&a, now).await?.unwrap();
    bounded(metrics, "control_check_lease", 0, async || s.check_lease(&f, now).await).await?;
    let mut child = a.clone();
    child.id = "operator:child".into();
    s.start_child(
        &f,
        &child,
        &ouro_durable::ChildLink { parent: a.id.clone(), key: "child".into(), reference: "child".into() },
        now,
    )
    .await?;
    let req = |id: &str| Request { id: id.into(), actor: "operator".into(), reason: "row bounds".into() };
    let mut w = control_wait();
    w.since_ms = now;
    w.timeout_ms = Some(now + 100);
    bounded(metrics, "control_describe_wait", 2, async || s.describe_wait(&f, "listen", &w, now).await).await?;
    bounded(metrics, "control_describe_wait_retry", 2, async || s.describe_wait(&f, "listen", &w, now).await).await?;
    let filter = Runs { definition: Some("operator".into()), ..Runs::default() };
    for (label, f) in [
        ("control_list_definition", filter.clone()),
        ("control_list_state", Runs { state: Some(RunState::Running), ..filter.clone() }),
        ("control_list_parent", Runs { parent: Some(a.id.clone()), ..filter.clone() }),
        ("control_list_age", Runs { since_ms: Some(now), before_ms: Some(now + 1), ..Runs::default() }),
    ] {
        assert!(!bounded(metrics, label, 2, async || s.query_runs(&f, false, None, 2).await).await?.items.is_empty());
    }
    let page = s.query_runs(&filter, false, None, 1).await?;
    bounded(metrics, "control_list_cursor", 1, async || s.query_runs(&filter, false, page.next.as_ref(), 1).await)
        .await?;
    for (label, f) in [
        ("control_waits_definition", Waits { definition: Some("operator".into()), ..Waits::default() }),
        ("control_waits_type", Waits { event_type: Some("answer".into()), ..Waits::default() }),
        ("control_waits_run", Waits { run: Some(a.id.clone()), ..Waits::default() }),
        ("control_waits_age", Waits { since_ms: Some(now), before_ms: Some(now + 1), ..Waits::default() }),
        (
            "control_waits_definition_type",
            Waits { definition: Some("operator".into()), event_type: Some("answer".into()), ..Waits::default() },
        ),
    ] {
        assert_eq!(bounded(metrics, label, 1, async || s.pending_waits(&f, None, 2).await).await?.items.len(), 1);
    }
    let page = s.pending_waits(&Waits::default(), None, 1).await?;
    bounded(metrics, "control_waits_cursor", 0, async || {
        s.pending_waits(&Waits::default(), page.next.as_ref(), 1).await
    })
    .await?;
    bounded(metrics, "control_inspect", 3, async || s.inspect_control(&a.id).await).await?;
    let pause = Command::Pause { definition: "operator".into(), note: "pause".into() };
    bounded(metrics, "control_pause", 1, async || s.control(&req("pause"), &pause, now).await).await?;
    bounded(metrics, "control_pause_retry", 0, async || s.control(&req("pause"), &pause, now).await).await?;
    bounded(metrics, "control_resume", 1, async || {
        s.control(&req("resume"), &Command::Resume { definition: "operator".into() }, now).await
    })
    .await?;
    bounded(metrics, "control_stop", 0, async || s.control(&req("stop"), &Command::Stop { enabled: true }, now).await)
        .await?;
    assert!(bounded(metrics, "control_stopped", 0, async || s.stopped().await).await?);
    assert!(bounded(metrics, "control_claim_stopped", 0, async || s.claim(&a.id, i64::MAX).await).await?.is_none());
    bounded(metrics, "control_clear_stop", 0, async || {
        s.control(&req("clear"), &Command::Stop { enabled: false }, now).await
    })
    .await?;
    let mut event = event("response", "answer", json!({"approved":true}));
    event["ticket"] = json!(42);
    let respond = Command::Respond { run: a.id.clone(), wait: "listen".into(), event };
    bounded(metrics, "control_respond", 2, async || s.control(&req("respond"), &respond, now).await).await?;
    bounded(metrics, "control_respond_retry", 0, async || s.control(&req("respond"), &respond, now).await).await?;
    bounded(metrics, "control_respond_identity", 0, async || s.control(&req("respond-identity"), &respond, now).await)
        .await?;
    bounded(metrics, "control_respond_take", 2, async || s.take_event(&f, &w.position, &w.types, now).await).await?;
    bounded(metrics, "control_cancel_tree", 3, async || {
        s.control(&req("cancel"), &Command::Cancel { run: a.id.clone() }, now + 1).await
    })
    .await?;
    bounded(metrics, "control_cancel_retry", 0, async || {
        s.control(&req("cancel"), &Command::Cancel { run: a.id.clone() }, now + 1).await
    })
    .await?;
    bounded(metrics, "control_entry", 0, async || s.control_entry("cancel").await).await?;
    bounded(metrics, "control_journal_run", 2, async || s.control_history(Some(&a.id), None, 2).await).await?;
    let journal = bounded(metrics, "control_journal", 2, async || s.control_history(None, None, 2).await).await?;
    bounded(metrics, "control_journal_cursor", 2, async || s.control_history(None, journal.next.as_ref(), 2).await)
        .await?;
    assert_eq!(
        bounded(metrics, "control_history_definition", 2, async || s.query_runs(&filter, true, None, 2).await)
            .await?
            .items
            .len(),
        2
    );
    let recent = Runs { since_ms: Some(now), before_ms: Some(now + 2), ..Runs::default() };
    assert_eq!(
        bounded(metrics, "control_history_window", 2, async || s.query_runs(&recent, true, None, 2).await)
            .await?
            .items
            .len(),
        2
    );
    let mut a = a.clone();
    a.id = "operator:wildcard".into();
    let f = s.start(&a, now).await?.unwrap();
    let mut w = control_wait();
    w.types = vec!["*".into()];
    w.strategy = ows::model::Strategy::Any { filters: Vec::new(), until: None };
    w.progress = ows::events::Consumer::new(&w.strategy).progress();
    w.since_ms = now;
    w.timeout_ms = None;
    bounded(metrics, "control_describe_wildcard", 1, async || s.describe_wait(&f, "listen", &w, now).await).await?;
    let respond = Command::Respond {
        run: a.id.clone(),
        wait: "listen".into(),
        event: ouro_durable::conformance::event("wildcard", "choice", json!(42)),
    };
    bounded(metrics, "control_respond_wildcard", 2, async || s.control(&req("wildcard"), &respond, now).await).await?;
    bounded(metrics, "control_take_wildcard", 2, async || s.take_event(&f, &w.position, &w.types, now).await).await?;
    s.finish(&f, &Status::Completed(json!(42)), now, 3).await?;
    for pause in [false, true] {
        let name = if pause { "operatorpausequeue" } else { "operatorstopqueue" };
        s.save_definition(name, "yaml").await?;
        let slot = |n| Trigger { at_ms: Some(now + n), types: vec![], state: json!(n) };
        s.activate_with(name, 1, 0, &Arming { triggers: BTreeMap::from([(name.into(), slot(0))]) }).await?;
        let mut first = a.clone();
        first.id = format!("{name}:first");
        first.name = name.into();
        let mut buffered = first.clone();
        buffered.id = format!("{name}:buffered");
        let fire = async |n, a: &ouro_durable::Admission| {
            s.fire(
                &Firing {
                    key: name,
                    expected: Some(&slot(n)),
                    next: Some(&slot(n + 1)),
                    busy: Some(&slot(n + 1)),
                    admit: std::slice::from_ref(a),
                    overlap: Some(ouro_durable::OverlapGuard {
                        policy: ouro_durable::Overlap::Queue,
                        max_concurrent_runs: 2,
                        ceiling: &slot(n + 1),
                    }),
                    ..Firing::default()
                },
                now,
            )
            .await
        };
        assert_eq!(fire(0, &first).await?, Fired::Committed);
        assert_eq!(fire(1, &buffered).await?, Fired::Queued);
        let stop = if pause {
            Command::Pause { definition: name.into(), note: "retain queue".into() }
        } else {
            Command::Stop { enabled: true }
        };
        bounded(metrics, if pause { "control_pause_queue" } else { "control_stop_queue" }, 2, async || {
            s.control(&req(&format!("{name}:stop")), &stop, now).await
        })
        .await?;
        bounded(metrics, "control_cancel_deferred_queue", 2, async || {
            s.control(&req(&format!("{name}:cancel")), &Command::Cancel { run: first.id.clone() }, now).await
        })
        .await?;
        assert!(s.get(&buffered.id).await?.is_none());
        let resume = if pause { Command::Resume { definition: name.into() } } else { Command::Stop { enabled: false } };
        bounded(metrics, if pause { "control_resume_queue" } else { "control_clear_deferred_queue" }, 3, async || {
            s.control(&req(&format!("{name}:clear")), &resume, now).await
        })
        .await?;
        assert_eq!(s.get(&buffered.id).await?.unwrap().admission, buffered);
        s.cancel(&buffered.id, now).await?;
    }
    Ok(())
}
