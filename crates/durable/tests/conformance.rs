use ouro_durable::{
    AtomicBackend, Snapshot, TransactionalStore,
    conformance::{self, TestStore},
    memory::MemoryBackend,
    postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use std::sync::Arc;

#[test]
fn memory_conformance() {
    let report = conformance::run_all(|_| {
        let backend = Arc::new(MemoryBackend::default());
        let reopen = backend.clone();
        Ok(TestStore {
            store: Arc::new(TransactionalStore::new(backend.clone())),
            backend: backend.clone(),
            faults: backend.faults.clone(),
            reopen: Arc::new(move || Ok(Arc::new(TransactionalStore::new(reopen.clone())))),
        })
    })
    .unwrap();
    eprintln!("memory: {report:?}");
}
#[test]
fn sqlite_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let count = std::sync::atomic::AtomicUsize::new(0);
    let report = conformance::run_all(|_| {
        let path = dir.path().join(format!("case{}.sqlite", count.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let backend = Arc::new(SqliteBackend::open(&path)?);
        let shared_faults = backend.faults.clone();
        Ok(TestStore {
            store: Arc::new(TransactionalStore::new(backend.clone())),
            backend: backend.clone(),
            faults: backend.faults.clone(),
            reopen: Arc::new(move || {
                Ok(Arc::new(TransactionalStore::new(Arc::new({
                    let mut b = SqliteBackend::open(&path)?;
                    b.faults = shared_faults.clone();
                    b
                }))))
            }),
        })
    })
    .unwrap();
    eprintln!("sqlite: {report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_conformance() {
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };
    let container;
    let config = if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
        url.parse::<tokio_postgres::Config>().unwrap()
    } else {
        container = GenericImage::new("postgres", "16")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
            .with_env_var("POSTGRES_PASSWORD", "local-test-password")
            .start()
            .await
            .expect("Postgres testcontainer must be reachable; alternatively set TEST_DATABASE_URL");
        let mut c = tokio_postgres::Config::new();
        c.host(container.get_host().await.unwrap().to_string());
        c.port(container.get_host_port_ipv4(5432).await.unwrap());
        c.user("postgres").password("local-test-password").dbname("postgres");
        c
    };
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (admin, connection) = rt.block_on(config.connect(tokio_postgres::NoTls)).unwrap();
        rt.spawn(async move {
            let _ = connection.await;
        });
        let databases = std::sync::Mutex::new(Vec::new());
        let result = conformance::run_all(|_| {
            let name = format!("ouro_test_{}", uuid::Uuid::new_v4().simple());
            rt.block_on(admin.batch_execute(&format!("CREATE DATABASE {name}")))?;
            databases.lock().unwrap().push(name.clone());
            let mut c = config.clone();
            c.dbname(&name);
            let reopen = c.clone();
            let backend = Arc::new(PostgresBackend::connect_config(c)?);
            let shared_faults = backend.faults.clone();
            Ok(TestStore {
                store: Arc::new(TransactionalStore::new(backend.clone())),
                backend: backend.clone(),
                faults: backend.faults.clone(),
                reopen: Arc::new(move || {
                    Ok(Arc::new(TransactionalStore::new(Arc::new(PostgresBackend::connect_with_faults(
                        reopen.clone(),
                        shared_faults.clone(),
                    )?))))
                }),
            })
        });
        for name in databases.into_inner().unwrap() {
            rt.block_on(admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))).unwrap();
        }
        eprintln!("postgres: {:?}", result.unwrap());
    })
    .await
    .unwrap();
}

#[test]
fn sqlite_corruption_stops_recovery_without_erasing_evidence() {
    use ouro_durable::RunStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sqlite");
    let backend = Arc::new(SqliteBackend::open(&path).unwrap());
    let s = TransactionalStore::new(backend.clone());
    s.start(&conformance::admission("r"), 0).unwrap();
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute("UPDATE ouro_rows SET value='broken json' WHERE kind='run'", []).unwrap();
    assert!(backend.snapshot().is_err());
    assert!(s.claim("r", 100_000).is_err());
    assert_eq!(
        c.query_row("SELECT value FROM ouro_rows WHERE kind='run'", [], |r| { r.get::<_, String>(0) }).unwrap(),
        "broken json"
    );
}

