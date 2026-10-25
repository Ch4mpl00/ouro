//! Embedded SQLite adapter. WAL and FULL durability are set on every open.
//!
//! rusqlite is synchronous, so a transaction holds the backend's one
//! connection and runs each statement on Tokio's blocking pool: no async
//! worker waits on the disk, and a backend has at most one statement in
//! flight (SQLite admits one writer anyway). Any runtime flavour works,
//! `current_thread` included; one is required, since statements are handed
//! to its blocking pool.
use crate::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// The schema version this build reads and migrates to (`ouro_migrations`).
/// The file's own `user_version` belongs to the host and is never read or set.
const SCHEMA: i64 = 1;

pub struct SqliteBackend {
    connection: Arc<Mutex<Connection>>,
    read_only: bool,
    pub faults: Arc<FaultInjector>,
    pub metrics: Arc<Metrics>,
}
impl SqliteBackend {
    /// Opens (and migrates) the database file. Blocking: call it while
    /// composing the host, before serving.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Rows without a recorded version are v0.1.0's: refused before
        // anything is created in the file.
        if recorded(&tx)? == 0 && has_table(&tx, "ouro_rows")? {
            return Err(SchemaError::Legacy.into());
        }
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ouro_migrations (version INTEGER PRIMARY KEY);")?;
        let version = recorded(&tx)?;
        if version > SCHEMA {
            return Err(SchemaError::Unsupported { found: version, supported: SCHEMA }.into());
        }
        if version == 0 {
            tx.execute_batch(include_str!("../migrations/sqlite/001_initial.sql"))?;
            tx.execute("INSERT INTO ouro_migrations VALUES (1)", [])?;
        }
        tx.commit()?;
        Ok(Self::with(connection, false))
    }
    /// Opens an existing database file for queries only: it is never
    /// created or migrated and nothing is ever written. A database without
    /// the schema, or with another version of it, is refused with a
    /// `SchemaError`; every write is refused with `ReadOnly`, by the
    /// backend and by SQLite itself (a read-only connection with
    /// `query_only`). Transactions are deferred: each reads one snapshot
    /// without taking the writer's lock, so a console never delays a
    /// worker. Blocking, like `open`.
    pub fn open_read_only(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let connection = Connection::open_with_flags(path, flags)?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.execute_batch("PRAGMA query_only = ON;")?;
        // No migrations table (or no recorded version) is an unmigrated
        // store, unless it holds rows: those are a v0.1.0 file's.
        let version = recorded(&connection)?;
        if version == 0 && has_table(&connection, "ouro_rows")? {
            return Err(SchemaError::Legacy.into());
        }
        SchemaError::check(version, SCHEMA)?;
        Ok(Self::with(connection, true))
    }
    fn with(connection: Connection, read_only: bool) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
            read_only,
            faults: Arc::new(FaultInjector::default()),
            metrics: Arc::new(Metrics::default()),
        }
    }
}

fn has_table(connection: &Connection, name: &str) -> rusqlite::Result<bool> {
    connection.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))
}
/// The recorded schema version (`ouro_migrations`), 0 when there is none.
fn recorded(connection: &Connection) -> rusqlite::Result<i64> {
    if !has_table(connection, "ouro_migrations")? {
        return Ok(0);
    }
    connection.query_row("SELECT COALESCE(MAX(version),0) FROM ouro_migrations", [], |r| r.get(0))
}

/// The connection while a transaction is open on it. Dropped uncommitted,
/// wherever that happens (with a dropped future, or in a blocking task that
/// finishes after its caller went away), it rolls back before the
/// connection is released to the next transaction.
struct Session {
    connection: OwnedMutexGuard<Connection>,
    open: bool,
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.open {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }
}
/// Runs `op` on the blocking pool with the session, and hands both back.
async fn blocking<T: Send + 'static>(
    mut session: Session,
    op: impl FnOnce(&mut Session) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<(Session, T)> {
    let (session, result) = tokio::task::spawn_blocking(move || {
        let result = op(&mut session);
        (session, result)
    })
    .await?;
    Ok((session, result?))
}

