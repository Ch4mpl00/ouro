//! Shared transactional semantics: 1. stored rows and their index keys;
//! 2. admission, leases and compatibility; 3. events: delivery, the mailbox,
//! publication and expiry; 4. triggers, ingress tombstones and activation
//! epochs; 5. termination and the breaker; 6. the `RunStore` implementation.
use crate::*;
use owf::{durable::check_transition, run::WorkflowError};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

// ── 1. rows and index keys ─────────────────────────────────────────────────

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
    /// The rearm a firing attached and the trigger that firing stored: a
    /// trigger that differs at the run's end was replaced (a later epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rearm: Option<(Rearm, Trigger)>,
}

/// An ingress identity a trigger took, until `expires_at_ms`.
#[derive(Serialize, Deserialize)]
struct Tombstone {
    payload: Value,
    expires_at_ms: i64,
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
/// The exclusive end of a range of timed keys up to `ms` inclusive.
fn until(ms: i64) -> String {
    format!("{};", time_key(ms))
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
/// An event's identity, `source` + `id`, when it has both.
fn identity(event: &Value) -> Option<String> {
    let text = |k: &str| event.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(format!("{}\u{0}{}", text("source")?, text("id")?))
}
/// What must agree when an identity is accepted again: the event without its
/// `time`, which a host may fill in at each attempt.
fn payload(event: &Value) -> Value {
    let mut event = event.clone();
    if let Value::Object(m) = &mut event {
        m.remove("time");
    }
    event
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
            // Per application version (empty: undeclared), so a declared
            // worker pages only through runs it may claim.
            let app = s.run.admission.application_version().unwrap_or_default();
            keys.insert(key(&partition("due_app", app), &timed(due, id)), value.clone());
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
        // The earliest mailbox expiry, for `sweep`.
        if let Some(at) = s.events.iter().filter(|e| e.consumed_key.is_none()).filter_map(|e| e.expires_at_ms).min() {
            keys.insert(key("mailbox", &timed(at, id)), value.clone());
        }
    } else if !s.records.is_empty() || !s.events.is_empty() {
        keys.insert(key("finished", &timed(s.run.ended_ms.unwrap(), id)), value.clone());
    }
    // A delivered identity leads a redelivery back to its recipient for as
    // long as the recipient keeps that evidence.
    for e in s.events.iter().filter(|e| e.delivered) {
        if let Some(identity) = e.envelope.as_ref().and_then(identity) {
            keys.insert(key(&partition("delivered", &identity), id), value.clone());
        }
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

// ── 2. admission, leases and compatibility ─────────────────────────────────

/// The current owner's authority: it acts only while its lease has more
/// than the skew bound left, so no other worker can have taken over yet.
fn checked(rows: &mut dyn Transaction, fence: &Fence, now: i64, skew: i64) -> anyhow::Result<StoredRun> {
    let snapshot = run(rows, &fence.run_id)?;
    anyhow::ensure!(
        snapshot.run.state == RunState::Running
            && snapshot.run.fence.as_ref() == Some(fence)
            && snapshot.run.lease_until_ms.is_some_and(|until| until > now.saturating_add(skew)),
        "run {} lease expired or was replaced",
        fence.run_id
    );
    Ok(snapshot)
}
fn claimable(r: &SavedRun, now: i64) -> bool {
    match r.state {
        RunState::Ready | RunState::Waiting => r.wake_at_ms <= now,
        // Takeover only after the lease has ended.
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
/// Admits a run once. Its admission instant and the activation epoch it
/// belongs to (when it runs the active version) are fixed here.
fn insert(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<bool> {
    if let Some(existing) = get::<StoredRun>(rows, "run", &admission.id)? {
        anyhow::ensure!(existing.run.admission.name == admission.name, "run already belongs to another definition");
        return Ok(false);
    }
    let epoch = get::<Definitions>(rows, "definitions", &admission.name)?
        .filter(|d| d.active == Some(admission.version))
        .map(|d| d.epoch);
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
                admitted_ms: now,
                epoch,
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

// ── 3. events ──────────────────────────────────────────────────────────────

fn expired(e: &StoredEvent, now: i64) -> bool {
    e.expires_at_ms.is_some_and(|at| at <= now)
}
/// Taken as a wildcard subscription's event: published, not delivered.
fn published(e: &StoredEvent) -> bool {
    e.envelope.is_some() && !e.delivered
}
/// Whether a pending event may be taken by `types` (`*` takes published ones).
fn takes(types: &[String], event: &StoredEvent) -> bool {
    types.contains(&event.type_) || (published(event) && types.iter().any(|t| t == "*"))
}
fn subscribed(snapshot: &StoredRun, event: &StoredEvent) -> bool {
    event.envelope.is_some() && snapshot.subscriptions.values().any(|types| takes(types, event))
}
/// A pending, unexpired copy the run is waiting or subscribed for.
fn wanted(snapshot: &StoredRun, event: &StoredEvent, now: i64) -> bool {
    event.consumed_key.is_none()
        && !expired(event, now)
        && (snapshot.awaits.contains_key(&event.type_) || subscribed(snapshot, event))
}
/// The event as taken: the accepted CloudEvent with its delivery `time`, or
/// a lifecycle notification's `type`, `data` and `time`.
fn envelope(event: &StoredEvent) -> anyhow::Result<Value> {
    let time = chrono::DateTime::from_timestamp_millis(event.delivered_at_ms)
        .ok_or_else(|| anyhow::anyhow!("invalid event timestamp"))?
        .to_rfc3339();
    Ok(match &event.envelope {
        Some(Value::Object(m)) if !m.contains_key("time") => {
            let mut m = m.clone();
            m.insert("time".into(), json!(time));
            Value::Object(m)
        }
        Some(envelope) => envelope.clone(),
        None => json!({"type":event.type_, "data":event.data, "time":time}),
    })
}
/// Consumes every pending copy whose mailbox time is over with an observable
/// `Record::Expired`, keyed by its position. Returns how many expired.
fn expire(snapshot: &mut StoredRun, now: i64) -> anyhow::Result<usize> {
    let mut count = 0;
    for i in 0..snapshot.events.len() {
        if snapshot.events[i].consumed_key.is_none() && expired(&snapshot.events[i], now) {
            let k = format!("mailbox#expired[{i}]");
            let event = envelope(&snapshot.events[i])?;
            snapshot.records.insert(k.clone(), serde_json::to_value(Record::Expired { event })?);
            snapshot.events[i].consumed_key = Some(k);
            count += 1;
        }
    }
    Ok(count)
}
/// Accepts `event` into the run, unless the run already holds its identity
/// (pending, taken or expired): `false` for the same payload, an
/// `IdentityConflict` for another. Wakes a run that waits for it.
fn accept(snapshot: &mut StoredRun, event: StoredEvent, now: i64) -> anyhow::Result<bool> {
    if let Some(id) = event.envelope.as_ref().and_then(identity) {
        let held =
            snapshot.events.iter().find_map(|e| e.envelope.as_ref().filter(|v| identity(v).as_ref() == Some(&id)));
        if let Some(held) = held {
            if event.envelope.as_ref().map(payload) != Some(payload(held)) {
                return Err(IdentityConflict(id.replace('\u{0}', "/")).into());
            }
            return Ok(false);
        }
    }
    if wanted(snapshot, &event, now) {
        snapshot.run.wake_at_ms = snapshot.run.wake_at_ms.min(now);
    }
    snapshot.events.push(event);
    Ok(true)
}
fn deliver(
    rows: &mut dyn Transaction,
    event: &Value,
    target: Option<&str>,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<Option<String>> {
    let t = event
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("a delivered event needs a type"))?;
    let id = identity(event).ok_or_else(|| anyhow::anyhow!("a delivered event needs a source and an id"))?;
    let stored = StoredEvent {
        type_: t.into(),
        data: event.get("data").cloned().unwrap_or(Value::Null),
        delivered_at_ms: now,
        consumed_key: None,
        envelope: Some(event.clone()),
        delivered: true,
        expires_at_ms: Some(now.saturating_add(policy.mailbox_ms)),
    };
    let candidate = match target {
        Some(target) => get::<StoredRun>(rows, "run", target)?,
        // A redelivery goes back to the run that accepted it first; a new
        // event to the oldest eligible registration of its type.
        None => match indexed_runs(rows, &partition("delivered", &id), "", None, 1)?.pop() {
            Some(first) => Some(first),
            None => indexed_runs(rows, &partition("wait", t), "", None, 1)?.pop(),
        },
    };
    let Some(mut snapshot) = candidate else {
        return Ok(None);
    };
    let recipient = snapshot.run.admission.id.clone();
    let holds = snapshot.events.iter().any(|e| e.envelope.as_ref().and_then(identity).as_ref() == Some(&id));
    if snapshot.run.ended_ms.is_some() && !holds {
        return Ok(None);
    }
    if accept(&mut snapshot, stored, now)? {
        store_run(rows, &snapshot)?;
    }
    Ok(Some(recipient))
}
/// A child's lifecycle notification to its parent, which registered for it
/// when it started the child: one pending copy per type, no identity.
fn notify(rows: &mut dyn Transaction, parent: &str, t: &str, data: &Value, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, parent)?;
    if snapshot.run.ended_ms.is_some()
        || !snapshot.awaits.contains_key(t)
        || snapshot.events.iter().any(|e| e.type_ == t && e.consumed_key.is_none())
    {
        return Ok(());
    }
    snapshot.events.push(StoredEvent {
        type_: t.into(),
        data: data.clone(),
        delivered_at_ms: now,
        consumed_key: None,
        envelope: None,
        delivered: false,
        expires_at_ms: None,
    });
    snapshot.run.wake_at_ms = now;
    store_run(rows, &snapshot)
}
/// Reads only the runs indexed under the event's type and `*` (or the
/// target): bounded by the subscribers, never by the runs stored.
fn publish(
    rows: &mut dyn Transaction,
    event: &Value,
    target: Option<&str>,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<Vec<String>> {
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
        delivered: false,
        // Addressed: kept in the target's mailbox for a limited time.
        expires_at_ms: target.map(|_| now.saturating_add(policy.mailbox_ms)),
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
        let eligible = snapshot.run.ended_ms.is_none() && (target.is_some() || subscribed(&snapshot, &stored));
        if eligible && accept(&mut snapshot, stored.clone(), now)? {
            receivers.push(snapshot.run.admission.id.clone());
            store_run(rows, &snapshot)?;
        }
    }
    Ok(receivers)
}

// ── 4. triggers, ingress and activation epochs ─────────────────────────────

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
        None if before.is_some() => rows.delete("trigger", key_),
        None => Ok(()),
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
/// Replaces the triggers a definition owns with `wanted`, in this commit.
fn arm(rows: &mut dyn Transaction, defs: &mut Definitions, wanted: &BTreeMap<String, Trigger>) -> anyhow::Result<()> {
    for k in defs.triggers.iter().filter(|k| !wanted.contains_key(*k)) {
        let current: Option<Trigger> = get(rows, "trigger", k)?;
        store_trigger(rows, k, current.as_ref(), None)?;
    }
    for (k, t) in wanted {
        let current: Option<Trigger> = get(rows, "trigger", k)?;
        store_trigger(rows, k, current.as_ref(), Some(t))?;
    }
    defs.triggers = wanted.keys().cloned().collect();
    Ok(())
}
/// `Some(true)`: a trigger already took this event (same payload);
/// `Some(false)`: its identity is free (absent or expired).
fn tombstone(rows: &mut dyn Transaction, event: &Value, now: i64) -> anyhow::Result<(String, Option<Tombstone>, bool)> {
    let id = identity(event).ok_or_else(|| anyhow::anyhow!("an ingress event needs a source and an id"))?;
    let k = component(&id);
    let stored: Option<Tombstone> = get(rows, "ingress", &k)?;
    let used = match &stored {
        Some(t) if t.expires_at_ms > now => {
            if t.payload != payload(event) {
                return Err(IdentityConflict(id.replace('\u{0}', "/")).into());
            }
            true
        }
        _ => false,
    };
    Ok((k, stored, used))
}
fn fire(rows: &mut dyn Transaction, f: &Firing<'_>, now: i64, policy: &Policy) -> anyhow::Result<Fired> {
    let current: Option<Trigger> = get(rows, "trigger", f.key)?;
    if current.as_ref() != f.expected {
        return Ok(Fired::Changed);
    }
    // Overlap: the previous run of this trigger is still running or waiting.
    if let Some(busy) = f.busy
        && !f.admit.is_empty()
        && let Some(last) = get::<String>(rows, "trigger_run", f.key)?
        && get::<StoredRun>(rows, "run", &last)?.is_some_and(|s| s.run.ended_ms.is_none())
    {
        store_trigger(rows, f.key, current.as_ref(), Some(busy))?;
        return Ok(Fired::Busy(last));
    }
    if let Some(event) = f.ingress {
        let (k, old, used) = tombstone(rows, event, now)?;
        if used {
            return Ok(Fired::Ingested);
        }
        if let Some(old) = old {
            rows.delete("ingress_due", &timed(old.expires_at_ms, &k))?;
        }
        let expires_at_ms = now.saturating_add(policy.ingress_ms);
        put(rows, "ingress", &k, &Tombstone { payload: payload(event), expires_at_ms })?;
        rows.put("ingress_due", &timed(expires_at_ms, &k), &json!(k))?;
    }
    store_trigger(rows, f.key, current.as_ref(), f.next)?;
    for a in f.admit {
        if insert(rows, a, now)?
            && let (Some(rearm), Some(next)) = (f.rearm, f.next)
        {
            let mut s = run(rows, &a.id)?;
            s.rearm = Some((rearm.clone(), next.clone()));
            store_run(rows, &s)?;
        }
    }
    if let Some(a) = f.admit.last() {
        put(rows, "trigger_run", f.key, &a.id)?;
    }
    Ok(Fired::Committed)
}
fn activate(
    rows: &mut dyn Transaction,
    name: &str,
    version: u64,
    epoch: Option<u64>,
    arming: &Arming,
) -> anyhow::Result<Option<Definitions>> {
    let mut defs: Definitions = get(rows, "definitions", name)?.unwrap_or_default();
    anyhow::ensure!(defs.versions.contains_key(&version), "unknown definition version");
    if epoch.is_some_and(|e| e != defs.epoch) {
        return Ok(None);
    }
    if defs.active != Some(version) {
        defs.previous = defs.active;
    }
    defs.active = Some(version);
    defs.epoch = defs.epoch.checked_add(1).ok_or_else(|| anyhow::anyhow!("activation epoch overflow"))?;
    defs.consecutive_faults = 0;
    defs.tripped = false;
    arm(rows, &mut defs, &arming.triggers)?;
    defs.fallback = arming.fallback.clone();
    put(rows, "definitions", name, &defs)?;
    Ok(Some(defs))
}

// ── 5. termination and the breaker ─────────────────────────────────────────

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
    notify(rows, &link.parent, &t, &data, now)
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
    // Only the trigger the firing stored is re-armed: a later activation
    // (a new epoch) replaced it, and an old run cannot move the new one.
    if let Some((rearm, armed)) = &snapshot.rearm
        && let Some(t) = get::<Trigger>(rows, "trigger", &rearm.trigger)?
        && t == *armed
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
/// A terminal outcome's breaker observation, in the terminal commit. Only a
/// run admitted in the active version's current epoch counts.
fn breaker(rows: &mut dyn Transaction, run: &SavedRun, fault: Option<bool>, threshold: u32) -> anyhow::Result<()> {
    let name = &run.admission.name;
    let Some(mut defs) = get::<Definitions>(rows, "definitions", name)? else {
        return Ok(());
    };
    if defs.active != Some(run.admission.version) || run.epoch != Some(defs.epoch) {
        return Ok(());
    }
    match fault {
        Some(false) if defs.consecutive_faults > 0 => defs.consecutive_faults = 0,
        Some(true) => {
            defs.consecutive_faults = defs.consecutive_faults.saturating_add(1);
            if defs.consecutive_faults >= threshold.max(1) {
                defs.consecutive_faults = 0;
                if let Some(previous) = defs.previous.take() {
                    // Rollback: the predecessor in a new epoch, with its triggers.
                    defs.active = Some(previous);
                    defs.epoch =
                        defs.epoch.checked_add(1).ok_or_else(|| anyhow::anyhow!("activation epoch overflow"))?;
                    let fallback = std::mem::take(&mut defs.fallback);
                    arm(rows, &mut defs, &fallback)?;
                } else {
                    // Nothing to roll back to: no new runs, no schedule.
                    defs.tripped = true;
                    arm(rows, &mut defs, &BTreeMap::new())?;
                }
            }
        }
        _ => return Ok(()),
    }
    put(rows, "definitions", name, &defs)
}

// ── 6. the RunStore implementation ─────────────────────────────────────────

impl RunStore for TransactionalStore {
    fn policy(&self) -> Policy {
        self.policy
    }
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
    fn claim_as(&self, id: &str, now: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            let Some(mut s) = get::<StoredRun>(rows, "run", id)? else {
                return Ok(None);
            };
            if !claimable(&s.run, now) || !worker.admits(s.run.admission.application_version()) {
                return Ok(None);
            }
            let fence = claim_run(&mut s, now, &owner)?;
            store_run(rows, &s)?;
            Ok(Some(fence))
        })
    }
    fn renew(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            s.run.lease_until_ms = Some(now.saturating_add(LEASE_MS));
            store_run(rows, &s)
        })
    }
    fn release(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            s.run.state = RunState::Ready;
            s.run.fence = None;
            s.run.lease_until_ms = None;
            s.run.wake_at_ms = now;
            store_run(rows, &s)
        })
    }
    fn load_records(&self, f: &Fence, now: i64) -> anyhow::Result<BTreeMap<String, Record>> {
        self.update(|rows| {
            checked(rows, f, now, self.policy.skew_ms)?
                .records
                .into_iter()
                .map(|(key, value)| Ok((key, serde_json::from_value(value)?)))
                .collect()
        })
    }

