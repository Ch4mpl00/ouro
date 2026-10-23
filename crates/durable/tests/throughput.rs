//! Opt-in local database throughput measurement; no handlers or external APIs.
use ouro_durable::{
    AtomicBackend, RunState, RunStore, TransactionalStore, conformance::admission, postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use owf::{durable::Record, run::Status};
use serde_json::json;
use std::{sync::Arc, time::Instant};

fn measure(label: &str, size: usize, backend: Arc<dyn AtomicBackend>) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend.clone());
    let f = store.start(&admission("template"), 0)?.unwrap();
    store.save_record(&f, "timer", &Record::Timer { deadline_ms: 42 }, 0)?;
    store.register_wait(&f, &["unrelated".into()], 0)?;
    store.finish(&f, &Status::Waiting { wake_at_ms: 1_000_000 }, 0, 3)?;
    let before = backend.snapshot()?;
    let template = before.rows[&("run".into(), "template".into())].clone();
    let mut rows = before.rows.clone();
    for n in 0..size {
        let id = format!("seed{n:06}");
        let mut value = template.clone();
        value["run"]["admission"]["id"] = json!(id);
        if n % 3 == 0 {
            value["run"]["state"] = json!(RunState::Ready);
            value["run"]["wake_at_ms"] = json!(0);
        } else if n % 3 == 1 {
            value["run"]["state"] = json!(RunState::Completed);
            value["run"]["ended_ms"] = json!(0);
            value["run"]["outcome"] = json!(Status::Completed(json!(42)));
            value["awaits"] = json!({});
        }
        rows.insert(("run".into(), id), value);
    }
    assert!(backend.compare_exchange(&before, &rows)?);
    let samples = 40;
    let mut fences = Vec::new();
    let started = Instant::now();
    for n in 0..samples {
        fences.push(store.start(&admission(&format!("new{n}")), 0)?.unwrap());
    }
    eprintln!(
        "BENCH,{label},{size},start,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for n in 0..samples {
        store.claim(&format!("seed{:06}", n * 3), 0)?.unwrap();
    }
    eprintln!(
        "BENCH,{label},{size},claim,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for f in &fences {
        store.save_record(f, "timer", &Record::Timer { deadline_ms: 42 }, 0)?;
    }
    eprintln!(
        "BENCH,{label},{size},save_record,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for f in &fences {
        store.finish(f, &Status::Completed(json!(42)), 0, 3)?;
    }
    eprintln!(
        "BENCH,{label},{size},finish,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "local throughput measurement: cargo test -p ouro-durable --test throughput -- --ignored --nocapture"]
async fn database_throughput() {
    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };
    let container = GenericImage::new("postgres", "16")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_env_var("POSTGRES_PASSWORD", "local-test-password")
        .start()
        .await
        .unwrap();
    let mut config = tokio_postgres::Config::new();
    config
        .host(container.get_host().await.unwrap().to_string())
        .port(container.get_host_port_ipv4(5432).await.unwrap())
        .user("postgres")
        .password("local-test-password")
        .dbname("postgres");
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (admin, connection) = rt.block_on(config.connect(tokio_postgres::NoTls))?;
        rt.spawn(async move {
            let _ = connection.await;
        });
        for size in [1_000, 10_000] {
            let dir = tempfile::tempdir()?;
            measure("sqlite", size, Arc::new(SqliteBackend::open(dir.path().join("bench.sqlite"))?))?;
            let name = format!("ouro_bench_{}", uuid::Uuid::new_v4().simple());
            rt.block_on(admin.batch_execute(&format!("CREATE DATABASE {name}")))?;
            let mut c = config.clone();
            c.dbname(&name);
            let result = measure("postgres", size, Arc::new(PostgresBackend::connect_config(c)?));
            rt.block_on(admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")))?;
            result?;
        }
        Ok(())
    })
    .await
    .unwrap()
    .unwrap();
}
