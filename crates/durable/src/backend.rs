//! Ordered transactional adapter seam. The engine owns keys and indexes;
//! adapters provide serializable scoped reads and durable atomic commits.
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub type Key = (String, String);
pub type Rows = BTreeMap<Key, Value>;
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub rows: Rows,
}

/// Failure injection includes each row write and an ambiguous successful
/// commit. Adapters exercise their real rollback path, not a fake store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    BeforeWrite(usize),
    BeforeCommit,
    AfterCommit,
}
#[derive(Default)]
pub struct FaultInjector(Mutex<Option<CommitFault>>);
impl FaultInjector {
    pub fn arm(&self, fault: CommitFault) {
        *self.0.lock().unwrap() = Some(fault);
    }
    pub fn check(&self, point: CommitFault) -> anyhow::Result<()> {
        let mut fault = self.0.lock().unwrap();
        if *fault == Some(point) {
            *fault = None;
            anyhow::bail!("injected storage failure at {point:?}");
        }
        Ok(())
    }
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoCounts {
    pub rows_read: u64,
    pub rows_written: u64,
    pub conflicts: u64,
}
/// Counts actual returned rows and attempted row mutations, including retries.
/// Empty point/range probes return no rows. Counters are per backend handle.
#[derive(Default)]
pub struct Metrics {
    read: AtomicU64,
    written: AtomicU64,
    conflicts: AtomicU64,
}
impl Metrics {
    pub fn counts(&self) -> IoCounts {
        IoCounts {
            rows_read: self.read.load(Ordering::Relaxed),
            rows_written: self.written.load(Ordering::Relaxed),
            conflicts: self.conflicts.load(Ordering::Relaxed),
        }
    }
    pub fn reset(&self) {
        self.read.store(0, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.conflicts.store(0, Ordering::Relaxed);
    }
    pub(crate) fn read(&self, n: usize) {
        self.read.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
    pub(crate) fn write(&self) {
        self.written.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn conflict(&self) {
        self.conflicts.fetch_add(1, Ordering::Relaxed);
    }
}

/// A serializable transaction, including read-only operations and empty range
/// reads (phantoms). Ordering is binary UTF-8, ranges are [start, end), and a
/// scan returns at most `limit` rows. Reads see this transaction's writes.
/// Dropping without commit rolls back, at any await point: a dropped
/// operation leaves nothing applied, and a dropped `commit` either committed
/// as a whole or not at all (an ambiguous outcome the caller never learns).
/// Only definite serialization failures may return Conflict; transport/commit
/// errors must never be retried.
#[async_trait]
pub trait Transaction: Send {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>>;
    async fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows>;
    async fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()>;
    async fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()>;
    async fn commit(self: Box<Self>) -> anyhow::Result<()>;
    /// Ends the transaction without committing. Dropping it does the same;
    /// an adapter whose rollback needs a round trip (to keep a pooled
    /// connection) does it here, on the engine's error path.
    async fn rollback(self: Box<Self>) {}
}
#[derive(Debug)]
pub struct Conflict;
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transaction serialization conflict")
    }
}
impl std::error::Error for Conflict {}

/// A write through a store opened read-only, refused before it touches data.
#[derive(Debug)]
pub struct ReadOnly;
impl std::fmt::Display for ReadOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the store was opened read-only")
    }
}
impl std::error::Error for ReadOnly {}

/// A mutation attempted inside a read transaction, refused before touching data.
#[derive(Debug)]
pub struct ReadOnlyTransaction;
impl std::fmt::Display for ReadOnlyTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a read transaction cannot mutate data")
    }
}
impl std::error::Error for ReadOnlyTransaction {}