struct SqliteTransaction<'a> {
    session: Option<Session>,
    backend: &'a SqliteBackend,
    writes: usize,
}
impl SqliteTransaction<'_> {
    async fn run<T: Send + 'static>(
        &mut self,
        op: impl FnOnce(&mut Session) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let session = self.session.take().ok_or_else(|| anyhow::anyhow!("the SQLite transaction has ended"))?;
        let (session, result) = blocking(session, op).await?;
        self.session = Some(session);
        Ok(result)
    }
}
impl Drop for SqliteTransaction<'_> {
    fn drop(&mut self) {
        // Roll back on the blocking pool rather than on an async worker. A
        // runtime that refuses the task (shutting down) drops it, and the
        // session's own drop rolls back where that happens.
        if let Some(session) = self.session.take().filter(|s| s.open)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn_blocking(move || drop(session));
        }
    }
}
#[async_trait::async_trait]
impl AtomicBackend for SqliteBackend {
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        let connection = self.connection.clone().lock_owned().await;
        let begin = if self.read_only { "BEGIN DEFERRED" } else { "BEGIN IMMEDIATE" };
        let (session, ()) = blocking(Session { connection, open: false }, move |s| {
            s.connection.execute_batch(begin)?;
            s.open = true;
            Ok(())
        })
        .await?;
        Ok(Box::new(SqliteTransaction { session: Some(session), backend: self, writes: 0 }))
    }
    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let connection = self.connection.clone().lock_owned().await;
        let (_, raw) = blocking(Session { connection, open: false }, |s| {
            let mut query = s.connection.prepare("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id")?;
            let raw = query
                .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get::<_, String>(2)?)))?
                .collect::<rusqlite::Result<Vec<(Key, String)>>>()?;
            Ok(raw)
        })
        .await?;
        self.metrics.read(raw.len());
        Ok(Snapshot {
            rows: raw.into_iter().map(|(k, v)| Ok((k, serde_json::from_str(&v)?))).collect::<anyhow::Result<_>>()?,
        })
    }
}
#[async_trait::async_trait]
impl Transaction for SqliteTransaction<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        let (kind, id) = (kind.to_owned(), id.to_owned());
        let value: Option<String> = self
            .run(move |s| {
                let mut query = s.connection.prepare_cached("SELECT value FROM ouro_rows WHERE kind=?1 AND id=?2")?;
                Ok(query.query_row(params![kind, id], |r| r.get(0)).optional()?)
            })
            .await?;
        self.backend.metrics.read(usize::from(value.is_some()));
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    async fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = if end.is_some() {
            "SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND id<?3 ORDER BY id LIMIT ?4"
        } else {
            "SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND ?3 IS NULL ORDER BY id LIMIT ?4"
        };
        let (owned, start, end) = (kind.to_owned(), start.to_owned(), end.map(str::to_owned));
        let raw = self
            .run(move |s| {
                let mut query = s.connection.prepare_cached(sql)?;
                let raw = query
                    .query_map(params![owned, start, end, limit], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(raw)
            })
            .await?;
        self.backend.metrics.read(raw.len());
        raw.into_iter().map(|(id, v)| Ok(((kind.into(), id), serde_json::from_str(&v)?))).collect()
    }
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        if self.backend.read_only {
            return Err(ReadOnly.into());
        }
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        let (kind, id, value) = (kind.to_owned(), id.to_owned(), serde_json::to_string(value)?);
        self.run(move |s| {
            let mut query = s.connection.prepare_cached(
                "INSERT INTO ouro_rows(kind,id,value) VALUES (?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",
            )?;
            query.execute(params![kind, id, value])?;
            Ok(())
        })
        .await
    }
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        if self.backend.read_only {
            return Err(ReadOnly.into());
        }
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        let (kind, id) = (kind.to_owned(), id.to_owned());
        self.run(move |s| {
            let mut query = s.connection.prepare_cached("DELETE FROM ouro_rows WHERE kind=?1 AND id=?2")?;
            query.execute(params![kind, id])?;
            Ok(())
        })
        .await
    }
    async fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        self.backend.faults.check(CommitFault::BeforeCommit)?;
        self.run(|s| {
            s.connection.execute_batch("COMMIT")?;
            s.open = false;
            Ok(())
        })
        .await?;
        self.backend.faults.check(CommitFault::AfterCommit)
    }
}
pub fn open(path: impl AsRef<Path>) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(SqliteBackend::open(path)?)))
}
/// A store over `SqliteBackend::open_read_only`: queries and `Control`
/// queries only; every mutating operation is refused with `ReadOnly`.
pub fn open_read_only(path: impl AsRef<Path>) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(SqliteBackend::open_read_only(path)?)))
}
