//! Ordered transactional adapter seam. The engine owns keys and indexes;
//! adapters provide serializable scoped reads and durable atomic commits.
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
/// Dropping without commit rolls back. Only definite serialization failures
/// may return Conflict; transport/commit errors must never be retried.
pub trait Transaction {
    fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>>;
    fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows>;
    fn put(&mut self, kind: &str, id: &str, value: &Value) -> anyhow::Result<()>;
    fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()>;
    fn commit(self: Box<Self>) -> anyhow::Result<()>;
}
#[derive(Debug)]
pub struct Conflict;
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transaction serialization conflict")
    }
}
impl std::error::Error for Conflict {}

pub trait AtomicBackend: Send + Sync {
    fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>>;
    /// Diagnostics only: full persisted-state audits, never used by RunStore.
    fn snapshot(&self) -> anyhow::Result<Snapshot>;
    /// Diagnostics/corruption injection only; validates the supplied rows.
    fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool> {
        let mut tx = self.begin()?;
        for ((kind, id), value) in &before.rows {
            if tx.get(kind, id)?.as_ref() != Some(value) {
                return Ok(false);
            }
        }
        for (key, value) in changes(&before.rows, rows) {
            if let Some(value) = value {
                tx.put(&key.0, &key.1, value)?;
            } else {
                tx.delete(&key.0, &key.1)?;
            }
        }
        match tx.commit() {
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
    pub(crate) fn update<T>(&self, operation: impl Fn(&mut dyn Transaction) -> anyhow::Result<T>) -> anyhow::Result<T> {
        for attempt in 0..128 {
            let result: anyhow::Result<T> = (|| {
                let mut tx = self.backend.begin()?;
                let result = operation(&mut *tx)?;
                tx.commit()?;
                Ok(result)
            })();
            match result {
                Err(e) if e.is::<Conflict>() => {
                    std::thread::sleep(std::time::Duration::from_micros(50 * (1_u64 << attempt.min(7))));
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