pub(crate) struct ReadTransaction<'a>(pub(crate) Box<dyn Transaction + 'a>);
#[async_trait]
impl Transaction for ReadTransaction<'_> {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.0.get(kind, id).await
    }
    async fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        self.0.scan(kind, start, end, limit).await
    }
    async fn put(&mut self, _kind: &str, _id: &str, _value: &Value) -> anyhow::Result<()> {
        Err(ReadOnlyTransaction.into())
    }
    async fn delete(&mut self, _kind: &str, _id: &str) -> anyhow::Result<()> {
        Err(ReadOnlyTransaction.into())
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.0.commit().await
    }
    async fn rollback(self: Box<Self>) {
        self.0.rollback().await;
    }
}

/// Why a store cannot be opened without migrating it: an adapter's
/// read-only open never creates or migrates a schema, so it refuses a
/// database that has none yet or one this build does not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    /// No schema: the database was never opened writable (migrated), or it
    /// is not an ouro store.
    Unmigrated,
    /// A schema version other than the one this build reads.
    Unsupported { found: i64, supported: i64 },
    /// The layout of an earlier release, which recorded no schema version
    /// in the store itself (SQLite's v0.1.0 rows). It is not converted.
    Legacy,
}
impl SchemaError {
    /// The refusal for a recorded schema `version` (0: none).
    pub(crate) fn check(version: i64, supported: i64) -> Result<(), SchemaError> {
        match version {
            0 => Err(SchemaError::Unmigrated),
            v if v == supported => Ok(()),
            found => Err(SchemaError::Unsupported { found, supported }),
        }
    }
}
impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unmigrated => f.write_str("the store has no schema; open it writable once to migrate it"),
            Self::Unsupported { found, supported } => {
                write!(f, "unsupported store schema version {found} (this build reads {supported})")
            }
            Self::Legacy => f.write_str("the store has the layout of an earlier release, which is not converted"),
        }
    }
}
impl std::error::Error for SchemaError {}

#[async_trait]
pub trait AtomicBackend: Send + Sync {
    async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>>;
    /// A serializable read transaction, including absence and bounded ranges.
    /// Both mutation methods fail with `ReadOnlyTransaction` before touching
    /// data. The default preserves `begin`'s isolation and ending behavior.
    async fn begin_read(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        Ok(Box::new(ReadTransaction(self.begin().await?)))
    }
    /// Opened read-only: `begin` transactions refuse `put` and `delete`
    /// with `ReadOnly` before touching data, and `TransactionalStore`
    /// refuses mutating operations before opening a transaction.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Diagnostics only: full persisted-state audits, never used by RunStore.
    async fn snapshot(&self) -> anyhow::Result<Snapshot>;
    /// Diagnostics/corruption injection only; validates the supplied rows.
    async fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool> {
        let mut tx = self.begin().await?;
        for ((kind, id), value) in &before.rows {
            if tx.get(kind, id).await?.as_ref() != Some(value) {
                return Ok(false);
            }
        }
        for (key, value) in changes(&before.rows, rows) {
            if let Some(value) = value {
                tx.put(&key.0, &key.1, value).await?;
            } else {
                tx.delete(&key.0, &key.1).await?;
            }
        }
        match tx.commit().await {
            Ok(()) => Ok(true),
            Err(e) if e.is::<Conflict>() => Ok(false),
            Err(e) => Err(e),
        }
    }
}

