//! Serializable Postgres transactions over indexed engine keys. A bounded I/O
//! thread bridges synchronous interpreter calls without nesting runtimes.
use crate::*;
use deadpool_postgres::{Manager, Pool};
use std::{
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tokio::{sync::mpsc as channel, task::JoinSet};
use tokio_postgres::{IsolationLevel, NoTls, error::SqlState};

type Reply<T> = mpsc::SyncSender<anyhow::Result<T>>;
enum Command {
    Get(String, String, Reply<Option<Value>>),
    Scan(String, String, Option<String>, usize, Reply<Rows>),
    Put(String, String, Option<Value>, Reply<()>),
    Commit(Reply<()>),
}
enum Request {
    Begin(channel::Receiver<Command>, Reply<()>),
    Snapshot(Reply<Snapshot>),
}
pub struct PostgresBackend {
    sender: Option<channel::Sender<Request>>,
    worker: Option<JoinHandle<()>>,
    pub faults: Arc<FaultInjector>,
    pub metrics: Arc<Metrics>,
}
// These synchronous seams may be called from a Tokio host. blocking_send
// would panic there; try_send with a deadline also bounds admission to I/O.
fn send<T>(sender: &channel::Sender<T>, mut request: T) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    loop {
        match sender.try_send(request) {
            Ok(()) => return Ok(()),
            Err(channel::error::TrySendError::Closed(_)) => {
                anyhow::bail!("Postgres I/O session closed; commit outcome may be unknown")
            }
            Err(channel::error::TrySendError::Full(value)) => request = value,
        }
        anyhow::ensure!(started.elapsed() < Duration::from_secs(30), "Postgres request queue exceeded 30 seconds");
        std::thread::sleep(Duration::from_micros(100));
    }
}
fn receive<T>(rx: mpsc::Receiver<anyhow::Result<T>>) -> anyhow::Result<T> {
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| anyhow::anyhow!("Postgres I/O response unavailable; commit outcome may be unknown"))?
}
impl Drop for PostgresBackend {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
        let (sender, mut receiver) = channel::channel(64);
        let (ready, started) = mpsc::sync_channel(1);
        let worker_faults = faults.clone();
        let metrics = Arc::new(Metrics::default());
        let worker_metrics = metrics.clone();
        let worker = std::thread::Builder::new().name("ouro-postgres".into()).spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let pool = Pool::builder(manager).max_size(16).build()?;
                rt.block_on(bounded(migrate(&pool)))?;
                Ok((rt,pool))
            })();
            match result {
                Ok((rt,pool)) => {
                    if ready.send(Ok(())).is_err() { return; }
                    rt.block_on(async move {
                        let mut sessions = JoinSet::new();
                        loop {
                            if sessions.len() == 16 { sessions.join_next().await; }
                            tokio::select! {
                                request = receiver.recv() => {
                                    let Some(request) = request else { break; };
                                    let pool = pool.clone();
                                    let faults = worker_faults.clone();
                                    let metrics = worker_metrics.clone();
                                    sessions.spawn(async move {
                                        match request {
                                            Request::Begin(commands,reply) => { let _ = bounded(session(&pool,commands,reply,&faults,&metrics)).await; }
                                            Request::Snapshot(reply) => { let _ = reply.send(bounded(snapshot(&pool,&metrics)).await); }
                                        }
                                    });
                                }
                                _ = sessions.join_next(), if !sessions.is_empty() => {}
                            }
                        }
                        while sessions.join_next().await.is_some() {}
                        pool.close();
                    });
                }
                Err(error) => { let _ = ready.send(Err(error)); }
            }
        })?;
        started.recv()??;
        Ok(Self { sender: Some(sender), worker: Some(worker), faults, metrics })
    }
}
struct PostgresTransaction<'a> {
    sender: channel::Sender<Command>,
    backend: &'a PostgresBackend,
}
impl PostgresTransaction<'_> {
    fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> anyhow::Result<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        send(&self.sender, command(tx))?;
        let result = receive(rx);
        if result.as_ref().is_err_and(|e| e.is::<Conflict>()) {
            self.backend.metrics.conflict();
        }
        result
    }
}
impl AtomicBackend for PostgresBackend {
    fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        let (sender, receiver) = channel::channel(1);
        let (tx, rx) = mpsc::sync_channel(1);
        send(self.sender.as_ref().unwrap(), Request::Begin(receiver, tx))?;
        receive(rx)?;
        Ok(Box::new(PostgresTransaction { sender, backend: self }))
    }
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let (tx, rx) = mpsc::sync_channel(1);
        send(self.sender.as_ref().unwrap(), Request::Snapshot(tx))?;
        receive(rx)
    }
}
impl Transaction for PostgresTransaction<'_> {
    fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.call(|r| Command::Get(kind.into(), id.into(), r))
    }
    fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        self.call(|r| Command::Scan(kind.into(), start.into(), end.map(str::to_owned), limit, r))
    }
    fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        self.call(|r| Command::Put(kind.into(), id.into(), Some(value.clone()), r))
    }
    fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.call(|r| Command::Put(kind.into(), id.into(), None, r))
    }
    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.call(Command::Commit)
    }
}
fn definite(error: tokio_postgres::Error) -> anyhow::Error {
    if error.code().is_some_and(|c| *c == SqlState::T_R_SERIALIZATION_FAILURE || *c == SqlState::T_R_DEADLOCK_DETECTED)
    {
        Conflict.into()
    } else {
        error.into()
    }
}
async fn session(
    pool: &Pool,
    mut commands: channel::Receiver<Command>,
    ready: Reply<()>,
    faults: &FaultInjector,
    metrics: &Metrics,
) -> anyhow::Result<()> {
    let started = async {
        let client = pool.get().await?;
        Ok::<_, anyhow::Error>(client)
    }
    .await;
    let mut client = match started {
        Ok(client) => client,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    let tx = match client.build_transaction().isolation_level(IsolationLevel::Serializable).start().await {
        Ok(tx) => tx,
        Err(e) => {
            let _ = ready.send(Err(definite(e)));
            return Ok(());
        }
    };
    if ready.send(Ok(())).is_err() {
        return Ok(());
    }
    let mut writes = 0;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Get(kind, id, reply) => {
                let result = async {
                    let row = tx
                        .query_opt("SELECT value FROM ouro_rows WHERE kind=$1 AND id=$2", &[&kind, &id])
                        .await
                        .map_err(definite)?;
                    metrics.read(usize::from(row.is_some()));
                    row.map(|r| Ok(serde_json::from_str(&r.get::<_, String>(0))?)).transpose()
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            Command::Scan(kind, start, end, limit, reply) => {
                let result: anyhow::Result<Rows> = async {
                    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
                    let rows = if let Some(end) = end {
                        tx.query(
                            "SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 AND id<$3 ORDER BY id LIMIT $4",
                            &[&kind, &start, &end, &limit],
                        )
                        .await
                    } else {
                        tx.query(
                            "SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 ORDER BY id LIMIT $3",
                            &[&kind, &start, &limit],
                        )
                        .await
                    }
                    .map_err(definite)?;
                    metrics.read(rows.len());
                    rows.into_iter()
                        .map(|r| Ok(((kind.clone(), r.get(0)), serde_json::from_str(&r.get::<_, String>(1))?)))
                        .collect()
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            Command::Put(kind, id, value, reply) => {
                let result = async {
                    faults.check(CommitFault::BeforeWrite(writes))?;
                    writes += 1;
                    metrics.write();
                    if let Some(value) = value {
                        let value = serde_json::to_string(&value)?;
                        // Concurrent inserts of the same absent key become a
                        // serializable conflict, rather than an ambiguous retry.
                        tx.execute("INSERT INTO ouro_rows(kind,id,value) VALUES ($1,$2,$3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",&[&kind,&id,&value]).await.map_err(definite)?;
                    } else {
                        tx.execute("DELETE FROM ouro_rows WHERE kind=$1 AND id=$2",&[&kind,&id]).await.map_err(definite)?;
                    }
                    Ok(())
                }.await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            Command::Commit(reply) => {
                let result = async {
                    faults.check(CommitFault::BeforeCommit)?;
                    tx.commit().await.map_err(definite)?;
                    faults.check(CommitFault::AfterCommit)
                }
                .await;
                let _ = reply.send(result);
                return Ok(());
            }
        }
    }
    tx.rollback().await?;
    Ok(())
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
async fn snapshot(pool: &Pool, metrics: &Metrics) -> anyhow::Result<Snapshot> {
    let client = pool.get().await?;
    let raw = client.query("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id", &[]).await?;
    metrics.read(raw.len());
    let rows = raw
        .into_iter()
        .map(|r| Ok(((r.get(0), r.get(1)), serde_json::from_str(&r.get::<_, String>(2))?)))
        .collect::<anyhow::Result<_>>()?;
    Ok(Snapshot { rows })
}
pub fn connect(url: &str) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(PostgresBackend::connect(url)?)))
}
async fn bounded<T>(operation: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .map_err(|_| anyhow::anyhow!("Postgres operation exceeded 30 seconds; commit outcome may be unknown"))?
}
