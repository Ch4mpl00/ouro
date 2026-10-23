use ouro_durable::{AtomicBackend, RunStore, TransactionalStore, conformance::admission};
use owf::{durable::Record, run::Status};
use serde_json::json;
use std::sync::Arc;

/// Duplicate persisted templates produced by real transitions, including their
/// index keys. Bulk insertion keeps fixture preparation outside measurements.
pub fn seed(backend: &Arc<dyn AtomicBackend>, size: usize) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend.clone());
    for mode in 0..3 {
        let id = format!("template{mode}");
        let now = if mode == 1 { 1_000_000 } else { 0 };
        let f = store.start(&admission(&id), now)?.unwrap();
        store.save_record(&f, "timer", &Record::Timer { deadline_ms: 42 }, now)?;
        store.register_wait(&f, &["unrelated".into()], now)?;
        let outcome = if mode == 1 {
            Status::Completed(json!(42))
        } else {
            Status::Waiting { wake_at_ms: if mode == 0 { 0 } else { 1_000_000 } }
        };
        store.finish(&f, &outcome, if mode == 1 { 1_000_000 } else { 0 }, 3)?;
    }
    let templates = backend.snapshot()?.rows;
    // Chunked transactions also respect the Postgres session deadline.
    for first in (0..size).step_by(200) {
        let mut tx = backend.begin()?;
        for n in first..size.min(first + 200) {
            let original = format!("template{}", n % 3);
            let id = format!("seed{n:06}");
            for ((kind, key), value) in &templates {
                if kind == "run" && key == &original {
                    let mut value = value.clone();
                    value["run"]["admission"]["id"] = json!(id);
                    tx.put(kind, &id, &value)?;
                } else if value == &json!(original) {
                    tx.put(kind, &key.replace(&original, &id), &json!(id))?;
                }
            }
        }
        tx.commit()?;
    }
    for mode in 0..3 {
        store.cancel(&format!("template{mode}"), 0)?;
    }
    Ok(())
}

pub async fn postgres(test: impl FnOnce(tokio_postgres::Config) -> anyhow::Result<()> + Send + 'static) {
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
            .unwrap();
        let mut config = tokio_postgres::Config::new();
        config
            .host(container.get_host().await.unwrap().to_string())
            .port(container.get_host_port_ipv4(5432).await.unwrap())
            .user("postgres")
            .password("local-test-password")
            .dbname("postgres");
        config
    };
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (admin, connection) = rt.block_on(config.connect(tokio_postgres::NoTls))?;
        rt.spawn(async move {
            let _ = connection.await;
        });
        let name = format!("ouro_scale_{}", uuid::Uuid::new_v4().simple());
        rt.block_on(admin.batch_execute(&format!("CREATE DATABASE {name}")))?;
        let mut c = config;
        c.dbname(&name);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(c)));
        rt.block_on(admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")))?;
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
    .await
    .unwrap()
    .unwrap();
}
