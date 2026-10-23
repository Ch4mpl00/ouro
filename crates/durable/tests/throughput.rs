//! Opt-in local database throughput measurement; no handlers or external APIs.
use ouro_durable::{
    AtomicBackend, RunStore, TransactionalStore, conformance::admission, postgres::PostgresBackend,
    sqlite::SqliteBackend,
};
use owf::{durable::Record, run::Status};
use serde_json::json;
use std::{sync::Arc, time::Instant};
mod support;

fn measure(label: &str, size: usize, backend: Arc<dyn AtomicBackend>) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend.clone());
    support::seed(&backend, size)?;
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
    for size in [1_000, 10_000] {
        let dir = tempfile::tempdir().unwrap();
        measure("sqlite", size, Arc::new(SqliteBackend::open(dir.path().join("bench.sqlite")).unwrap())).unwrap();
        support::postgres(move |config| measure("postgres", size, Arc::new(PostgresBackend::connect_config(config)?)))
            .await;
    }
}
