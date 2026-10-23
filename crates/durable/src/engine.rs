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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rearm: Option<Rearm>,
}

fn key(kind: &str, id: &str) -> Key {
    (kind.into(), id.into())
}
fn get<T: DeserializeOwned>(rows: &mut dyn Transaction, kind: &str, id: &str) -> anyhow::Result<Option<T>> {
    rows.get(kind, id)?.map(serde_json::from_value).transpose().map_err(Into::into)
}
fn put(rows: &mut dyn Transaction, kind: &str, id: &str, value: &impl Serialize) -> anyhow::Result<()> {
    rows.put(kind, id, &serde_json::to_value(value)?)
}
fn run(rows: &mut dyn Transaction, id: &str) -> anyhow::Result<StoredRun> {
    get(rows, "run", id)?.ok_or_else(|| anyhow::anyhow!("unknown run {id}"))
}
fn component(value: &str) -> String {
    value.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
// Flip the sign bit so lexicographic order also covers negative timestamps.
fn time_key(ms: i64) -> String {
    format!("{:016x}", u64::from_be_bytes((ms ^ i64::MIN).to_be_bytes()))
}
fn timed(ms: i64, id: &str) -> String {
    format!("{}:{id}", time_key(ms))
}
fn partition(kind: &str, value: &str) -> String {
    format!("{kind}:{}", component(value))
}
fn prefix_end(prefix: &str) -> Option<String> {
    let mut chars: Vec<_> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut next = u32::from(last) + 1;
        if next == 0xd800 {
            next = 0xe000;
        }
        if let Some(next) = char::from_u32(next) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}
// Index only eligible waits and unswept evidence: a full inbox or an already
// swept run must not make subsequent lookups walk historical registrations.
fn indexes(s: &StoredRun) -> Rows {
    let mut keys = Rows::new();
    let id = &s.run.admission.id;
    let value = json!(id);
    keys.insert(key(&partition("name", &s.run.admission.name), id), value.clone());
    if s.run.ended_ms.is_none() {
        let due = if s.run.state == RunState::Running { s.run.lease_until_ms } else { Some(s.run.wake_at_ms) };
        if let Some(due) = due {
            keys.insert(key("due", &timed(due, id)), value.clone());
        }
        if let Some(link) = &s.run.child {
            keys.insert(key(&partition("children", &link.parent), id), value.clone());
        }
        for (t, at) in &s.awaits {
            if !s.events.iter().any(|e| e.type_ == *t && e.consumed_key.is_none()) {
                keys.insert(key(&partition("wait", t), &timed(*at, id)), value.clone());
            }
        }
        // A publish reads only the runs subscribed to its type (or to `*`).
        for t in s.subscriptions.values().flatten() {
            keys.insert(key(&partition("subs", t), id), value.clone());
        }
    } else if !s.records.is_empty() || !s.events.is_empty() {
        keys.insert(key("finished", &timed(s.run.ended_ms.unwrap(), id)), value);
    }
    keys
}
fn store_run(rows: &mut dyn Transaction, snapshot: &StoredRun) -> anyhow::Result<()> {
    let id = &snapshot.run.admission.id;
    let old: Option<StoredRun> = get(rows, "run", id)?;
    let before = old.as_ref().map(indexes).unwrap_or_default();
    let after = indexes(snapshot);
    for ((kind, id), value) in crate::backend::changes(&before, &after) {
        if let Some(value) = value {
            rows.put(kind, id, value)?;
        } else {
            rows.delete(kind, id)?;
        }
    }
    put(rows, "run", id, snapshot)
}
fn checked(rows: &mut dyn Transaction, fence: &Fence, now: i64) -> anyhow::Result<StoredRun> {
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
fn indexed_runs(
    rows: &mut dyn Transaction,
    kind: &str,
    start: &str,
    end: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<StoredRun>> {
    rows.scan(kind, start, end, limit)?
        .into_values()
        .map(|id| run(rows, id.as_str().ok_or_else(|| anyhow::anyhow!("invalid run index"))?))
        .collect()
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
fn insert(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<bool> {
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
            rearm: None,
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
fn deliver(
    rows: &mut dyn Transaction,
    t: &str,
    data: &Value,
    now: i64,
    target: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let candidate = if let Some(id) = target {
        let s = run(rows, id)?;
        (s.run.ended_ms.is_none()
            && s.awaits.contains_key(t)
            && !s.events.iter().any(|e| e.type_ == t && e.consumed_key.is_none()))
        .then_some(s)
    } else {
        indexed_runs(rows, &partition("wait", t), "", None, 1)?.into_iter().next()
    };
    let Some(mut snapshot) = candidate else {
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
/// Reads only the runs indexed under the event's type and `*` (or the
/// target): bounded by the subscribers, never by the runs stored.
fn publish(rows: &mut dyn Transaction, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Vec<String>> {
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
    let candidates = match target {
        Some(id) => get::<StoredRun>(rows, "run", id)?.into_iter().collect(),
        None => {
            let mut runs = indexed_runs(rows, &partition("subs", t), "", None, usize::MAX)?;
            for s in indexed_runs(rows, &partition("subs", "*"), "", None, usize::MAX)? {
                if !runs.iter().any(|r| r.run.admission.id == s.run.admission.id) {
                    runs.push(s);
                }
            }
            runs.sort_by(|a, b| a.run.admission.id.cmp(&b.run.admission.id));
            runs
        }
    };
    let mut receivers = Vec::new();
    for mut snapshot in candidates {
        let eligible = snapshot.run.ended_ms.is_none()
            && (target.is_some() || subscribed(&snapshot, &stored))
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
fn trigger_indexes(key_: &str, t: &Trigger) -> Rows {
    let mut keys = Rows::new();
    if let Some(at) = t.at_ms {
        keys.insert(key("trigger_due", &timed(at, key_)), json!(key_));
    }
    for ty in &t.types {
        keys.insert(key(&partition("trigger_type", ty), key_), json!(key_));
    }
    keys
}
/// Writes a trigger and moves its index keys, like `store_run`.
fn store_trigger(
    rows: &mut dyn Transaction,
    key_: &str,
    before: Option<&Trigger>,
    after: Option<&Trigger>,
) -> anyhow::Result<()> {
    let old = before.map(|t| trigger_indexes(key_, t)).unwrap_or_default();
    let new = after.map(|t| trigger_indexes(key_, t)).unwrap_or_default();
    for ((kind, id), value) in crate::backend::changes(&old, &new) {
        if let Some(value) = value {
            rows.put(kind, id, value)?;
        } else {
            rows.delete(kind, id)?;
        }
    }
    match after {
        Some(t) => put(rows, "trigger", key_, t),
        None => rows.delete("trigger", key_),
    }
}
fn indexed_triggers(rows: &mut dyn Transaction, keys: Rows) -> anyhow::Result<Vec<(String, Trigger)>> {
    let mut out: Vec<(String, Trigger)> = Vec::new();
    for value in keys.into_values() {
        let k = value.as_str().ok_or_else(|| anyhow::anyhow!("invalid trigger index"))?.to_owned();
        if out.iter().any(|(o, _)| *o == k) {
            continue;
        }
        let t = get(rows, "trigger", &k)?.ok_or_else(|| anyhow::anyhow!("dangling trigger index {k}"))?;
        out.push((k, t));
    }
    Ok(out)
}
fn lifecycle(rows: &mut dyn Transaction, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
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
fn terminate(rows: &mut dyn Transaction, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
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
    if let Some(rearm) = &snapshot.rearm
        && let Some(t) = get::<Trigger>(rows, "trigger", &rearm.trigger)?
        && t.at_ms.is_none()
    {
        let due = Trigger { at_ms: Some(now.saturating_add(rearm.delay_ms)), ..t.clone() };
        store_trigger(rows, &rearm.trigger, Some(&t), Some(&due))?;
    }
    // Iterative cascade avoids stack growth on arbitrarily nested child runs.
    let mut parents = vec![id.to_string()];
    while let Some(parent) = parents.pop() {
        for mut child in indexed_runs(rows, &partition("children", &parent), "", None, usize::MAX)? {
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
        self.update(|rows| {
            checked(rows, f, now)?
                .records
                .into_iter()
                .map(|(key, value)| Ok((key, serde_json::from_value(value)?)))
                .collect()
        })
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
    fn trigger(&self, key: &str) -> anyhow::Result<Option<Trigger>> {
        self.update(|rows| get(rows, "trigger", key))
    }
    fn due_triggers(&self, now: i64, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.update(|rows| {
            let keys = rows.scan("trigger_due", "", Some(&format!("{};", time_key(now))), limit)?;
            indexed_triggers(rows, keys)
        })
    }
    fn triggers_for(&self, t: &str, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.update(|rows| {
            let mut keys = rows.scan(&partition("trigger_type", t), "", None, limit)?;
            if t != "*" && keys.len() < limit {
                keys.extend(rows.scan(&partition("trigger_type", "*"), "", None, limit - keys.len())?);
            }
            indexed_triggers(rows, keys)
        })
    }
    fn swap_trigger(
        &self,
        key: &str,
        expected: Option<&Trigger>,
        next: Option<&Trigger>,
        admit: &[Admission],
        rearm: Option<&Rearm>,
        now: i64,
    ) -> anyhow::Result<bool> {
        self.update(|rows| {
            if get::<Trigger>(rows, "trigger", key)?.as_ref() != expected {
                return Ok(false);
            }
            store_trigger(rows, key, expected, next)?;
            for a in admit {
                if insert(rows, a, now)? && rearm.is_some() {
                    let mut s = run(rows, &a.id)?;
                    s.rearm = rearm.cloned();
                    store_run(rows, &s)?;
                }
            }
            Ok(true)
        })
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
        self.update(|rows| {
            let s = run(rows, child)?;
            anyhow::ensure!(
                s.run.child.as_ref().is_some_and(|l| l.parent == parent && l.key == k),
                "child belongs to another parent or key"
            );
            Ok(s.run.outcome)
        })
    }
    fn due(&self, now: i64, limit: usize) -> anyhow::Result<Vec<String>> {
        self.update(|rows| {
            rows.scan("due", "", Some(&format!("{};", time_key(now))), limit)?
                .into_values()
                .map(|id| id.as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("invalid due index")))
                .collect()
        })
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
            for mut s in indexed_runs(
                rows,
                "finished",
                "",
                Some(&format!("{};", time_key(now.saturating_sub(kept)))),
                usize::MAX,
            )? {
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
        self.update(|rows| Ok(get::<StoredRun>(rows, "run", id)?.map(|s| s.run)))
    }
    fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        self.update(|rows| {
            let end = prefix_end(prefix);
            if let Some(name) = name {
                Ok(indexed_runs(rows, &partition("name", name), prefix, end.as_deref(), usize::MAX)?
                    .into_iter()
                    .map(|s| s.run)
                    .collect())
            } else {
                rows.scan("run", prefix, end.as_deref(), usize::MAX)?
                    .into_values()
                    .map(|v| Ok(serde_json::from_value::<StoredRun>(v)?.run))
                    .collect()
            }
        })
    }
    fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>> {
        self.update(|rows| get(rows, "run", id))
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
        self.update(|rows| Ok(get(rows, "definitions", name)?.unwrap_or_default()))
    }
}