pub struct TransactionalStore {
    pub(crate) backend: Arc<dyn AtomicBackend>,
    pub(crate) policy: crate::Policy,
}
impl TransactionalStore {
    pub fn new(backend: Arc<dyn AtomicBackend>) -> Self {
        Self { backend, policy: crate::Policy::default() }
    }
    /// The mailbox and tombstone lifetimes and the clock skew bound.
    pub fn with_policy(mut self, policy: crate::Policy) -> Self {
        self.policy = policy;
        self
    }
    /// Runs a mutating `operation` (`read` otherwise); a read-only backend
    /// refuses it with `ReadOnly` before opening a transaction, whether or
    /// not it would write.
    pub(crate) async fn update<'c, T: Send>(
        &'c self,
        operation: impl for<'t> Fn(&'t mut (dyn Transaction + 'c)) -> BoxFuture<'t, anyhow::Result<T>> + Send + Sync,
    ) -> anyhow::Result<T> {
        if self.backend.is_read_only() {
            return Err(ReadOnly.into());
        }
        self.transact(false, operation).await
    }
    /// Runs a pure query in a guarded read transaction, including on a
    /// read-only backend. The store guard also protects against adapters
    /// whose `begin_read` override forgets to refuse mutations.
    pub(crate) async fn read<'c, T: Send>(
        &'c self,
        operation: impl for<'t> Fn(&'t mut (dyn Transaction + 'c)) -> BoxFuture<'t, anyhow::Result<T>> + Send + Sync,
    ) -> anyhow::Result<T> {
        self.transact(true, operation).await
    }
    /// Retries definite conflicts only. Each attempt derives its result
    /// from its own reads. Dropping the call rolls the open transaction back.
    async fn transact<'c, T: Send>(
        &'c self,
        read: bool,
        operation: impl for<'t> Fn(&'t mut (dyn Transaction + 'c)) -> BoxFuture<'t, anyhow::Result<T>> + Send + Sync,
    ) -> anyhow::Result<T> {
        for attempt in 0..128 {
            let transaction = if read {
                self.backend.begin_read().await.map(|tx| {
                    let guarded: Box<dyn Transaction> = Box::new(ReadTransaction(tx));
                    guarded
                })
            } else {
                self.backend.begin().await
            };
            let result = match transaction {
                Ok(mut tx) => match operation(&mut *tx).await {
                    Ok(value) => tx.commit().await.map(|()| value),
                    Err(e) => {
                        tx.rollback().await;
                        Err(e)
                    }
                },
                Err(e) => Err(e),
            };
            match result {
                Err(e) if e.is::<Conflict>() => {
                    // Independent workers must not retry in lockstep. Jitter
                    // stays below the existing per-attempt backoff ceiling.
                    let ceiling = 50 * (1_u64 << attempt.min(7));
                    let jitter = uuid::Uuid::new_v4().as_u64_pair().0 % (ceiling / 2 + 1);
                    tokio::time::sleep(std::time::Duration::from_micros(ceiling / 2 + jitter)).await;
                }
                other => return other,
            }
        }
        anyhow::bail!("storage contention exceeded transaction retry limit")
    }
}

pub(crate) fn changes<'a>(before: &'a Rows, after: &'a Rows) -> Vec<(&'a Key, Option<&'a Value>)> {
    after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .map(|(k, v)| (k, Some(v)))
        .chain(before.keys().filter(|k| !after.contains_key(*k)).map(|k| (k, None)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBackend;
    use serde_json::json;

    struct UnguardedBackend(MemoryBackend);
    #[async_trait]
    impl AtomicBackend for UnguardedBackend {
        async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
            self.0.begin().await
        }
        async fn begin_read(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
            self.begin().await
        }
        async fn snapshot(&self) -> anyhow::Result<Snapshot> {
            self.0.snapshot().await
        }
    }

    #[tokio::test]
    async fn store_read_guards_unguarded_backend_override() {
        let backend = Arc::new(UnguardedBackend(MemoryBackend::default()));
        let mut tx = backend.begin().await.unwrap();
        tx.put("guard", "existing", &json!(1)).await.unwrap();
        tx.commit().await.unwrap();
        let before = backend.snapshot().await.unwrap();
        let store = TransactionalStore::new(backend.clone());
        let put = store.read(|tx| Box::pin(async move { tx.put("guard", "new", &json!(2)).await })).await;
        assert!(put.unwrap_err().is::<ReadOnlyTransaction>());
        assert_eq!(backend.snapshot().await.unwrap(), before);
        let delete = store.read(|tx| Box::pin(async move { tx.delete("guard", "existing").await })).await;
        assert!(delete.unwrap_err().is::<ReadOnlyTransaction>());
        assert_eq!(backend.snapshot().await.unwrap(), before);
    }
}
