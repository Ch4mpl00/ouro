//! Minimal adapter seam: consistent ordered snapshots and atomic CAS writes.
//! The transition engine is shared by the bundled adapters. A third-party
//! adapter may implement RunStore directly or supply this smaller seam.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub type Rows = BTreeMap<(String, String), Value>;
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub revision: i64,
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

pub trait AtomicBackend: Send + Sync {
    fn snapshot(&self) -> anyhow::Result<Snapshot>;
    /// False means another writer committed first; no writes may be visible.
    fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool>;
}

pub struct TransactionalStore {
    pub(crate) backend: Arc<dyn AtomicBackend>,
}
impl TransactionalStore {
    pub fn new(backend: Arc<dyn AtomicBackend>) -> Self {
        Self { backend }
    }
    pub(crate) fn update<T>(&self, operation: impl Fn(&mut Rows) -> anyhow::Result<T>) -> anyhow::Result<T> {
        // CAS conflicts are ordinary concurrency, not function retries.
        for _ in 0..1024 {
            let before = self.backend.snapshot()?;
            let mut rows = before.rows.clone();
            let result = operation(&mut rows)?;
            if self.backend.compare_exchange(&before, &rows)? {
                return Ok(result);
            }
            std::thread::yield_now();
        }
        anyhow::bail!("storage contention exceeded transaction retry limit")
    }
}

pub(crate) fn changes<'a>(before: &'a Rows, after: &'a Rows) -> Vec<(&'a (String, String), Option<&'a Value>)> {
    let mut result = Vec::new();
    for (key, value) in after {
        if before.get(key) != Some(value) {
            result.push((key, Some(value)));
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            result.push((key, None));
        }
    }
    result
}
