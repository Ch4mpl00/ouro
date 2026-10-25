use ouro_durable::{
    AtomicBackend, RunStore, Snapshot, TransactionalStore,
    conformance::{self, TestStore},
    memory::MemoryBackend,
    postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use std::sync::Arc;

#[tokio::test]
async fn memory_conformance() {
    let report = conformance::run_all(async |_| {
        let backend = Arc::new(MemoryBackend::default());
        let reopen = backend.clone();
        Ok(TestStore {
            store: Arc::new(TransactionalStore::new(backend.clone())),
            backend: backend.clone(),
            faults: backend.faults.clone(),
            reopen: Arc::new(move || {
                let store: Arc<dyn RunStore> = Arc::new(TransactionalStore::new(reopen.clone()));
                Box::pin(async move { Ok(store) })
            }),
            read_only: Arc::new(move || {
                let view: Arc<dyn AtomicBackend> = Arc::new(backend.read_only_view());
                Box::pin(async move { Ok(view) })
            }),
        })
    })
    .await
    .unwrap();
    eprintln!("memory: {report:?}");
}
#[tokio::test]
async fn sqlite_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let count = std::sync::atomic::AtomicUsize::new(0);
    let report = conformance::run_all(async |_| {
        let path = dir.path().join(format!("case{}.sqlite", count.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let backend = Arc::new(SqliteBackend::open(&path)?);
        let shared_faults = backend.faults.clone();
        let reader = path.clone();
        Ok(TestStore {
            store: Arc::new(TransactionalStore::new(backend.clone())),
            backend: backend.clone(),
            faults: backend.faults.clone(),
            reopen: Arc::new(move || {
                let (path, faults) = (path.clone(), shared_faults.clone());
                Box::pin(async move {
                    let mut b = SqliteBackend::open(&path)?;
                    b.faults = faults;
                    Ok(Arc::new(TransactionalStore::new(Arc::new(b))) as Arc<dyn RunStore>)
                })
            }),
            read_only: Arc::new(move || {
                let path = reader.clone();
                Box::pin(async move { Ok(Arc::new(SqliteBackend::open_read_only(&path)?) as Arc<dyn AtomicBackend>) })
            }),
        })
    })
    .await
    .unwrap();
    eprintln!("sqlite: {report:?}");
}

// A current-thread runtime: the async store needs no worker threads, and
// the suite's interpreter cases (crash and resume included) run on it.
/// A Postgres server for one test: `TEST_DATABASE_URL`, or a disposable
/// container that lives as long as the returned handle.
async fn postgres_server()
-> (Option<testcontainers::ContainerAsync<testcontainers::GenericImage>>, tokio_postgres::Config) {
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };
    if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
        return (None, url.parse::<tokio_postgres::Config>().unwrap());
    }
    let container = GenericImage::new("postgres", "16")
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
    (Some(container), c)
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_conformance() {
    let (_container, config) = postgres_server().await;
    let (admin, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let databases = std::sync::Mutex::new(Vec::new());
    let result = conformance::run_all(async |_| {
        let name = format!("ouro_test_{}", uuid::Uuid::new_v4().simple());
        admin.batch_execute(&format!("CREATE DATABASE {name}")).await?;
        databases.lock().unwrap().push(name.clone());
        let mut c = config.clone();
        c.dbname(&name);
        let reopen = c.clone();
        let reader = c.clone();
        let backend = Arc::new(PostgresBackend::connect_config(c).await?);
        let shared_faults = backend.faults.clone();
        Ok(TestStore {
            store: Arc::new(TransactionalStore::new(backend.clone())),
            backend: backend.clone(),
            faults: backend.faults.clone(),
            reopen: Arc::new(move || {
                let (config, faults) = (reopen.clone(), shared_faults.clone());
                Box::pin(async move {
                    let backend = PostgresBackend::connect_with_faults(config, faults).await?;
                    Ok(Arc::new(TransactionalStore::new(Arc::new(backend))) as Arc<dyn RunStore>)
                })
            }),
            read_only: Arc::new(move || {
                let config = reader.clone();
                Box::pin(async move {
                    Ok(Arc::new(PostgresBackend::read_only_config(config).await?) as Arc<dyn AtomicBackend>)
                })
            }),
        })
    })
    .await;
    for name in databases.into_inner().unwrap() {
        admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    }
    eprintln!("postgres: {:?}", result.unwrap());
}

#[tokio::test]
async fn sqlite_corruption_stops_recovery_without_erasing_evidence() {
    use ouro_durable::RunStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sqlite");
    let backend = Arc::new(SqliteBackend::open(&path).unwrap());
    let s = TransactionalStore::new(backend.clone());
    s.start(&conformance::admission("r"), 0).await.unwrap();
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute("UPDATE ouro_rows SET value='broken json' WHERE kind='run'", []).unwrap();
    assert!(backend.snapshot().await.is_err());
    assert!(s.claim("r", 100_000).await.is_err());
    assert_eq!(
        c.query_row("SELECT value FROM ouro_rows WHERE kind='run'", [], |r| { r.get::<_, String>(0) }).unwrap(),
        "broken json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_and_cas_writes_have_one_winner() {
    use ouro_durable::RunStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.sqlite");
    ouro_durable::sqlite::open(&path).unwrap().admit(&conformance::admission("r"), 0).await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let s = ouro_durable::sqlite::open(path).unwrap();
                barrier.wait().await;
                s.claim("r", 0).await.unwrap().is_some()
            })
        })
        .collect();
    let mut winners = 0;
    for worker in workers {
        winners += usize::from(worker.await.unwrap());
    }
    assert_eq!(winners, 1);
}

