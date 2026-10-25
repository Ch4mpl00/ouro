//! Serializable Postgres transactions over indexed engine keys, on
//! `tokio-postgres` through a `deadpool-postgres` pool. Every operation is
//! awaited on the caller's runtime, whatever its flavour: no thread of its
//! own, no queue in front of the pool. Connections are spawned on the runtime
//! that opens them, so a backend serves the runtime it was built on.
use crate::*;
use deadpool_postgres::{Manager, Object, Pool};
use std::{future::Future, sync::Arc, time::Duration};
use tokio_postgres::{NoTls, error::SqlState};

/// The most sessions (pooled connections, hence open transactions) at once.
const SESSIONS: usize = 16;
/// The longest a wait for a session, one statement or a commit may take.
const DEADLINE: Duration = Duration::from_secs(30);
/// The schema version this build reads and migrates to (`ouro_migrations`).
const SCHEMA: i64 = 1;

pub struct PostgresBackend {
    pool: Pool,
    read_only: bool,
    pub faults: Arc<FaultInjector>,
    pub metrics: Arc<Metrics>,
}
impl PostgresBackend {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        Self::connect_config(url.parse::<tokio_postgres::Config>()?).await
    }
    pub async fn connect_config(config: tokio_postgres::Config) -> anyhow::Result<Self> {
        Self::connect_with_faults(config, Arc::new(FaultInjector::default())).await
    }
    pub async fn connect_with_faults(
        config: tokio_postgres::Config,
        faults: Arc<FaultInjector>,
    ) -> anyhow::Result<Self> {
        Self::with_manager(Manager::new(config, NoTls), faults).await
    }
    /// The host can inject a manager with TLS or custom connection settings.
    /// Migrates the schema before returning.
    pub async fn with_manager(manager: Manager, faults: Arc<FaultInjector>) -> anyhow::Result<Self> {
        let pool = Pool::builder(manager).max_size(SESSIONS).build()?;
        bounded(migrate(&pool)).await?;
        Ok(Self { pool, read_only: false, faults, metrics: Arc::new(Metrics::default()) })
    }
    pub async fn connect_read_only(url: &str) -> anyhow::Result<Self> {
        Self::read_only_config(url.parse::<tokio_postgres::Config>()?).await
    }
    pub async fn read_only_config(config: tokio_postgres::Config) -> anyhow::Result<Self> {
        Self::read_only_manager(Manager::new(config, NoTls)).await
    }
    /// A backend for queries only: it never creates or migrates the schema
    /// and never writes. A database without the schema, or with another
    /// version of it, is refused with a `SchemaError`. Every transaction is
    /// `SERIALIZABLE READ ONLY`, so the server refuses a write too; the
    /// backend refuses each `put` and `delete` with `ReadOnly` before it is
    /// sent. A role with `SELECT` on `ouro_rows` and `ouro_migrations` is
    /// enough.
    pub async fn read_only_manager(manager: Manager) -> anyhow::Result<Self> {
        let pool = Pool::builder(manager).max_size(SESSIONS).build()?;
        bounded(schema(&pool)).await?;
        Ok(Self {
            pool,
            read_only: true,
            faults: Arc::new(FaultInjector::default()),
            metrics: Arc::new(Metrics::default()),
        })
    }
    async fn session(&self) -> anyhow::Result<Object> {
        bounded(async { Ok(self.pool.get().await?) }).await
    }
}
impl Drop for PostgresBackend {
    fn drop(&mut self) {
        // Idle sessions close now; a transaction borrows the backend, so
        // none is open any more.
        self.pool.close();
    }
}

