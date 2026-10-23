//! In-memory transactional adapter. Failed transactions discard their staged
//! rows; ambiguous commits retain them, just like persistent adapters.
use crate::{AtomicBackend, CommitFault, FaultInjector, Rows, Snapshot, TransactionalStore, backend::changes};
use std::sync::{Arc, Mutex};
#[derive(Default)]
pub struct MemoryBackend {
    state: Mutex<Snapshot>,
    pub faults: Arc<FaultInjector>,
}
impl AtomicBackend for MemoryBackend {
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        Ok(self.state.lock().unwrap().clone())
    }
    fn compare_exchange(&self, before: &Snapshot, rows: &Rows) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if state.revision != before.revision {
            return Ok(false);
        }
        for (i, _) in changes(&before.rows, rows).iter().enumerate() {
            self.faults.check(CommitFault::BeforeWrite(i))?;
        }
        self.faults.check(CommitFault::BeforeCommit)?;
        *state = Snapshot {
            revision: before.revision.checked_add(1).ok_or_else(|| anyhow::anyhow!("revision overflow"))?,
            rows: rows.clone(),
        };
        self.faults.check(CommitFault::AfterCommit)?;
        Ok(true)
    }
}
pub fn memory() -> TransactionalStore {
    TransactionalStore::new(Arc::new(MemoryBackend::default()))
}