#[tokio::test]
async fn run_rows_of_another_state_version_are_refused() {
    use ouro_durable::RunStore;
    let backend = Arc::new(MemoryBackend::default());
    let s = TransactionalStore::new(backend.clone());
    s.start(&conformance::admission("r"), 0).await.unwrap();
    let key = ("run".to_string(), "r".to_string());
    // 3 and earlier: one aggregate per run; 4 and 5: call records with an effect.
    for version in [None, Some(3), Some(4), Some(5), Some(6)] {
        let before = backend.snapshot().await.unwrap();
        let mut rows = before.rows.clone();
        let run = rows.get_mut(&key).unwrap().as_object_mut().unwrap();
        // The earlier layout: one aggregate holding the records, no version.
        run.insert("records".into(), serde_json::json!({ "wait": { "kind": "timer", "deadline_ms": 1 } }));
        match version {
            Some(v) => run.insert("version".into(), serde_json::json!(v)),
            None => run.remove("version"),
        };
        assert!(backend.compare_exchange(&before, &rows).await.unwrap());
        let refused = s.get("r").await.unwrap_err().to_string();
        assert!(refused.contains("unsupported durable state version"), "{refused}");
        assert!(s.claim("r", ouro_durable::LEASE_MS + 1).await.is_err());
        assert!(s.inspect("r").await.is_err() && s.list(None, "").await.is_err() && s.cancel("r", 0).await.is_err());
        assert_eq!(backend.snapshot().await.unwrap(), Snapshot { rows }, "a refused row is left as it is");
        let current = backend.snapshot().await.unwrap();
        assert!(backend.compare_exchange(&current, &before.rows).await.unwrap());
    }
}

#[tokio::test]
async fn definitions_rows_of_an_earlier_layout_are_refused() {
    use ouro_durable::RunStore;
    let backend = Arc::new(MemoryBackend::default());
    let s = TransactionalStore::new(backend.clone());
    let d = s.save_definition("workflow", "yaml").await.unwrap();
    s.activate("workflow", d.version, 0).await.unwrap();
    let key = ("definitions".to_string(), "workflow".to_string());
    let before = backend.snapshot().await.unwrap();
    let mut rows = before.rows.clone();
    let row = rows.get_mut(&key).unwrap().as_object_mut().unwrap();
    // The earlier layout: a predecessor and its triggers kept for a breaker
    // rollback, a boolean trip, no version.
    row.remove("version");
    row.insert("previous".into(), serde_json::json!(null));
    row.insert("fallback".into(), serde_json::json!({}));
    row.insert("tripped".into(), serde_json::json!(true));
    assert!(backend.compare_exchange(&before, &rows).await.unwrap());
    let refused = s.definitions("workflow").await.unwrap_err().to_string();
    assert!(refused.contains("unsupported layout"), "{refused}");
    assert!(
        s.activate("workflow", d.version, 0).await.is_err() && s.save_definition("workflow", "next").await.is_err()
    );
    assert!(s.admit(&conformance::admission("r"), 0).await.is_err(), "admission reads the definition's epoch");
    assert_eq!(backend.snapshot().await.unwrap(), Snapshot { rows: rows.clone() }, "a refused row is left as it is");
    // Layout 3, without activation times, is refused as well rather than
    // read as versions that were never activated.
    let current = backend.snapshot().await.unwrap();
    assert!(backend.compare_exchange(&current, &before.rows).await.unwrap());
    let mut rows = before.rows.clone();
    let row = rows.get_mut(&key).unwrap().as_object_mut().unwrap();
    row.insert("version".into(), serde_json::json!(3));
    row.remove("activations");
    assert!(backend.compare_exchange(&before, &rows).await.unwrap());
    let refused = s.definitions("workflow").await.unwrap_err().to_string();
    assert!(refused.contains("unsupported layout 3"), "{refused}");
    assert_eq!(backend.snapshot().await.unwrap(), Snapshot { rows }, "a refused row is left as it is");
}