/// One serializable transaction on its own session. A session goes back to
/// the pool only in a known state: after a commit, a rollback round trip or
/// a commit the server refused. Any other end (a dropped future, a passed
/// deadline, a transport error) discards it: its connection closes and the
/// server rolls the transaction back, so nothing half applied outlives it.
struct PostgresTransaction<'a> {
    session: Option<Object>,
    backend: &'a PostgresBackend,
    writes: usize,
}
impl Drop for PostgresTransaction<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            drop(Object::take(session));
        }
    }
}
impl PostgresTransaction<'_> {
    fn session(&self) -> anyhow::Result<&Object> {
        self.session.as_ref().ok_or_else(|| anyhow::anyhow!("the Postgres transaction has ended"))
    }
    /// One statement under the deadline; definite serialization failures
    /// become `Conflict` and are counted.
    async fn statement<T>(
        &self,
        operation: impl Future<Output = Result<T, tokio_postgres::Error>>,
    ) -> anyhow::Result<T> {
        let result = match tokio::time::timeout(DEADLINE, operation).await {
            Ok(result) => result.map_err(definite),
            Err(_) => Err(anyhow::anyhow!("Postgres operation exceeded 30 seconds; commit outcome may be unknown")),
        };
        if result.as_ref().is_err_and(|e| e.is::<Conflict>()) {
            self.backend.metrics.conflict();
        }
        result
    }
}
#[async_trait::async_trait]
impl AtomicBackend for PostgresBackend {
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        let tx = PostgresTransaction { session: Some(self.session().await?), backend: self, writes: 0 };
        let begin = if self.read_only {
            "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY"
        } else {
            "BEGIN ISOLATION LEVEL SERIALIZABLE"
        };
        tx.statement(tx.session()?.batch_execute(begin)).await?;
        Ok(Box::new(tx))
    }
    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let session = self.session().await?;
        let raw =
            bounded(async { Ok(session.query("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id", &[]).await?) })
                .await?;
        self.metrics.read(raw.len());
        let rows = raw
            .into_iter()
            .map(|r| Ok(((r.get(0), r.get(1)), serde_json::from_str(&r.get::<_, String>(2))?)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Snapshot { rows })
    }
}
#[async_trait::async_trait]
impl Transaction for PostgresTransaction<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        let session = self.session()?;
        let row = self
            .statement(async {
                let query = session.prepare_cached("SELECT value FROM ouro_rows WHERE kind=$1 AND id=$2").await?;
                session.query_opt(&query, &[&kind, &id]).await
            })
            .await?;
        self.backend.metrics.read(usize::from(row.is_some()));
        row.map(|r| Ok(serde_json::from_str(&r.get::<_, String>(0))?)).transpose()
    }
    async fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let session = self.session()?;
        let rows = self
            .statement(async {
                if let Some(end) = end {
                    let query = session
                        .prepare_cached(
                            "SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 AND id<$3 ORDER BY id LIMIT $4",
                        )
                        .await?;
                    session.query(&query, &[&kind, &start, &end, &limit]).await
                } else {
                    let query = session
                        .prepare_cached("SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 ORDER BY id LIMIT $3")
                        .await?;
                    session.query(&query, &[&kind, &start, &limit]).await
                }
            })
            .await?;
        self.backend.metrics.read(rows.len());
        rows.into_iter()
            .map(|r| Ok(((kind.to_owned(), r.get(0)), serde_json::from_str(&r.get::<_, String>(1))?)))
            .collect()
    }
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        if self.backend.read_only {
            return Err(ReadOnly.into());
        }
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        let value = serde_json::to_string(value)?;
        let session = self.session()?;
        // Concurrent inserts of the same absent key become a serializable
        // conflict, rather than an ambiguous retry.
        self.statement(async {
            let query = session
                .prepare_cached(
                    "INSERT INTO ouro_rows(kind,id,value) VALUES ($1,$2,$3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",
                )
                .await?;
            session.execute(&query, &[&kind, &id, &value]).await
        })
        .await?;
        Ok(())
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        if self.backend.read_only {
            return Err(ReadOnly.into());
        }
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        let session = self.session()?;
        self.statement(async {
            let query = session.prepare_cached("DELETE FROM ouro_rows WHERE kind=$1 AND id=$2").await?;
            session.execute(&query, &[&kind, &id]).await
        })
        .await?;
        Ok(())
    }
    async fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        self.backend.faults.check(CommitFault::BeforeCommit)?;
        let session = self.session()?;
        let answered = match tokio::time::timeout(DEADLINE, session.batch_execute("COMMIT")).await {
            Ok(answered) => answered,
            Err(_) => anyhow::bail!("Postgres commit exceeded 30 seconds; commit outcome may be unknown"),
        };
        // The server answered, so the transaction is over either way and
        // the session can serve another one.
        if answered.as_ref().map_or_else(|e| e.as_db_error().is_some(), |()| true) {
            self.session.take();
        }
        answered.map_err(|e| {
            let e = definite(e);
            if e.is::<Conflict>() {
                self.backend.metrics.conflict();
            }
            e
        })?;
        self.backend.faults.check(CommitFault::AfterCommit)
    }
    async fn rollback(mut self: Box<Self>) {
        if let Some(session) = &self.session
            && let Ok(Ok(())) = tokio::time::timeout(DEADLINE, session.batch_execute("ROLLBACK")).await
        {
            self.session.take();
        }
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
async fn migrate(pool: &Pool) -> anyhow::Result<()> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(716532018); CREATE TABLE IF NOT EXISTS ouro_migrations (version INTEGER PRIMARY KEY);").await?;
    let version: i32 = tx.query_one("SELECT COALESCE(MAX(version),0) FROM ouro_migrations", &[]).await?.get(0);
    if i64::from(version) > SCHEMA {
        return Err(SchemaError::Unsupported { found: version.into(), supported: SCHEMA }.into());
    }
    if version == 0 {
        tx.batch_execute(include_str!("../migrations/postgres/001_initial.sql")).await?;
        tx.execute("INSERT INTO ouro_migrations VALUES (1)", &[]).await?;
    }
    tx.commit().await?;
    Ok(())
}
/// The recorded schema version, read without writing: no migrations table
/// (or no recorded version) is an unmigrated store.
async fn schema(pool: &Pool) -> anyhow::Result<()> {
    let client = pool.get().await?;
    let present: bool = client.query_one("SELECT to_regclass('ouro_migrations') IS NOT NULL", &[]).await?.get(0);
    let version: i32 = if present {
        client.query_one("SELECT COALESCE(MAX(version),0) FROM ouro_migrations", &[]).await?.get(0)
    } else {
        0
    };
    Ok(SchemaError::check(version.into(), SCHEMA)?)
}
pub async fn connect(url: &str) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(PostgresBackend::connect(url).await?)))
}
/// A store over `PostgresBackend::connect_read_only`: queries and `Control`
/// queries only; every mutating operation is refused with `ReadOnly`.
pub async fn connect_read_only(url: &str) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(PostgresBackend::connect_read_only(url).await?)))
}
async fn bounded<T>(operation: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(DEADLINE, operation)
        .await
        .map_err(|_| anyhow::anyhow!("Postgres operation exceeded 30 seconds; commit outcome may be unknown"))?
}
