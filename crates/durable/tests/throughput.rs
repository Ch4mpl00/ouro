//! Opt-in local database throughput measurement; no handlers or external APIs.
use ouro_durable::{
    AtomicBackend, RunStore, TransactionalStore, conformance::admission, memory::MemoryBackend,
    postgres::PostgresBackend, sqlite::SqliteBackend,
};
use ows::{durable::Record, run::Status};
use serde_json::json;
use std::{sync::Arc, time::Instant};
mod support;

async fn measure(label: &str, size: usize, backend: Arc<dyn AtomicBackend>) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend.clone());
    support::seed(&backend, size).await?;
    let samples = 40;
    let mut fences = Vec::new();
    let started = Instant::now();
    for n in 0..samples {
        fences.push(store.start(&admission(&format!("new{n}")), 0).await?.unwrap());
    }
    eprintln!(
        "BENCH,{label},{size},start,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for n in 0..samples {
        store.claim(&format!("seed{:06}", n * 3), 0).await?.unwrap();
    }
    eprintln!(
        "BENCH,{label},{size},claim,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for f in &fences {
        store.save_record(f, "timer", &Record::Timer { deadline_ms: 42 }, 0).await?;
    }
    eprintln!(
        "BENCH,{label},{size},save_record,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    let started = Instant::now();
    for f in &fences {
        store.finish(f, &Status::Completed(json!(42)), 0, 3).await?;
    }
    eprintln!(
        "BENCH,{label},{size},finish,{samples},{:.6},{:.2}",
        started.elapsed().as_secs_f64(),
        f64::from(samples) / started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// One run accumulating `records` records, as a long agent loop or a large
/// `for` does: save rates over the first and the last slices of its life, and
/// the final load. A layout that rewrites the run on every save slows down as
/// the run grows; one row per record keeps both slices alike.
async fn long_run(label: &str, records: usize, backend: Arc<dyn AtomicBackend>) -> anyhow::Result<()> {
    let store = TransactionalStore::new(backend);
    let f = store.start(&admission("long"), 0).await?.unwrap();
    let slice = records / 10;
    let report = |part: &str, n: usize, seconds: f64| {
        eprintln!("BENCH,{label},{records},{part},{n},{seconds:.6},{:.2}", n as f64 / seconds);
    };
    let started = Instant::now();
    let mut slice_started = started;
    for n in 0..records {
        if n == records - slice {
            slice_started = Instant::now();
        }
        store.save_record(&f, &format!("/do/{n}/step#wait"), &Record::Timer { deadline_ms: 42 }, 0).await?;
        if n + 1 == slice {
            report("save_record_first_tenth", slice, slice_started.elapsed().as_secs_f64());
        }
    }
    report("save_record_last_tenth", slice, slice_started.elapsed().as_secs_f64());
    report("save_record_all", records, started.elapsed().as_secs_f64());
    let started = Instant::now();
    assert_eq!(store.load_records(&f, 0).await?.len(), records);
    report("load_records", 1, started.elapsed().as_secs_f64());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "local throughput measurement: cargo test -p ouro-durable --test throughput -- --ignored --nocapture"]
async fn long_run_throughput() {
    const RECORDS: usize = 5_000;
    long_run("memory", RECORDS, Arc::new(MemoryBackend::default())).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    long_run("sqlite", RECORDS, Arc::new(SqliteBackend::open(dir.path().join("long.sqlite")).unwrap())).await.unwrap();
    support::postgres(async |config| {
        long_run("postgres", RECORDS, Arc::new(PostgresBackend::connect_config(config).await?)).await
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "local throughput measurement: cargo test -p ouro-durable --test throughput -- --ignored --nocapture"]
async fn database_throughput() {
    for size in [1_000, 10_000] {
        let dir = tempfile::tempdir().unwrap();
        measure("sqlite", size, Arc::new(SqliteBackend::open(dir.path().join("bench.sqlite")).unwrap())).await.unwrap();
        support::postgres(async |config| {
            measure("postgres", size, Arc::new(PostgresBackend::connect_config(config).await?)).await
        })
        .await;
    }
}