#[tokio::test]
async fn sqlite_read_only_open_never_creates_or_migrates() {
    use ouro_durable::{ReadOnly, SchemaError};
    let dir = tempfile::tempdir().unwrap();
    let schema = |e: anyhow::Error| e.downcast::<SchemaError>().expect("a typed schema refusal");
    // A missing file is not created.
    let missing = dir.path().join("missing.sqlite");
    assert!(SqliteBackend::open_read_only(&missing).is_err());
    assert!(!missing.exists());
    // An empty database has no schema, and opening it read-only adds none.
    let empty = dir.path().join("empty.sqlite");
    rusqlite::Connection::open(&empty).unwrap().execute_batch("PRAGMA user_version = 0").unwrap();
    assert!(matches!(schema(SqliteBackend::open_read_only(&empty).err().unwrap()), SchemaError::Unmigrated));
    let tables: i64 = rusqlite::Connection::open(&empty)
        .unwrap()
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tables, 0);
    // A newer schema is refused by both opens, and left as it is.
    let newer = dir.path().join("newer.sqlite");
    drop(SqliteBackend::open(&newer).unwrap());
    rusqlite::Connection::open(&newer).unwrap().execute_batch("PRAGMA user_version = 2").unwrap();
    let expected = SchemaError::Unsupported { found: 2, supported: 1 };
    assert_eq!(schema(SqliteBackend::open_read_only(&newer).err().unwrap()), expected);
    assert_eq!(schema(SqliteBackend::open(&newer).err().unwrap()), expected);
    // A store whose writer has closed (its WAL checkpointed away) still
    // opens read-only and answers; a write is refused by the backend and,
    // underneath it, by SQLite.
    let path = dir.path().join("closed.sqlite");
    let writer = TransactionalStore::new(Arc::new(SqliteBackend::open(&path).unwrap()));
    writer.admit(&conformance::admission("r"), 0).await.unwrap();
    drop(writer);
    let backend = Arc::new(SqliteBackend::open_read_only(&path).unwrap());
    let reader = TransactionalStore::new(backend.clone());
    assert_eq!(reader.get("r").await.unwrap().unwrap().admission, conformance::admission("r"));
    assert!(reader.admit(&conformance::admission("s"), 0).await.unwrap_err().is::<ReadOnly>());
    let raw = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(raw.execute("DELETE FROM ouro_rows", []).is_err());
    assert!(backend.snapshot().await.unwrap().rows.contains_key(&("run".to_string(), "r".to_string())));
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_read_only_open_never_creates_or_migrates() {
    use ouro_durable::{ReadOnly, SchemaError};
    let (_container, server) = postgres_server().await;
    let (admin, connection) = server.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let name = format!("ouro_read_only_{}", uuid::Uuid::new_v4().simple());
    admin.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
    let mut config = server.clone();
    config.dbname(&name);
    let result: anyhow::Result<()> = async {
        let schema = |e: anyhow::Error| e.downcast::<SchemaError>().expect("a typed schema refusal");
        let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let tables = async || -> anyhow::Result<i64> {
            Ok(client.query_one("SELECT count(*) FROM pg_tables WHERE tablename LIKE 'ouro_%'", &[]).await?.get(0))
        };
        // No migrations table: unmigrated, and nothing is created.
        let refused = PostgresBackend::read_only_config(config.clone()).await.err().unwrap();
        assert_eq!(schema(refused), SchemaError::Unmigrated);
        assert_eq!(tables().await?, 0);
        let writer = TransactionalStore::new(Arc::new(PostgresBackend::connect_config(config.clone()).await?));
        writer.admit(&conformance::admission("r"), 0).await?;
        let backend = Arc::new(PostgresBackend::read_only_config(config.clone()).await?);
        let reader = TransactionalStore::new(backend.clone());
        assert_eq!(reader.get("r").await?.unwrap().admission, conformance::admission("r"));
        assert!(reader.cancel("r", 0).await.unwrap_err().is::<ReadOnly>());
        // A newer schema is refused by both opens.
        client.batch_execute("INSERT INTO ouro_migrations VALUES (2)").await?;
        let expected = SchemaError::Unsupported { found: 2, supported: 1 };
        assert_eq!(schema(PostgresBackend::read_only_config(config.clone()).await.err().unwrap()), expected);
        assert_eq!(schema(PostgresBackend::connect_config(config.clone()).await.err().unwrap()), expected);
        Ok(())
    }
    .await;
    admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    result.unwrap();
}
