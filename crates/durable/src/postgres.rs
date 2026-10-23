//! Postgres adapter using tokio-postgres and a deadpool pool. The interpreter
//! has synchronous persistence seams; a dedicated I/O thread bridges them
//! without nesting a runtime or blocking a Tokio connection driver.
use crate::{AtomicBackend, CommitFault, FaultInjector, Rows, Snapshot, TransactionalStore, backend::changes};
use deadpool_postgres::{Manager, Pool};
use std::sync::{Arc, mpsc};
use tokio_postgres::{IsolationLevel, NoTls};

enum Request {
    Snapshot(mpsc::SyncSender<anyhow::Result<Snapshot>>),
    Commit(Snapshot, Rows, mpsc::SyncSender<anyhow::Result<bool>>),
}
pub struct PostgresBackend {
    sender: mpsc::SyncSender<Request>,
    pub faults: Arc<FaultInjector>,
}
impl PostgresBackend {
    pub fn connect(url: &str) -> anyhow::Result<Self> {
        Self::connect_config(url.parse::<tokio_postgres::Config>()?)
    }
    pub fn connect_config(config: tokio_postgres::Config) -> anyhow::Result<Self> {
        Self::connect_with_faults(config, Arc::new(FaultInjector::default()))
    }
    pub fn connect_with_faults(config: tokio_postgres::Config, faults: Arc<FaultInjector>) -> anyhow::Result<Self> {
        Self::with_manager(Manager::new(config, NoTls), faults)
    }
    /// The host can inject a manager with TLS or custom connection settings.
    pub fn with_manager(manager: Manager, faults: Arc<FaultInjector>) -> anyhow::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let (ready, started) = mpsc::sync_channel(1);
        let worker_faults = faults.clone();
        std::thread::Builder::new().name("ouro-postgres".into()).spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let pool = Pool::builder(manager).max_size(4).build()?;
                rt.block_on(bounded(migrate(&pool)))?;
                Ok((rt, pool))
            })();
            match result {
                Ok((rt, pool)) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    while let Ok(request) = receiver.recv() {
                        match request {
                            Request::Snapshot(reply) => {
                                let _ = reply.send(rt.block_on(bounded(snapshot(&pool))));
                            }
                            Request::Commit(before, rows, reply) => {
                                let _ = reply.send(rt.block_on(bounded(commit(&pool, &before, &rows, &worker_faults))));
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        })?;
        started.recv()??;
        Ok(Self { sender, faults })
    }
}
impl AtomicBackend for PostgresBackend {
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender.send(Request::Snapshot(tx))?;
        rx.recv()?
    }
    fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender.send(Request::Commit(before.clone(), rows.clone(), tx))?;
        rx.recv()?
    }
}
async fn migrate(pool: &Pool) -> anyhow::Result<()> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(716532018); CREATE TABLE IF NOT EXISTS ouro_migrations (version INTEGER PRIMARY KEY);").await?;
    let version: i32 = tx.query_one("SELECT COALESCE(MAX(version),0) FROM ouro_migrations", &[]).await?.get(0);
    anyhow::ensure!(version <= 1, "unsupported Postgres schema version {version}");
    if version == 0 {
        tx.batch_execute(include_str!("../migrations/postgres/001_initial.sql")).await?;
        tx.execute("INSERT INTO ouro_migrations VALUES (1)", &[]).await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn snapshot(pool: &Pool) -> anyhow::Result<Snapshot> {
    let mut client = pool.get().await?;
    let tx = client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).read_only(true).start().await?;
    let revision = tx.query_one("SELECT revision FROM ouro_revision WHERE id=1", &[]).await?.get(0);
    let mut rows = Rows::new();
    for row in tx.query("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id", &[]).await? {
        let value: String = row.get(2);
        rows.insert((row.get(0), row.get(1)), serde_json::from_str(&value)?);
    }
    tx.commit().await?;
    Ok(Snapshot { revision, rows })
}
async fn commit(pool: &Pool, before: &Snapshot, rows: &Rows, faults: &FaultInjector) -> anyhow::Result<bool> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    if tx
        .execute("UPDATE ouro_revision SET revision=revision+1 WHERE id=1 AND revision=$1", &[&before.revision])
        .await?
        == 0
    {
        return Ok(false);
    }
    for (i, ((kind, id), value)) in changes(&before.rows, rows).into_iter().enumerate() {
        faults.check(CommitFault::BeforeWrite(i))?;
        if let Some(value) = value {
            let value = serde_json::to_string(value)?;
            tx.execute("INSERT INTO ouro_rows(kind,id,value) VALUES ($1,$2,$3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",&[kind,id,&value]).await?;
        } else {
            tx.execute("DELETE FROM ouro_rows WHERE kind=$1 AND id=$2", &[kind, id]).await?;
        }
    }
    faults.check(CommitFault::BeforeCommit)?;
    tx.commit().await?;
    faults.check(CommitFault::AfterCommit)?;
    Ok(true)
}
pub fn connect(url: &str) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(PostgresBackend::connect(url)?)))
}

async fn bounded<T>(operation: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(std::time::Duration::from_secs(30), operation)
        .await
        .map_err(|_| anyhow::anyhow!("Postgres operation exceeded 30 seconds; commit outcome may be unknown"))?
}
