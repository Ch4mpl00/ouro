//! Shared transactional semantics, adapted from the original run store.
use crate::*;
use owf::{durable::check_transition, run::WorkflowError};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

// Journal bytes are decoded only by journal operations. Corrupt evidence
// prevents replay but must not prevent terminal cancellation or erase itself.
#[derive(Serialize, Deserialize)]
struct StoredRun {
    run: SavedRun,
    records: BTreeMap<String, Value>,
    awaits: BTreeMap<String, i64>,
    listeners: BTreeMap<String, Vec<String>>,
    events: Vec<StoredEvent>,
    #[serde(default)]
    subscriptions: BTreeMap<String, Vec<String>>,
}

fn key(kind: &str, id: &str) -> (String, String) {
    (kind.into(), id.into())
}
fn get<T: DeserializeOwned>(rows: &Rows, kind: &str, id: &str) -> anyhow::Result<Option<T>> {
    rows.get(&key(kind, id)).cloned().map(serde_json::from_value).transpose().map_err(Into::into)
}
fn put(rows: &mut Rows, kind: &str, id: &str, value: &impl Serialize) -> anyhow::Result<()> {
    rows.insert(key(kind, id), serde_json::to_value(value)?);
    Ok(())
}
fn run(rows: &Rows, id: &str) -> anyhow::Result<StoredRun> {
    get(rows, "run", id)?.ok_or_else(|| anyhow::anyhow!("unknown run {id}"))
}
fn store_run(rows: &mut Rows, snapshot: &StoredRun) -> anyhow::Result<()> {
    put(rows, "run", &snapshot.run.admission.id, snapshot)
}
fn checked(rows: &Rows, fence: &Fence, now: i64) -> anyhow::Result<StoredRun> {
    let snapshot = run(rows, &fence.run_id)?;
    anyhow::ensure!(
        snapshot.run.state == RunState::Running
            && snapshot.run.fence.as_ref() == Some(fence)
            && snapshot.run.lease_until_ms.is_some_and(|until| until > now),
        "run {} lease expired or was replaced",
        fence.run_id
    );
    Ok(snapshot)
}
fn all_runs(rows: &Rows) -> anyhow::Result<Vec<StoredRun>> {
    rows.iter().filter(|((kind, _), _)| kind == "run").map(|(_, v)| Ok(serde_json::from_value(v.clone())?)).collect()
}
fn claimable(r: &SavedRun, now: i64) -> bool {
    match r.state {
        RunState::Ready | RunState::Waiting => r.wake_at_ms <= now,
        RunState::Running => r.lease_until_ms.is_some_and(|at| at <= now),
        _ => false,
    }
}
fn claim_run(snapshot: &mut StoredRun, now: i64, owner: &str) -> anyhow::Result<Fence> {
    let r = &mut snapshot.run;
    r.generation = r.generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("fence generation overflow"))?;
    let fence = Fence { run_id: r.admission.id.clone(), owner: owner.into(), generation: r.generation };
    r.fence = Some(fence.clone());
    r.lease_until_ms = Some(now.saturating_add(LEASE_MS));
    r.state = RunState::Running;
    Ok(fence)
}
fn insert(rows: &mut Rows, admission: &Admission, now: i64) -> anyhow::Result<bool> {
    if let Some(existing) = get::<StoredRun>(rows, "run", &admission.id)? {
        anyhow::ensure!(existing.run.admission.name == admission.name, "run already belongs to another definition");
        return Ok(false);
    }
    store_run(
        rows,
        &StoredRun {
            run: SavedRun {
                admission: admission.clone(),
                state: RunState::Ready,
                fence: None,
                generation: 0,
                lease_until_ms: None,
                wake_at_ms: now,
                interruptions: 0,
                outcome: None,
                ended_ms: None,
                child: None,
            },
            records: BTreeMap::new(),
            awaits: BTreeMap::new(),
            listeners: BTreeMap::new(),
            events: Vec::new(),
            subscriptions: BTreeMap::new(),
        },
    )?;
    Ok(true)
}
fn close(snapshot: &mut StoredRun, types: &[String]) {
    for t in types {
        if !snapshot.listeners.values().any(|types| types.contains(t)) {
            snapshot.awaits.remove(t);
        }
    }
}
fn envelope(event: &StoredEvent) -> anyhow::Result<Value> {
    if let Some(envelope) = &event.envelope {
        return Ok(envelope.clone());
    }
    Ok(
        json!({"type":event.type_, "data":event.data, "time":chrono::DateTime::from_timestamp_millis(event.delivered_at_ms).ok_or_else(|| anyhow::anyhow!("invalid event timestamp"))?.to_rfc3339()}),
    )
}
fn deliver(rows: &mut Rows, t: &str, data: &Value, now: i64, target: Option<&str>) -> anyhow::Result<Option<String>> {
    let mut eligible: Vec<_> = all_runs(rows)?
        .into_iter()
        .filter(|s| {
            s.run.ended_ms.is_none()
                && target.is_none_or(|id| id == s.run.admission.id)
                && s.awaits.contains_key(t)
                && !s.events.iter().any(|e| e.type_ == t && e.consumed_key.is_none())
        })
        .collect();
    eligible.sort_by_key(|s| (s.awaits[t], s.run.admission.id.clone()));
    let Some(mut snapshot) = eligible.into_iter().next() else {
        return Ok(None);
    };
    snapshot.events.push(StoredEvent {
        type_: t.into(),
        data: data.clone(),
        delivered_at_ms: now,
        consumed_key: None,
        envelope: None,
    });
    snapshot.run.wake_at_ms = now;
    store_run(rows, &snapshot)?;
    Ok(Some(snapshot.run.admission.id))
}
/// Whether a pending event may be taken by `types` (`*` takes published ones).
fn takes(types: &[String], event: &StoredEvent) -> bool {
    types.contains(&event.type_) || (event.envelope.is_some() && types.iter().any(|t| t == "*"))
}
fn subscribed(snapshot: &StoredRun, event: &StoredEvent) -> bool {
    event.envelope.is_some() && snapshot.subscriptions.values().any(|types| takes(types, event))
}
fn same_event(a: &Value, b: &Value) -> bool {
    a.get("id").is_some() && a.get("id") == b.get("id") && a.get("source") == b.get("source")
}
fn publish(rows: &mut Rows, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Vec<String>> {
    let t = event
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("a published event needs a type"))?;
    let stored = StoredEvent {
        type_: t.into(),
        data: event.get("data").cloned().unwrap_or(Value::Null),
        delivered_at_ms: now,
        consumed_key: None,
        envelope: Some(event.clone()),
    };
    let mut receivers = Vec::new();
    for mut snapshot in all_runs(rows)? {
        let eligible = snapshot.run.ended_ms.is_none()
            && match target {
                Some(id) => id == snapshot.run.admission.id,
                None => subscribed(&snapshot, &stored),
            }
            && !snapshot.events.iter().any(|e| e.envelope.as_ref().is_some_and(|e| same_event(e, event)));
        if eligible {
            snapshot.events.push(stored.clone());
            snapshot.run.wake_at_ms = now;
            receivers.push(snapshot.run.admission.id.clone());
            store_run(rows, &snapshot)?;
        }
    }
    Ok(receivers)
}
fn lifecycle(rows: &mut Rows, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
    let Some(link) = run(rows, id)?.run.child else {
        return Ok(());
    };
    let (kind, data) = match status {
        Status::Completed(output) => ("done", json!({"status":"completed","output":output})),
        Status::Faulted(error) => ("done", json!({"status":"faulted","error":error})),
        Status::Cancelled => ("done", json!({"status":"cancelled"})),
        Status::Waiting { .. } => ("waiting", json!({"status":"waiting"})),
        Status::Interrupted(_) => return Ok(()),
    };
    let t = format!("{kind}:{id}");
    if kind == "done" && run(rows, &link.parent)?.events.iter().any(|e| e.type_ == t) {
        return Ok(());
    }
    deliver(rows, &t, &data, now, Some(&link.parent))?;
    Ok(())
}
fn terminate(rows: &mut Rows, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, id)?;
    snapshot.run.state = match status {
        Status::Completed(_) => RunState::Completed,
        Status::Faulted(_) => RunState::Failed,
        _ => RunState::Cancelled,
    };
    snapshot.run.outcome = Some(status.clone());
    snapshot.run.ended_ms = Some(now);
    snapshot.run.fence = None;
    snapshot.run.lease_until_ms = None;
    snapshot.awaits.clear();
    snapshot.listeners.clear();
    snapshot.subscriptions.clear();
    store_run(rows, &snapshot)?;
    // Iterative cascade avoids stack growth on arbitrarily nested child runs.
    let mut parents = vec![id.to_string()];
    while let Some(parent) = parents.pop() {
        for mut child in all_runs(rows)?
            .into_iter()
            .filter(|s| s.run.ended_ms.is_none() && s.run.child.as_ref().is_some_and(|l| l.parent == parent))
        {
            child.run.state = RunState::Cancelled;
            child.run.outcome = Some(Status::Cancelled);
            child.run.ended_ms = Some(now);
            child.run.fence = None;
            child.run.lease_until_ms = None;
            child.awaits.clear();
            child.listeners.clear();
            child.subscriptions.clear();
            parents.push(child.run.admission.id.clone());
            store_run(rows, &child)?;
            lifecycle(rows, &child.run.admission.id, &Status::Cancelled, now)?;
        }
    }
    lifecycle(rows, id, status, now)
}

