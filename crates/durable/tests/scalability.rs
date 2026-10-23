use ouro_durable::{
    AtomicBackend, Conflict, Fence, IoCounts, Metrics, RunState, RunStore, TransactionalStore, conformance::admission,
    memory::MemoryBackend, postgres::PostgresBackend, sqlite::SqliteBackend,
};
use owf::{
    durable::Record,
    run::{Status, WorkflowError},
};
use serde_json::json;
use std::sync::{Arc, Barrier};
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
fn bounds(backend: Arc<dyn AtomicBackend>, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    support::seed(&backend, 10_000)?;
    let s = TransactionalStore::new(backend);
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
        s.deliver("crowded", &json!(n), 1)?.unwrap();
        s.finish(&f, &Status::Waiting { wake_at_ms: i64::MAX }, 1, 3)?;
    }
    assert!(bounded(&metrics, "deliver_saturated", 0, || s.deliver("crowded", &json!(0), 1))?.is_none());
    let types = vec!["answer".into()];
    bounded(&metrics, "register_wait", 0, || s.register_wait(&f, &types, 1))?;
    assert!(bounded(&metrics, "take_event_pending", 0, || s.take_event(&f, "answer", &types, 1))?.is_none());
    assert_eq!(bounded(&metrics, "deliver", 1, || s.deliver("answer", &json!(42), 1))?, Some(a.id.clone()));
    assert!(bounded(&metrics, "deliver_full", 0, || s.deliver("answer", &json!(42), 1))?.is_none());
    assert!(bounded(&metrics, "deliver_absent", 0, || s.deliver("absent", &json!(42), 1))?.is_none());
    assert!(bounded(&metrics, "take_event", 0, || s.take_event(&f, "answer", &types, 1))?.is_some());
    bounded(&metrics, "take_event_replay", 0, || s.take_event(&f, "answer", &types, 1))?;
    bounded(&metrics, "unregister_wait", 0, || s.unregister_wait(&f, "answer", &types, 1))?;
    assert_eq!(bounded(&metrics, "load_records", 2, || s.load_records(&f, 1))?.len(), 2);
    bounded(&metrics, "finish_waiting", 0, || s.finish(&f, &Status::Waiting { wake_at_ms: 1 }, 1, 3))?;
    let f = bounded(&metrics, "claim_waiting", 0, || s.claim(&a.id, 1))?.unwrap();
    bounded(&metrics, "release", 0, || s.release(&f, 1))?;
    let f = s.claim(&a.id, 1)?.unwrap();
    for id in ["target:c", "target:live"] {
        let mut child = a.clone();
        child.id = id.into();
        let link = ouro_durable::ChildLink { parent: a.id.clone(), key: id.into(), reference: "/child".into() };
        let c = bounded(&metrics, "start_child", 1, || s.start_child(&f, &child, &link, 1))?.unwrap();
        assert!(bounded(&metrics, "child_outcome", 0, || s.child_outcome(&a.id, id, id))?.is_none());
        if id == "target:c" {
            bounded(&metrics, "finish_child", 1, || s.finish(&c, &Status::Completed(json!(42)), 1, 3))?;
        }
    }
    assert!(bounded(&metrics, "cancel_cascade", 1, || s.cancel(&a.id, 1))?);
    bounded(&metrics, "finish", 0, || s.finish(&admitted, &Status::Completed(json!(42)), 1, 3))?;
    assert!(bounded(&metrics, "get", 0, || s.get(&a.id))?.is_some());
    assert!(bounded(&metrics, "get_absent", 0, || s.get("missing"))?.is_none());
    bounded(&metrics, "inspect", 2, || s.inspect(&a.id))?;
    assert_eq!(bounded(&metrics, "list_name", 4, || s.list(Some("target"), "target:"))?.len(), 4);
    assert_eq!(bounded(&metrics, "list_prefix", 4, || s.list(None, "target:"))?.len(), 4);
    assert_eq!(bounded(&metrics, "due_page", 7, || s.due(1, 7))?.len(), 7);
    assert!(bounded(&metrics, "due_empty", 0, || s.due(-1, 7))?.is_empty());
    assert!(bounded(&metrics, "due_zero_limit", 0, || s.due(i64::MAX, 0))?.is_empty());
    bounded(&metrics, "sweep", 6, || s.sweep(1, 0))?;
    assert_eq!(bounded(&metrics, "sweep_already_swept", 0, || s.sweep(1, 0))?, 0);
    assert_eq!(s.list(None, "seed").unwrap().len(), 10_000);
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
                    assert_eq!(s.deliver(&types[0], &json!(n), now)?, Some(id.clone()));
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
fn sqlite_query_plans(path: &std::path::Path) -> anyhow::Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    for kind in ["due", "wait:616e73776572", "children:70", "name:746172676574", "finished"] {
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
            for kind in ["due","wait:616e73776572","children:70","name:746172676574","finished"] {
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
    ] {
        let before = backend.snapshot()?;
        assert!(backend.compare_exchange(&before, &ouro_durable::Rows::new())?);
        let p = s.start(&admission("p"), 0)?.unwrap();
        s.register_wait(&p, &["answer".into(), "deliverable".into()], 0)?;
        s.take_event(&p, "event", &["answer".into()], 0)?;
        s.deliver("answer", &json!(42), 0)?;
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
        if operation == "sweep" {
            s.finish(&p, &Status::Completed(json!(42)), 0, 3)?;
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
                "deliver" => s.deliver("deliverable", &json!(42), 0).map(|_| ()),
                "take" => s.take_event(&p, "event", &["answer".into()], 0).map(|_| ()),
                "sweep" => s.sweep(1, 0).map(|_| ()),
                "release" => s.release(&p, 0),
                "renew" => s.renew(&p, 1),
                "record" => s.save_record(&p, "timer", &Record::Timer { deadline_ms: 42 }, 0),
                "register" => s.register_wait(&p, &["new".into()], 0),
                "unregister" => s.unregister_wait(&p, "event", &["answer".into()], 0),
                "definition" => s.save_definition("workflow", "third").map(|_| ()),
                "activate" => s.activate("workflow", second.version),
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
    bounds(b.clone(), b.metrics.clone()).unwrap();
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
    bounds(b.clone(), b.metrics.clone()).unwrap();
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
        bounds(b.clone(), b.metrics.clone())?;
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