    fn save_record(&self, f: &Fence, k: &str, r: &Record, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            if matches!(r, Record::Event { .. } | Record::Expired { .. }) {
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
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            for t in types {
                s.awaits.entry(t.clone()).or_insert(now);
            }
            store_run(rows, &s)
        })
    }
    fn unregister_wait(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            s.listeners.remove(k);
            close(&mut s, types);
            store_run(rows, &s)
        })
    }
    fn deliver(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Option<String>> {
        self.update(|rows| deliver(rows, event, target, now, &self.policy))
    }
    fn take_event(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<Option<Value>> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            if let Some(saved) = s.records.get(k) {
                let Record::Event { event } = serde_json::from_value(saved.clone())? else {
                    anyhow::bail!("invalid event checkpoint")
                };
                return Ok(Some(event.clone()));
            }
            // A copy past its mailbox time is consumed by its expiry record,
            // never by this listen.
            expire(&mut s, now)?;
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
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            s.subscriptions.insert(k.into(), types.to_vec());
            store_run(rows, &s)
        })
    }
    fn unsubscribe(&self, f: &Fence, k: &str, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            s.subscriptions.remove(k);
            store_run(rows, &s)
        })
    }
    fn publish(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Vec<String>> {
        self.update(|rows| publish(rows, event, target, now, &self.policy))
    }
    fn ingested(&self, event: &Value, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| Ok(tombstone(rows, event, now)?.2))
    }
    fn trigger(&self, key: &str) -> anyhow::Result<Option<Trigger>> {
        self.update(|rows| get(rows, "trigger", key))
    }
    fn due_triggers(&self, now: i64, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.update(|rows| {
            let keys = rows.scan("trigger_due", "", Some(&until(now)), limit)?;
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
    fn fire(&self, firing: &Firing<'_>, now: i64) -> anyhow::Result<Fired> {
        self.update(|rows| fire(rows, firing, now, &self.policy))
    }
    fn finish(&self, f: &Fence, outcome: &Status, now: i64, threshold: u32) -> anyhow::Result<Status> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            let mut status = outcome.clone();
            match &status {
                Status::Waiting { wake_at_ms } => {
                    s.run.state = RunState::Waiting;
                    s.run.interruptions = 0;
                    s.run.wake_at_ms = *wake_at_ms;
                    if s.events.iter().any(|e| wanted(&s, e, now)) {
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
                // The breaker update shares the terminal commit, so an
                // ambiguous finish or a fast parent cannot count a run twice.
                // Cancellation and infrastructure interruptions (including
                // their exhaustion) are not faults of the version.
                let fault = match &status {
                    Status::Completed(_) => Some(false),
                    Status::Faulted(_) if !matches!(outcome, Status::Interrupted(_)) => Some(true),
                    _ => None,
                };
                breaker(rows, &s.run, fault, threshold)?;
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
            let mut p = checked(rows, parent, now, self.policy.skew_ms)?;
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
    fn due_as(&self, now: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>> {
        self.update(|rows| {
            let Some(own) = &worker.version else {
                return rows
                    .scan("due", "", Some(&until(now)), limit)?
                    .into_values()
                    .map(|id| id.as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("invalid due index")))
                    .collect();
            };
            // Undeclared runs, its own version and the ones it accepts: one
            // bounded page per partition, merged in due order.
            let mut versions = vec!["", own.as_str()];
            versions.extend(worker.accepts.iter().map(String::as_str));
            versions.sort_unstable();
            versions.dedup();
            let mut due = Vec::new();
            for v in versions {
                for ((_, k), id) in rows.scan(&partition("due_app", v), "", Some(&until(now)), limit)? {
                    let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid due index"))?.to_owned();
                    due.push((k, id));
                }
            }
            due.sort();
            Ok(due.into_iter().take(limit).map(|(_, id)| id).collect())
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
    fn sweep(&self, now: i64, kept: i64) -> anyhow::Result<Swept> {
        anyhow::ensure!(kept >= 0, "negative retention");
        self.update(|rows| {
            let mut swept = Swept::default();
            let horizon = now.saturating_sub(kept);
            for mut s in indexed_runs(rows, "finished", "", Some(&until(horizon)), usize::MAX)? {
                if s.run.ended_ms.is_some_and(|at| at <= horizon) {
                    swept.records += s.records.len();
                    s.records.clear();
                    s.events.clear();
                    store_run(rows, &s)?;
                }
            }
            // Unfinished runs whose mailbox holds a copy past its time.
            for mut s in indexed_runs(rows, "mailbox", "", Some(&until(now)), usize::MAX)? {
                swept.expired += expire(&mut s, now)?;
                store_run(rows, &s)?;
            }
            for ((_, k), id) in rows.scan("ingress_due", "", Some(&until(now)), usize::MAX)? {
                let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid ingress index"))?.to_owned();
                rows.delete("ingress_due", &k)?;
                rows.delete("ingress", &id)?;
                swept.tombstones += 1;
            }
            Ok(swept)
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
        self.update(|rows| activate(rows, name, version, None, &Arming::default())).map(|_| ())
    }
    fn activate_with(
        &self,
        name: &str,
        version: u64,
        epoch: u64,
        arming: &Arming,
    ) -> anyhow::Result<Option<Definitions>> {
        self.update(|rows| activate(rows, name, version, Some(epoch), arming))
    }
    fn definitions(&self, name: &str) -> anyhow::Result<Definitions> {
        self.update(|rows| Ok(get(rows, "definitions", name)?.unwrap_or_default()))
    }
}
