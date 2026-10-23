//! Embedded SQLite adapter. WAL and FULL durability are set on every open.
use crate::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
pub struct SqliteBackend {
    connection: Mutex<Connection>,
    pub faults: Arc<FaultInjector>,
    pub metrics: Arc<Metrics>,
}
impl SqliteBackend {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(version <= 1, "unsupported SQLite schema version {version}");
        if version == 0 {
            tx.execute_batch(include_str!("../migrations/sqlite/001_initial.sql"))?;
            tx.execute_batch("PRAGMA user_version = 1;")?;
        }
        tx.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
            faults: Arc::new(FaultInjector::default()),
            metrics: Arc::new(Metrics::default()),
        })
    }
}
struct SqliteTransaction<'a> {
    connection: MutexGuard<'a, Connection>,
    backend: &'a SqliteBackend,
    writes: usize,
    committed: bool,
}
impl Drop for SqliteTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }
}
impl AtomicBackend for SqliteBackend {
    fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        let connection = self.connection.lock().unwrap();
        connection.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Box::new(SqliteTransaction { connection, backend: self, writes: 0, committed: false }))
    }
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let connection = self.connection.lock().unwrap();
        let mut query = connection.prepare("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id")?;
        let raw = query
            .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.metrics.read(raw.len());
        Ok(Snapshot {
            rows: raw.into_iter().map(|(k, v)| Ok((k, serde_json::from_str(&v)?))).collect::<anyhow::Result<_>>()?,
        })
    }
}
impl Transaction for SqliteTransaction<'_> {
    fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        let value: Option<String> = self
            .connection
            .query_row("SELECT value FROM ouro_rows WHERE kind=?1 AND id=?2", params![kind, id], |r| r.get(0))
            .optional()?;
        self.backend.metrics.read(usize::from(value.is_some()));
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = if end.is_some() {
            "SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND id<?3 ORDER BY id LIMIT ?4"
        } else {
            "SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND ?3 IS NULL ORDER BY id LIMIT ?4"
        };
        let mut query = self.connection.prepare_cached(sql)?;
        let raw = query
            .query_map(params![kind, start, end, limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        self.backend.metrics.read(raw.len());
        raw.into_iter().map(|(id, v)| Ok(((kind.into(), id), serde_json::from_str(&v)?))).collect()
    }
    fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()> {
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        self.connection.execute("INSERT INTO ouro_rows(kind,id,value) VALUES (?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value",params![kind,id,serde_json::to_string(value)?])?;
        Ok(())
    }
    fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.backend.faults.check(CommitFault::BeforeWrite(self.writes))?;
        self.writes += 1;
        self.backend.metrics.write();
        self.connection.execute("DELETE FROM ouro_rows WHERE kind=?1 AND id=?2", params![kind, id])?;
        Ok(())
    }
    fn commit(mut self: Box<Self>) -> anyhow::Result<()> {
        self.backend.faults.check(CommitFault::BeforeCommit)?;
        self.connection.execute_batch("COMMIT")?;
        self.committed = true;
        self.backend.faults.check(CommitFault::AfterCommit)
    }
}
pub fn open(path: impl AsRef<Path>) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(SqliteBackend::open(path)?)))
}
