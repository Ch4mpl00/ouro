use ouro_durable::{AtomicBackend, RunStore, TransactionalStore, conformance::admission};
use owf::{durable::Record, run::Status};
use serde_json::json;
use std::sync::Arc;

/// Duplicate persisted templates produced by real transitions, including their
/// index keys. Bulk insertion keeps fixture preparation outside measurements.
pub async fn seed(backend: &Arc<dyn AtomicBackend>, size: usize) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend.clone());
    for mode in 0..3 {
        let id = format!("template{mode}");
        let now = if mode == 1 { 1_000_000 } else { 0 };
        let f = store.start(&admission(&id), now).await?.unwrap();
        store.save_record(&f, "timer", &Record::Timer { deadline_ms: 42 }, now).await?;
        store.register_wait(&f, &["unrelated".into()], now).await?;
        // Unfinished seeds also subscribe to published events of another type.
        store.subscribe(&f, "listen", &["unrelated".into()], now).await?;
        let mut wait = ouro_durable::conformance::control_wait();
        wait.types = vec!["unrelated".into()];
        if let owf::model::Strategy::One(filter) = &mut wait.strategy {
            filter.with.insert("type".into(), json!("unrelated"));
        }
        wait.since_ms = now;
        wait.timeout_ms = None;
        store.describe_wait(&f, "listen", &wait, now).await?;
        let outcome = if mode == 1 {
            Status::Completed(json!(42))
        } else {
            Status::Waiting { wake_at_ms: if mode == 0 { 0 } else { 1_000_000 } }
        };
        store.finish(&f, &outcome, if mode == 1 { 1_000_000 } else { 0 }, 3).await?;
    }
    let templates = backend.snapshot().await?.rows;
    let hex = |id: &str| id.bytes().map(|b| format!("{b:02x}")).collect::<String>();
    // Chunked transactions also respect the Postgres session deadline.
    for first in (0..size).step_by(200) {
        let mut tx = backend.begin().await?;
        for n in first..size.min(first + 200) {
            let original = format!("template{}", n % 3);
            let id = format!("seed{n:06}");
            for ((kind, key), value) in &templates {
                if kind == "run" && key == &original {
                    let mut value = value.clone();
                    value["run"]["admission"]["id"] = json!(id);
                    tx.put(kind, &id, &value).await?;
                } else if kind.starts_with("control_wait") && value.get("run") == Some(&json!(original)) {
                    let mut value = value.clone();
                    value["run"] = json!(id);
                    tx.put(&kind.replace(&hex(&original), &hex(&id)), &key.replace(&hex(&original), &hex(&id)), &value)
                        .await?;
                } else if kind.contains(&hex(&original)) {
                    // The run's own rows (its records), partitioned by its id.
                    tx.put(&kind.replace(&hex(&original), &hex(&id)), key, value).await?;
                } else if value == &json!(original) {
                    tx.put(kind, &key.replace(&original, &id), &json!(id)).await?;
                }
            }
        }
        tx.commit().await?;
    }
    for mode in 0..3 {
        store.cancel(&format!("template{mode}"), 0).await?;
    }
    Ok(())
}

pub async fn postgres(test: impl AsyncFnOnce(tokio_postgres::Config) -> anyhow::Result<()>) {
    use futures::FutureExt;
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
    let (admin, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let name = format!("ouro_scale_{}", uuid::Uuid::new_v4().simple());
    admin.batch_execute(&format!("CREATE DATABASE {name}")).await.unwrap();
    let mut c = config;
    c.dbname(&name);
    // The database is dropped whether the test passes, fails or panics.
    let result = std::panic::AssertUnwindSafe(test(c)).catch_unwind().await;
    admin.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    match result {
        Ok(result) => result.unwrap(),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