#[test]
fn concurrent_claims_and_cas_writes_have_one_winner() {
    use ouro_durable::RunStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.sqlite");
    ouro_durable::sqlite::open(&path).unwrap().admit(&conformance::admission("r"), 0).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let s = ouro_durable::sqlite::open(path).unwrap();
                barrier.wait();
                s.claim("r", 0).unwrap().is_some()
            })
        })
        .collect();
    assert_eq!(workers.into_iter().map(|w| usize::from(w.join().unwrap())).sum::<usize>(), 1);
}

#[test]
fn run_rows_of_another_state_version_are_refused() {
    use ouro_durable::RunStore;
    let backend = Arc::new(MemoryBackend::default());
    let s = TransactionalStore::new(backend.clone());
    s.start(&conformance::admission("r"), 0).unwrap();
    let key = ("run".to_string(), "r".to_string());
    // 3 and earlier: one aggregate per run; 4 and 5: call records with an effect.
    for version in [None, Some(3), Some(4), Some(5), Some(6)] {
        let before = backend.snapshot().unwrap();
        let mut rows = before.rows.clone();
        let run = rows.get_mut(&key).unwrap().as_object_mut().unwrap();
        // The earlier layout: one aggregate holding the records, no version.
        run.insert("records".into(), serde_json::json!({ "wait": { "kind": "timer", "deadline_ms": 1 } }));
        match version {
            Some(v) => run.insert("version".into(), serde_json::json!(v)),
            None => run.remove("version"),
        };
        assert!(backend.compare_exchange(&before, &rows).unwrap());
        let refused = s.get("r").unwrap_err().to_string();
        assert!(refused.contains("unsupported durable state version"), "{refused}");
        assert!(s.claim("r", ouro_durable::LEASE_MS + 1).is_err());
        assert!(s.inspect("r").is_err() && s.list(None, "").is_err() && s.cancel("r", 0).is_err());
        assert_eq!(backend.snapshot().unwrap(), Snapshot { rows }, "a refused row is left as it is");
        let current = backend.snapshot().unwrap();
        assert!(backend.compare_exchange(&current, &before.rows).unwrap());
    }
}

#[test]
fn definitions_rows_of_an_earlier_layout_are_refused() {
    use ouro_durable::RunStore;
    let backend = Arc::new(MemoryBackend::default());
    let s = TransactionalStore::new(backend.clone());
    let d = s.save_definition("workflow", "yaml").unwrap();
    s.activate("workflow", d.version).unwrap();
    let key = ("definitions".to_string(), "workflow".to_string());
    let before = backend.snapshot().unwrap();
    let mut rows = before.rows.clone();
    let row = rows.get_mut(&key).unwrap().as_object_mut().unwrap();
    // The earlier layout: a predecessor and its triggers kept for a breaker
    // rollback, a boolean trip, no version.
    row.remove("version");
    row.insert("previous".into(), serde_json::json!(null));
    row.insert("fallback".into(), serde_json::json!({}));
    row.insert("tripped".into(), serde_json::json!(true));
    assert!(backend.compare_exchange(&before, &rows).unwrap());
    let refused = s.definitions("workflow").unwrap_err().to_string();
    assert!(refused.contains("unsupported layout"), "{refused}");
    assert!(s.activate("workflow", d.version).is_err() && s.save_definition("workflow", "next").is_err());
    assert!(s.admit(&conformance::admission("r"), 0).is_err(), "admission reads the definition's epoch");
    assert_eq!(backend.snapshot().unwrap(), Snapshot { rows }, "a refused row is left as it is");
}
