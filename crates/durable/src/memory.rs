//! Optimistic per-key memory transactions. Range observations validate their
//! limited result as well as point reads, including absence and phantoms.
use crate::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
    sync::{Arc, Mutex},
};

type Versioned = BTreeMap<Key, (uuid::Uuid, serde_json::Value)>;
#[derive(Default)]
struct State {
    rows: Versioned,
}
#[derive(Default)]
pub struct MemoryBackend {
    state: Mutex<State>,
    pub faults: Arc<FaultInjector>,
    pub metrics: Arc<Metrics>,
}
struct RangeRead {
    kind: String,
    start: String,
    end: Option<String>,
    limit: usize,
    versions: Vec<(Key, uuid::Uuid)>,
}
struct MemoryTransaction<'a> {
    backend: &'a MemoryBackend,
    reads: BTreeMap<Key, Option<(uuid::Uuid, serde_json::Value)>>,
    scans: Vec<RangeRead>,
    writes: BTreeMap<Key, Option<serde_json::Value>>,
}
fn range<'a>(
    rows: &'a Versioned,
    kind: &str,
    start: &str,
    end: Option<&str>,
    limit: usize,
) -> impl Iterator<Item = (&'a Key, &'a (uuid::Uuid, serde_json::Value))> {
    let upper = end.map_or_else(|| (format!("{kind}\0"), String::new()), |end| (kind.into(), end.into()));
    rows.range((Bound::Included((kind.into(), start.into())), Bound::Excluded(upper))).take(limit)
}
impl AtomicBackend for MemoryBackend {
    fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        Ok(Box::new(MemoryTransaction {
            backend: self,
            reads: BTreeMap::new(),
            scans: Vec::new(),
            writes: BTreeMap::new(),
        }))
    }
    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let state = self.state.lock().unwrap();
        self.metrics.read(state.rows.len());
        Ok(Snapshot { rows: state.rows.iter().map(|(k, (_, v))| (k.clone(), v.clone())).collect() })
    }
}
impl Transaction for MemoryTransaction<'_> {
    fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let key = (kind.into(), id.into());
        if let Some(value) = self.writes.get(&key) {
            return Ok(value.clone());
        }
        let value = self.reads.entry(key.clone()).or_insert_with(|| {
            let value = self.backend.state.lock().unwrap().rows.get(&key).cloned();
            self.backend.metrics.read(usize::from(value.is_some()));
            value
        });
        Ok(value.as_ref().map(|(_, v)| v.clone()))
    }
    fn scan(&mut self, kind: &str, start: &str, end: Option<&str>, limit: usize) -> anyhow::Result<Rows> {
        if limit == 0 || end.is_some_and(|end| end <= start) {
            return Ok(Rows::new());
        }
        let read_limit = limit.saturating_add(self.writes.len());
        let state = self.backend.state.lock().unwrap();
        let observed: Vec<_> = range(&state.rows, kind, start, end, read_limit).collect();
        self.backend.metrics.read(observed.len());
        self.scans.push(RangeRead {
            kind: kind.into(),
            start: start.into(),
            end: end.map(str::to_owned),
            limit: read_limit,
            versions: observed.iter().map(|(k, (v, _))| ((*k).clone(), *v)).collect(),
        });
        let mut rows: Rows = observed.into_iter().map(|(k, (_, v))| (k.clone(), v.clone())).collect();
        for ((k, id), value) in &self.writes {
            if k == kind && id.as_str() >= start && end.is_none_or(|end| id.as_str() < end) {
                if let Some(value) = value {
                    rows.insert((k.clone(), id.clone()), value.clone());
                } else {
                    rows.remove(&(k.clone(), id.clone()));
                }
            }
        }
        Ok(rows.into_iter().take(limit).collect())
    }
    fn put(&mut self, kind: &str, id: &str, value: &serde_json::Value) -> anyhow::Result<()> {
        self.get(kind, id)?;
        self.writes.insert((kind.into(), id.into()), Some(value.clone()));
        Ok(())
    }
    fn delete(&mut self, kind: &str, id: &str) -> anyhow::Result<()> {
        self.get(kind, id)?;
        self.writes.insert((kind.into(), id.into()), None);
        Ok(())
    }
    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let mut state = self.backend.state.lock().unwrap();
        // A range validates every returned key's version below. A point
        // observation of that same version needs no second lookup; a point
        // that saw another version must still be checked independently.
        let ranged: BTreeSet<_> =
            self.scans.iter().flat_map(|scan| scan.versions.iter().map(|(key, version)| (key, *version))).collect();
        for (key, before) in &self.reads {
            if before.as_ref().is_some_and(|(version, _)| ranged.contains(&(key, *version))) {
                continue;
            }
            let current = state.rows.get(key);
            self.backend.metrics.read(usize::from(current.is_some()));
            if current.map(|(v, _)| v) != before.as_ref().map(|(v, _)| v) {
                self.backend.metrics.conflict();
                return Err(Conflict.into());
            }
        }
        for scan in &self.scans {
            let current: Vec<_> = range(&state.rows, &scan.kind, &scan.start, scan.end.as_deref(), scan.limit)
                .map(|(k, (v, _))| (k.clone(), *v))
                .collect();
            self.backend.metrics.read(current.len());
            if current != scan.versions {
                self.backend.metrics.conflict();
                return Err(Conflict.into());
            }
        }
        for (i, _) in self.writes.iter().enumerate() {
            self.backend.faults.check(CommitFault::BeforeWrite(i))?;
        }
        self.backend.faults.check(CommitFault::BeforeCommit)?;
        for (key, value) in self.writes {
            self.backend.metrics.write();
            if let Some(value) = value {
                let version = uuid::Uuid::new_v4();
                state.rows.insert(key, (version, value));
            } else {
                state.rows.remove(&key);
            }
        }
        self.backend.faults.check(CommitFault::AfterCommit)
    }
}
pub fn memory() -> TransactionalStore {
    TransactionalStore::new(Arc::new(MemoryBackend::default()))
}
