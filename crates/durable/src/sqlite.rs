//! Embedded SQLite adapter. WAL and FULL durability are set on every open.
use crate::{AtomicBackend, CommitFault, FaultInjector, Rows, Snapshot, TransactionalStore, backend::changes};
use rusqlite::{Connection, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
pub struct SqliteBackend {
    connection: Mutex<Connection>,
    pub faults: Arc<FaultInjector>,
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
        Ok(Self { connection: Mutex::new(connection), faults: Arc::new(FaultInjector::default()) })
    }
}
impl AtomicBackend for SqliteBackend {
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let mut connection = self.connection.lock().unwrap();
        let tx = connection.transaction()?;
        let revision = tx.query_row("SELECT revision FROM ouro_revision WHERE id=1", [], |r| r.get(0))?;
        let rows = {
            let mut query = tx.prepare("SELECT kind,id,value FROM ouro_rows ORDER BY kind,id")?;
            let rows = query
                .query_map([], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, String>(1)?), r.get::<_, String>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|(k, v)| Ok((k, serde_json::from_str(&v)?))).collect::<anyhow::Result<_>>()?
        };
        tx.commit()?;
        Ok(Snapshot { revision, rows })
    }
    fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool> {
        let mut connection = self.connection.lock().unwrap();
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed =
            tx.execute("UPDATE ouro_revision SET revision=revision+1 WHERE id=1 AND revision=?1", [before.revision])?;
        if changed == 0 {
            return Ok(false);
        }
        for (i, ((kind, id), value)) in changes(&before.rows, rows).into_iter().enumerate() {
            self.faults.check(CommitFault::BeforeWrite(i))?;
            if let Some(value) = value {
                tx.execute("INSERT INTO ouro_rows(kind,id,value) VALUES (?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value", params![kind,id,serde_json::to_string(value)?])?;
            } else {
                tx.execute("DELETE FROM ouro_rows WHERE kind=?1 AND id=?2", params![kind, id])?;
            }
        }
        self.faults.check(CommitFault::BeforeCommit)?;
        tx.commit()?;
        self.faults.check(CommitFault::AfterCommit)?;
        Ok(true)
    }
}
pub fn open(path: impl AsRef<Path>) -> anyhow::Result<TransactionalStore> {
    Ok(TransactionalStore::new(Arc::new(SqliteBackend::open(path)?)))
}