impl RunStore for TransactionalStore {
    fn admit(&self, a: &Admission, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| insert(rows, a, now))
    }
    fn start(&self, a: &Admission, now: i64) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            if !insert(rows, a, now)? {
                return Ok(None);
            }
            let mut s = run(rows, &a.id)?;
            let fence = claim_run(&mut s, now, &owner)?;
            store_run(rows, &s)?;
            Ok(Some(fence))
        })
    }
    fn claim(&self, id: &str, now: i64) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            let Some(mut s) = get::<StoredRun>(rows, "run", id)? else {
                return Ok(None);
            };
            if !claimable(&s.run, now) {
                return Ok(None);
            }
            let fence = claim_run(&mut s, now, &owner)?;
            store_run(rows, &s)?;
            Ok(Some(fence))
        })
    }
    fn renew(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            s.run.lease_until_ms = Some(now.saturating_add(LEASE_MS));
            store_run(rows, &s)
        })
    }
    fn release(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            s.run.state = RunState::Ready;
            s.run.fence = None;
            s.run.lease_until_ms = None;
            s.run.wake_at_ms = now;
            store_run(rows, &s)
        })
    }
    fn load_records(&self, f: &Fence, now: i64) -> anyhow::Result<BTreeMap<String, Record>> {
        checked(&self.backend.snapshot()?.rows, f, now)?
            .records
            .into_iter()
            .map(|(key, value)| Ok((key, serde_json::from_value(value)?)))
            .collect()
    }
    fn save_record(&self, f: &Fence, k: &str, r: &Record, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            if matches!(r, Record::Event { .. }) {
                anyhow::ensure!(
                    s.records.get(k) == Some(&serde_json::to_value(r)?),
                    "event records must be committed by take_event"
                );
            }
            if let Some(old) = s.records.get(k) {
                check_transition(&serde_json::from_value(old.clone())?, r)?;
            }
            s.records.insert(k.into(), serde_json::to_value(r)?);
            store_run(rows, &s)
        })
    }
    fn register_wait(&self, f: &Fence, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            for t in types {
                s.awaits.entry(t.clone()).or_insert(now);
            }
            store_run(rows, &s)
        })
    }
    fn unregister_wait(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            s.listeners.remove(k);
            close(&mut s, types);
            store_run(rows, &s)
        })
    }
    fn deliver(&self, t: &str, data: &Value, now: i64) -> anyhow::Result<Option<String>> {
        self.update(|rows| deliver(rows, t, data, now, None))
    }
    fn take_event(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<Option<Value>> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            if let Some(saved) = s.records.get(k) {
                let Record::Event { event } = serde_json::from_value(saved.clone())? else {
                    anyhow::bail!("invalid event checkpoint")
                };
                return Ok(Some(event.clone()));
            }
            let candidate = s.events.iter().position(|e| e.consumed_key.is_none() && takes(types, e));
            let result = if let Some(i) = candidate {
                let event = envelope(&s.events[i])?;
                s.events[i].consumed_key = Some(k.into());
                if let Some(child) = s.events[i].type_.strip_prefix("done:") {
                    s.listeners.remove(&format!("child:{child}"));
                }
                s.listeners.remove(k);
                close(&mut s, types);
                s.records.insert(k.into(), serde_json::to_value(Record::Event { event: event.clone() })?);
                Some(event)
            } else {
                s.listeners.insert(k.into(), types.to_vec());
                None
            };
            store_run(rows, &s)?;
            Ok(result)
        })
    }
    fn subscribe(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            s.subscriptions.insert(k.into(), types.to_vec());
            store_run(rows, &s)
        })
    }
    fn unsubscribe(&self, f: &Fence, k: &str, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            s.subscriptions.remove(k);
            store_run(rows, &s)
        })
    }
    fn publish(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Vec<String>> {
        self.update(|rows| publish(rows, event, target, now))
    }
    fn definition_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .backend
            .snapshot()?
            .rows
            .keys()
            .filter(|(kind, _)| kind == "definitions")
            .map(|(_, id)| id.clone())
            .collect())
    }
    fn finish(&self, f: &Fence, outcome: &Status, now: i64, threshold: u32) -> anyhow::Result<Status> {
        self.update(|rows| {
            let mut s = checked(rows, f, now)?;
            let mut status = outcome.clone();
            match &status {
                Status::Waiting { wake_at_ms } => {
                    s.run.state = RunState::Waiting;
                    s.run.interruptions = 0;
                    s.run.wake_at_ms = *wake_at_ms;
                    if s.events
                        .iter()
                        .any(|e| e.consumed_key.is_none() && (s.awaits.contains_key(&e.type_) || subscribed(&s, e)))
                    {
                        s.run.wake_at_ms = now;
                    }
                }
                Status::Interrupted(error) => {
                    s.run.interruptions = s.run.interruptions.saturating_add(1);
                    if s.run.interruptions >= MAX_INTERRUPTIONS {
                        status = Status::Faulted(WorkflowError::runtime(format!(
                            "gave up after {} interruptions: {error}",
                            s.run.interruptions
                        )));
                    } else {
                        s.run.state = RunState::Ready;
                        s.run.wake_at_ms =
                            now.saturating_add((2_000_i64 << (s.run.interruptions - 1).min(20)).min(600_000));
                    }
                }
                _ => {
                    s.run.interruptions = 0;
                }
            }
            s.run.fence = None;
            s.run.lease_until_ms = None;
            store_run(rows, &s)?;
            if matches!(status, Status::Completed(_) | Status::Faulted(_) | Status::Cancelled) {
                // Breaker update shares the terminal commit, so an ambiguous
                // finish or a fast parent cannot count a run twice.
                let name = &s.run.admission.name;
                let mut defs: Definitions = get(rows, "definitions", name)?.unwrap_or_default();
                if defs.active == Some(s.run.admission.version) {
                    match &status {
                        Status::Completed(_) => defs.consecutive_faults = 0,
                        Status::Faulted(_) => {
                            defs.consecutive_faults = defs.consecutive_faults.saturating_add(1);
                            if defs.consecutive_faults >= threshold.max(1) {
                                defs.active = defs.previous.take();
                                defs.consecutive_faults = 0;
                            }
                        }
                        _ => {}
                    }
                    put(rows, "definitions", name, &defs)?;
                }
                terminate(rows, &f.run_id, &status, now)?;
            } else {
                lifecycle(rows, &f.run_id, &status, now)?;
            }
            Ok(status)
        })
    }
    fn start_child(&self, parent: &Fence, a: &Admission, link: &ChildLink, now: i64) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            let mut p = checked(rows, parent, now)?;
            anyhow::ensure!(link.parent == parent.run_id && a.id != parent.run_id, "invalid child parent");
            for t in [format!("done:{}", a.id), format!("waiting:{}", a.id)] {
                p.awaits.entry(t).or_insert(now);
            }
            p.listeners.insert(format!("child:{}", a.id), vec![format!("done:{}", a.id)]);
            store_run(rows, &p)?;
            let inserted = insert(rows, a, now)?;
            let mut c = run(rows, &a.id)?;
            if inserted {
                c.run.child = Some(link.clone());
            }
            anyhow::ensure!(c.run.child.as_ref() == Some(link), "child belongs to another invocation");
            let fence = if inserted { Some(claim_run(&mut c, now, &owner)?) } else { None };
            store_run(rows, &c)?;
            if let Some(outcome) = c.run.outcome {
                lifecycle(rows, &a.id, &outcome, now)?;
            } else if c.run.state == RunState::Waiting {
                lifecycle(rows, &a.id, &Status::Waiting { wake_at_ms: 0 }, now)?;
            }
            Ok(fence)
        })
    }
    fn child_outcome(&self, parent: &str, k: &str, child: &str) -> anyhow::Result<Option<Status>> {
        let s = run(&self.backend.snapshot()?.rows, child)?;
        anyhow::ensure!(
            s.run.child.as_ref().is_some_and(|l| l.parent == parent && l.key == k),
            "child belongs to another parent or key"
        );
        Ok(s.run.outcome)
    }
    fn due(&self, now: i64, limit: usize) -> anyhow::Result<Vec<String>> {
        let mut due: Vec<_> =
            all_runs(&self.backend.snapshot()?.rows)?.into_iter().filter(|s| claimable(&s.run, now)).collect();
        due.sort_by_key(|s| (s.run.wake_at_ms, s.run.admission.id.clone()));
        Ok(due.into_iter().take(limit).map(|s| s.run.admission.id).collect())
    }
    fn cancel(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| {
            let Some(s) = get::<StoredRun>(rows, "run", id)? else {
                return Ok(false);
            };
            if s.run.ended_ms.is_some() {
                return Ok(false);
            }
            terminate(rows, id, &Status::Cancelled, now)?;
            Ok(true)
        })
    }
    fn sweep(&self, now: i64, kept: i64) -> anyhow::Result<usize> {
        anyhow::ensure!(kept >= 0, "negative retention");
        self.update(|rows| {
            let mut count = 0;
            for mut s in all_runs(rows)? {
                if s.run.ended_ms.is_some_and(|at| at <= now.saturating_sub(kept)) {
                    count += s.records.len();
                    s.records.clear();
                    s.events.clear();
                    store_run(rows, &s)?;
                }
            }
            Ok(count)
        })
    }
    fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>> {
        Ok(get::<StoredRun>(&self.backend.snapshot()?.rows, "run", id)?.map(|s| s.run))
    }
    fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        Ok(all_runs(&self.backend.snapshot()?.rows)?
            .into_iter()
            .map(|s| s.run)
            .filter(|r| name.is_none_or(|name| name == r.admission.name) && r.admission.id.starts_with(prefix))
            .collect())
    }
    fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>> {
        get(&self.backend.snapshot()?.rows, "run", id)
    }
    fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        self.update(|rows| {
            let mut defs: Definitions = get(rows, "definitions", name)?.unwrap_or_default();
            let version = defs
                .versions
                .last_key_value()
                .map_or(Some(1), |(v, _)| v.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("definition version overflow"))?;
            let d = Definition { name: name.into(), version, yaml: yaml.into() };
            defs.versions.insert(version, d.clone());
            put(rows, "definitions", name, &defs)?;
            Ok(d)
        })
    }
    fn activate(&self, name: &str, version: u64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut defs: Definitions = get(rows, "definitions", name)?.unwrap_or_default();
            anyhow::ensure!(defs.versions.contains_key(&version), "unknown definition version");
            if defs.active != Some(version) {
                defs.previous = defs.active;
                defs.active = Some(version);
                defs.consecutive_faults = 0;
            }
            put(rows, "definitions", name, &defs)
        })
    }
    fn definitions(&self, name: &str) -> anyhow::Result<Definitions> {
        Ok(get(&self.backend.snapshot()?.rows, "definitions", name)?.unwrap_or_default())
    }
}
