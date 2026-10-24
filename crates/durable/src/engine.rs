//! Shared transactional semantics: 1. stored rows and their index keys;
//! 2. admission, leases and compatibility; 3. events: delivery, the mailbox,
//! publication and expiry; 4. triggers, ingress tombstones and activation
//! epochs; 5. termination and the breaker; 6. the `RunStore` implementation.
//!
//! A run tree's budget is one counter row (`budget`, keyed by the root's
//! id) next to the runs: a spend checks the spending run's fence, writes its
//! decision's record row and moves that counter, so a child's unit and the
//! root's count commit together without touching the root's run row.
use crate::*;
use owf::{
    durable::{EXCEEDED, VERSION, check_transition},
    run::WorkflowError,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

// ── 1. rows and index keys ─────────────────────────────────────────────────

// The run row holds lifecycle and fencing and the run's state bounded by its
// open listens: waits, listeners, subscriptions, plus counters summarising
// its copies. What grows with the run's life has rows of its own, partitioned
// by the run (`<family>:<hex run id>`), read by ordered range scans and
// written one key per change in the commit that checks the fence:
//
// | Family | ID | Value |
// | --- | --- | --- |
// | `record` | record key | One journal record, decoded only by journal operations |
// | `event` | sequence | One accepted copy (`StoredEvent`), pending or consumed |
// | `event_id` | hex identity | The sequence of the copy holding a `source` + `id` |
// | `event_type` | hex type | The sequence of the first copy of a type |
// | `pending:<hex type>` | sequence | A pending copy of the type, in arrival order |
// | `live:<hex type>` | expiry + sequence | The same by expiry (none: never), whether it is a CloudEvent |
// | `published` | sequence | A pending published copy, for wildcard takes |
// | `live_published` | expiry + sequence | The same by expiry |
// | `expiry` | expiry + sequence | A pending mailbox copy, until it is taken or expires |
//
// Consumed copies keep only their `event`, `event_id` and `event_type` rows;
// `sweep` removes every row of a finished run's evidence. A save therefore
// touches the run row, its record and nothing that grows with the run.
// Journal bytes are decoded only by journal operations. Corrupt evidence
// prevents replay but must not prevent terminal cancellation or erase itself.
#[derive(Serialize, Deserialize)]
struct StoredRun {
    /// The durable state version this row was written under. A row of another
    /// version (or none: the earlier per-run aggregate) is refused, never
    /// read as a run without evidence.
    version: u32,
    run: SavedRun,
    awaits: BTreeMap<String, i64>,
    listeners: BTreeMap<String, Vec<String>>,
    subscriptions: BTreeMap<String, Vec<String>>,
    waits: BTreeMap<String, owf::events::PendingWait>,
    budget_ended: Option<u64>,
    addressed_waits: std::collections::BTreeSet<String>,
    /// The rearm a firing attached and the trigger that firing stored: a
    /// trigger that differs at the run's end was replaced (a later epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rearm: Option<(Rearm, Trigger)>,
    /// Live membership of the trigger that admitted this run, across epochs.
    trigger: Option<String>,
    /// Copies accepted so far, the next copy's sequence number. Never reset,
    /// so a copy's sequence is also its position (`mailbox#expired[i]`).
    accepted: u64,
    /// Pending copies (expired ones included until consumed) per type.
    pending: BTreeMap<String, Pending>,
    /// Pending published copies, which a wildcard takes.
    published: u64,
    /// The earliest expiry among pending mailbox copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mailbox: Option<i64>,
    /// Records or copies are stored: `sweep` has evidence to remove.
    evidence: bool,
}
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct Pending {
    copies: u64,
    /// Of them, accepted CloudEvents (not lifecycle notifications).
    cloud: u64,
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
fn stored_run(value: Value) -> anyhow::Result<StoredRun> {
    let version = value.get("version").and_then(Value::as_u64);
    anyhow::ensure!(
        version == Some(u64::from(VERSION)),
        "unsupported durable state version {} of a run row (expected {VERSION}); earlier layouts are not converted",
        version.map_or_else(|| "none".into(), |v| v.to_string())
    );
    Ok(serde_json::from_value(value)?)
}
fn find_run(rows: &mut dyn Transaction, id: &str) -> anyhow::Result<Option<StoredRun>> {
    rows.get("run", id)?.map(stored_run).transpose()
}
fn run(rows: &mut dyn Transaction, id: &str) -> anyhow::Result<StoredRun> {
    find_run(rows, id)?.ok_or_else(|| anyhow::anyhow!("unknown run {id}"))
}
/// The layout version of a `definitions` row. The earlier layout (none: it
/// kept a predecessor and its triggers for a breaker rollback, and a boolean
/// trip) is refused, never read as a definition that cannot be tripped.
const DEFINITIONS_VERSION: u64 = 3;
fn load_definitions(rows: &mut dyn Transaction, name: &str) -> anyhow::Result<Option<Definitions>> {
    let Some(mut value) = rows.get("definitions", name)? else { return Ok(None) };
    let version = value.as_object_mut().and_then(|m| m.remove("version")).and_then(|v| v.as_u64());
    anyhow::ensure!(
        version == Some(DEFINITIONS_VERSION),
        "unsupported layout {} of the definitions row of {name} (expected {DEFINITIONS_VERSION}); earlier layouts are not converted",
        version.map_or_else(|| "none".into(), |v| v.to_string())
    );
    Ok(Some(serde_json::from_value(value)?))
}
fn store_definitions(rows: &mut dyn Transaction, name: &str, defs: &Definitions) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(defs)?;
    if let Value::Object(m) = &mut value {
        m.insert("version".into(), json!(DEFINITIONS_VERSION));
    }
    rows.put("definitions", name, &value)
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
/// A run's records, one row each, keyed by record key.
pub(crate) fn records_of(run: &str) -> String {
    partition("record", run)
}
fn events_of(run: &str) -> String {
    partition("event", run)
}
fn identities_of(run: &str) -> String {
    partition("event_id", run)
}
fn types_of(run: &str) -> String {
    partition("event_type", run)
}
fn pending_of(run: &str, type_: &str) -> String {
    format!("{}:{}", partition("pending", run), component(type_))
}
fn live_of(run: &str, type_: &str) -> String {
    format!("{}:{}", partition("live", run), component(type_))
}
fn published_of(run: &str) -> String {
    partition("published", run)
}
fn live_published_of(run: &str) -> String {
    partition("live_published", run)
}
fn expiring_of(run: &str) -> String {
    partition("expiry", run)
}
/// Copies are keyed by their fixed-width sequence, so ranges keep arrival order.
fn sequence(n: u64) -> String {
    format!("{n:016x}")
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
// Everything here derives from the run row; the copies' own index rows and
// `delivered` change with the copy, in the same commit.
fn indexes(s: &StoredRun) -> Rows {
    let mut keys = Rows::new();
    let id = &s.run.admission.id;
    let value = json!(id);
    keys.insert(key(&partition("name", &s.run.admission.name), id), value.clone());
    let started = timed(s.run.admitted_ms, id);
    for kind in run_kinds(&s.run, false) {
        keys.insert(key(&kind, &started), value.clone());
    }
    if let Some(at) = s.run.ended_ms {
        for kind in run_kinds(&s.run, true) {
            keys.insert(key(&kind, &timed(at, id)), value.clone());
        }
    }
    if let Some(link) = &s.run.child {
        keys.insert(key(&partition("control_parent", &link.parent), &started), value.clone());
    }
    for (k, w) in &s.waits {
        let entry = control::Wait {
            run: id.clone(),
            definition: s.run.admission.name.clone(),
            key: k.clone(),
            waiting: w.clone(),
        };
        let index = timed(w.since_ms, &format!("{}:{}", component(id), component(k)));
        for kind in wait_kinds(&entry) {
            keys.insert(key(&kind, &index), serde_json::to_value(&entry).expect("wait serializes"));
        }
    }
    if s.run.ended_ms.is_none() {
        keys.insert(key(&partition("live_name", &s.run.admission.name), id), value.clone());
        if let Some(trigger) = &s.trigger {
            keys.insert(key(&partition("trigger_live", trigger), id), value.clone());
        }
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
            // Child waiting notifications are addressed internally. Their
            // automatic registration needs no competing-delivery index until
            // an explicit listen registers that type.
            let internal =
                t.strip_prefix("waiting:").is_some_and(|id| s.listeners.contains_key(&format!("child:{id}")));
            if !s.pending.contains_key(t) && (!internal || s.addressed_waits.contains(t)) {
                keys.insert(key(&partition("wait", t), &timed(*at, id)), value.clone());
            }
        }
        // A publish reads only the runs subscribed to its type (or to `*`).
        for t in s.subscriptions.values().flatten() {
            keys.insert(key(&partition("subs", t), id), value.clone());
        }
        // The earliest mailbox expiry, for `sweep`.
        if let Some(at) = s.mailbox {
            keys.insert(key("mailbox", &timed(at, id)), value.clone());
        }
    } else if s.evidence {
        keys.insert(key("finished", &timed(s.run.ended_ms.unwrap(), id)), value.clone());
    }
    keys
}
fn store_run(rows: &mut dyn Transaction, snapshot: &StoredRun) -> anyhow::Result<()> {
    let id = &snapshot.run.admission.id;
    let old = find_run(rows, id)?;
    let before = old.as_ref().map(indexes).unwrap_or_default();
    store_run_indexed(rows, snapshot, &before)
}
fn store_run_indexed(rows: &mut dyn Transaction, snapshot: &StoredRun, before: &Rows) -> anyhow::Result<()> {
    let id = &snapshot.run.admission.id;
    let after = indexes(snapshot);
    for ((kind, id), value) in crate::backend::changes(before, &after) {
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
fn authority(rows: &mut dyn Transaction, fence: &Fence, now: i64, skew: i64) -> anyhow::Result<StoredRun> {
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
fn is_stopped(rows: &mut dyn Transaction) -> anyhow::Result<bool> {
    Ok(get::<bool>(rows, "control", "stopped")?.unwrap_or(false))
}
fn running(rows: &mut dyn Transaction) -> anyhow::Result<()> {
    if is_stopped(rows)? {
        return Err(control::Stopped.into());
    }
    Ok(())
}
fn checked(rows: &mut dyn Transaction, fence: &Fence, now: i64, skew: i64) -> anyhow::Result<StoredRun> {
    running(rows)?;
    authority(rows, fence, now, skew)
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
/// belongs to (when it runs the active version) are fixed here. A tripped
/// definition admits no new run, of any version and by any path, in the
/// same commit that reads the trip; an existing run is still joined.
fn insert(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<bool> {
    let Some(snapshot) = admitted(rows, admission, now)? else { return Ok(false) };
    store_run(rows, &snapshot)?;
    Ok(true)
}
/// The row a new run starts with, not stored yet; `None`: already admitted.
fn admitted(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<Option<StoredRun>> {
    if let Some(existing) = find_run(rows, &admission.id)? {
        anyhow::ensure!(existing.run.admission.name == admission.name, "run already belongs to another definition");
        return Ok(None);
    }
    running(rows)?;
    let defs = load_definitions(rows, &admission.name)?;
    if let Some(trip) = defs.as_ref().and_then(|d| d.tripped.as_ref()) {
        return Err(trip.refusal(&admission.name).into());
    }
    let epoch = defs.filter(|d| d.active == Some(admission.version)).map(|d| d.epoch);
    Ok(Some(StoredRun {
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
            root: None,
        },
        version: VERSION,
        awaits: BTreeMap::new(),
        listeners: BTreeMap::new(),
        subscriptions: BTreeMap::new(),
        waits: BTreeMap::new(),
        budget_ended: None,
        addressed_waits: Default::default(),
        rearm: None,
        trigger: None,
        accepted: 0,
        pending: BTreeMap::new(),
        published: 0,
        mailbox: None,
        evidence: false,
    }))
}
/// A refused trigger run (`Firing::refused`): admitted under the same rules
/// as `insert` and ended faulted in the same write, so it is never due and
/// never claimed. Its fault feeds the breaker as a terminal one would. A
/// run already stored under its id is joined: nothing is counted again.
fn refuse(rows: &mut dyn Transaction, refused: &Refused, now: i64) -> anyhow::Result<()> {
    let Some(mut snapshot) = admitted(rows, &refused.admission, now)? else { return Ok(()) };
    let status = Status::Faulted(refused.error.clone());
    snapshot.run.state = RunState::Failed;
    snapshot.run.outcome = Some(status);
    snapshot.run.ended_ms = Some(now);
    store_run(rows, &snapshot)?;
    breaker(rows, &snapshot.run, Some(&refused.error), false, refused.breaker_threshold, now)
}
fn close(snapshot: &mut StoredRun, types: &[String]) {
    for t in types {
        if !snapshot.listeners.values().any(|types| types.contains(t)) {
            snapshot.awaits.remove(t);
            snapshot.addressed_waits.remove(t);
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
fn subscribed(snapshot: &StoredRun, event: &StoredEvent) -> bool {
    event.envelope.is_some()
        && snapshot
            .subscriptions
            .values()
            .any(|types| types.contains(&event.type_) || (published(event) && types.iter().any(|t| t == "*")))
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
fn copy(rows: &mut dyn Transaction, run: &str, n: u64) -> anyhow::Result<StoredEvent> {
    get(rows, &events_of(run), &sequence(n))?.ok_or_else(|| anyhow::anyhow!("dangling event index {run}/{n}"))
}
/// The index rows of a pending copy: by type in arrival order and by expiry,
/// the same for published copies, and the mailbox expiry.
fn pending_rows(run: &str, n: u64, e: &StoredEvent) -> Vec<(String, String, Value)> {
    // A copy that never expires sorts after every instant, `i64::MAX` included.
    let live = e.expires_at_ms.map_or_else(|| format!("~:{}", sequence(n)), |at| timed(at, &sequence(n)));
    let mut keys = vec![
        (pending_of(run, &e.type_), sequence(n), json!(n)),
        (live_of(run, &e.type_), live.clone(), json!(e.envelope.is_some())),
    ];
    if published(e) {
        keys.push((published_of(run), sequence(n), json!(n)));
        keys.push((live_published_of(run), live, json!(n)));
    }
    if let Some(at) = e.expires_at_ms {
        keys.push((expiring_of(run), timed(at, &sequence(n)), json!([at, n])));
    }
    keys
}
/// Stores a new copy with its rows and counters; the caller stores the run.
fn push(rows: &mut dyn Transaction, s: &mut StoredRun, event: StoredEvent) -> anyhow::Result<()> {
    let id = s.run.admission.id.clone();
    let n = s.accepted;
    s.accepted = n.checked_add(1).ok_or_else(|| anyhow::anyhow!("event sequence overflow"))?;
    put(rows, &events_of(&id), &sequence(n), &event)?;
    if let Some(identity) = event.envelope.as_ref().and_then(identity) {
        rows.put(&identities_of(&id), &component(&identity), &json!(n))?;
        // A delivered identity leads a redelivery back to its recipient for
        // as long as the recipient keeps that evidence.
        if event.delivered {
            rows.put(&partition("delivered", &identity), &id, &json!(id))?;
        }
    }
    if rows.get(&types_of(&id), &component(&event.type_))?.is_none() {
        rows.put(&types_of(&id), &component(&event.type_), &json!(n))?;
    }
    for (kind, k, value) in pending_rows(&id, n, &event) {
        rows.put(&kind, &k, &value)?;
    }
    let pending = s.pending.entry(event.type_.clone()).or_default();
    pending.copies += 1;
    pending.cloud += u64::from(event.envelope.is_some());
    s.published += u64::from(published(&event));
    if let Some(at) = event.expires_at_ms {
        s.mailbox = Some(s.mailbox.map_or(at, |m| m.min(at)));
    }
    s.evidence = true;
    Ok(())
}
/// Marks a pending copy consumed under `k` and drops its pending rows and
/// counters. The caller refreshes `mailbox` when it consumed a mailbox copy.
fn consume(
    rows: &mut dyn Transaction,
    s: &mut StoredRun,
    n: u64,
    mut event: StoredEvent,
    k: &str,
) -> anyhow::Result<()> {
    let id = s.run.admission.id.clone();
    for (kind, key, _) in pending_rows(&id, n, &event) {
        rows.delete(&kind, &key)?;
    }
    if let Some(pending) = s.pending.get_mut(&event.type_) {
        pending.copies = pending.copies.saturating_sub(1);
        pending.cloud = pending.cloud.saturating_sub(u64::from(event.envelope.is_some()));
        if pending.copies == 0 {
            s.pending.remove(&event.type_);
        }
    }
    s.published = s.published.saturating_sub(u64::from(published(&event)));
    event.consumed_key = Some(k.into());
    put(rows, &events_of(&id), &sequence(n), &event)
}
/// The earliest expiry left among pending mailbox copies: one indexed row.
fn refresh_mailbox(rows: &mut dyn Transaction, s: &mut StoredRun) -> anyhow::Result<()> {
    s.mailbox = match rows.scan(&expiring_of(&s.run.admission.id), "", None, 1)?.into_values().next() {
        Some(v) => Some(serde_json::from_value::<(i64, u64)>(v)?.0),
        None => None,
    };
    Ok(())
}
/// Consumes every pending copy whose mailbox time is over with an observable
/// `Record::Expired`, keyed by its position. Reads only the expired copies.
fn expire(rows: &mut dyn Transaction, s: &mut StoredRun, now: i64) -> anyhow::Result<usize> {
    if s.mailbox.is_none_or(|at| at > now) {
        return Ok(0);
    }
    let id = s.run.admission.id.clone();
    let due = rows.scan(&expiring_of(&id), "", Some(&until(now)), usize::MAX)?;
    for value in due.values() {
        let (_, n) = serde_json::from_value::<(i64, u64)>(value.clone())?;
        let event = copy(rows, &id, n)?;
        let k = format!("mailbox#expired[{n}]");
        put(rows, &records_of(&id), &k, &Record::Expired { event: envelope(&event)? })?;
        consume(rows, s, n, event, &k)?;
    }
    refresh_mailbox(rows, s)?;
    Ok(due.len())
}
/// Whether a pending, unexpired copy is one the run waits or is subscribed
/// for. The counters answer unless a copy has expired unconsumed; then one
/// bounded read per waited or subscribed type with pending copies.
fn pending_wanted(rows: &mut dyn Transaction, s: &StoredRun, now: i64) -> anyhow::Result<bool> {
    let id = &s.run.admission.id;
    let fresh = s.mailbox.is_none_or(|at| at > now);
    // Copies expiring after `now` (or never), at most `limit` of them.
    let mut live = |kind: String, limit: usize| -> anyhow::Result<Vec<Value>> {
        Ok(rows.scan(&kind, &until(now), None, limit)?.into_values().collect())
    };
    for t in s.awaits.keys() {
        if s.pending.contains_key(t) && (fresh || !live(live_of(id, t), 1)?.is_empty()) {
            return Ok(true);
        }
    }
    for t in s.subscriptions.values().flatten() {
        if t == "*" && s.published > 0 && (fresh || !live(live_published_of(id), 1)?.is_empty()) {
            return Ok(true);
        }
        // A type keeps at most one pending lifecycle notification, so two
        // live copies include a CloudEvent if one is live.
        if s.pending.get(t).is_some_and(|p| p.cloud > 0)
            && (fresh || live(live_of(id, t), 2)?.iter().any(|cloud| cloud == &json!(true)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Accepts `event` into the run, unless the run already holds its identity
/// (pending, taken or expired): `false` for the same payload, an
/// `IdentityConflict` for another. Wakes a run that waits for it.
fn accept(rows: &mut dyn Transaction, snapshot: &mut StoredRun, event: StoredEvent, now: i64) -> anyhow::Result<bool> {
    if let Some(id) = event.envelope.as_ref().and_then(identity) {
        let run = snapshot.run.admission.id.clone();
        if let Some(n) = get::<u64>(rows, &identities_of(&run), &component(&id))? {
            let held = copy(rows, &run, n)?;
            if event.envelope.as_ref().map(payload) != held.envelope.as_ref().map(payload) {
                return Err(IdentityConflict(id.replace('\u{0}', "/")).into());
            }
            return Ok(false);
        }
    }
    if wanted(snapshot, &event, now) {
        snapshot.run.wake_at_ms = snapshot.run.wake_at_ms.min(now);
    }
    push(rows, snapshot, event)?;
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
        Some(target) => find_run(rows, target)?,
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
    if snapshot.run.ended_ms.is_some() && rows.get(&identities_of(&recipient), &component(&id))?.is_none() {
        return Ok(None);
    }
    if accept(rows, &mut snapshot, stored, now)? {
        store_run(rows, &snapshot)?;
    }
    Ok(Some(recipient))
}
/// A child's lifecycle notification to its parent, which registered for it
/// when it started the child: one pending copy per type, no identity.
fn notify(rows: &mut dyn Transaction, parent: &str, t: &str, data: &Value, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, parent)?;
    if snapshot.run.ended_ms.is_some() || !snapshot.awaits.contains_key(t) || snapshot.pending.contains_key(t) {
        return Ok(());
    }
    let event = StoredEvent {
        type_: t.into(),
        data: data.clone(),
        delivered_at_ms: now,
        consumed_key: None,
        envelope: None,
        delivered: false,
        expires_at_ms: None,
    };
    push(rows, &mut snapshot, event)?;
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
        Some(id) => find_run(rows, id)?.into_iter().collect(),
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
        if eligible && accept(rows, &mut snapshot, stored.clone(), now)? {
            receivers.push(snapshot.run.admission.id.clone());
            store_run(rows, &snapshot)?;
        }
    }
    Ok(receivers)
}
/// Removes a finished run's evidence: its records and copies with all their
/// rows. Reads only that run's rows; returns how many records it held.
fn forget(rows: &mut dyn Transaction, s: &mut StoredRun) -> anyhow::Result<usize> {
    let id = s.run.admission.id.clone();
    let records = rows.scan(&records_of(&id), "", None, usize::MAX)?;
    for (kind, k) in records.keys() {
        rows.delete(kind, k)?;
    }
    for ((kind, k), value) in rows.scan(&events_of(&id), "", None, usize::MAX)? {
        let event: StoredEvent = serde_json::from_value(value)?;
        let n = u64::from_str_radix(&k, 16)?;
        if event.consumed_key.is_none() {
            for (kind, key, _) in pending_rows(&id, n, &event) {
                rows.delete(&kind, &key)?;
            }
        }
        if let Some(identity) = event.envelope.as_ref().and_then(identity) {
            rows.delete(&identities_of(&id), &component(&identity))?;
            if event.delivered {
                rows.delete(&partition("delivered", &identity), &id)?;
            }
        }
        rows.delete(&kind, &k)?;
    }
    for (kind, k) in rows.scan(&types_of(&id), "", None, usize::MAX)?.into_keys() {
        rows.delete(&kind, &k)?;
    }
    s.pending.clear();
    s.published = 0;
    s.mailbox = None;
    s.evidence = false;
    Ok(records.len())
}

// ── 4. triggers, ingress and activation epochs ─────────────────────────────

/// One buffered slot. Admission and bindings are pinned before buffering;
/// promotion and the predecessor's terminal transition share a commit.
#[derive(Serialize, Deserialize)]
struct Queued {
    version: u32,
    admission: Admission,
    rearm: Option<Rearm>,
    max_concurrent_runs: usize,
    failure: Option<(WorkflowError, u32)>,
}
fn queued(rows: &mut dyn Transaction, k: &str) -> anyhow::Result<Option<Queued>> {
    let q = get::<Queued>(rows, "trigger_queue", k)?;
    anyhow::ensure!(q.as_ref().is_none_or(|q| q.version == 1), "unsupported trigger queue layout");
    Ok(q)
}
fn remove_queue(rows: &mut dyn Transaction, k: &str) -> anyhow::Result<()> {
    if let Some(q) = queued(rows, k)? {
        rows.delete("trigger_queue", k)?;
        rows.delete(&partition("queue_name", &q.admission.name), k)?;
    }
    Ok(())
}
fn capacity(rows: &mut dyn Transaction, name: &str, max: usize) -> anyhow::Result<bool> {
    Ok(max > 0 && rows.scan(&partition("live_name", name), "", None, max)?.len() < max)
}
fn admit_firing(
    rows: &mut dyn Transaction,
    k: &str,
    a: &Admission,
    rearm: Option<&Rearm>,
    next: Option<&Trigger>,
    now: i64,
) -> anyhow::Result<()> {
    if insert(rows, a, now)? {
        let mut s = run(rows, &a.id)?;
        s.trigger = Some(k.into());
        s.rearm = rearm.zip(next).map(|(r, t)| (r.clone(), t.clone()));
        store_run(rows, &s)?;
    }
    put(rows, "trigger_run", k, &a.id)
}
fn drain_queues(rows: &mut dyn Transaction, name: &str, now: i64) -> anyhow::Result<()> {
    if is_stopped(rows)? {
        if !rows.scan(&partition("queue_name", name), "", None, 1)?.is_empty() {
            put(rows, "control_drain", name, &true)?;
        }
        return Ok(());
    }
    for ((_, k), _) in rows.scan(&partition("queue_name", name), "", None, usize::MAX)? {
        let Some(q) = queued(rows, &k)? else {
            // A prior promotion in this batch may trip and disarm the
            // definition, removing the remaining buffered slots.
            anyhow::ensure!(rows.get(&partition("queue_name", name), &k)?.is_none(), "dangling queue index");
            continue;
        };
        if !rows.scan(&partition("trigger_live", &k), "", None, 1)?.is_empty()
            || !capacity(rows, name, q.max_concurrent_runs)?
        {
            continue;
        }
        let t = get::<Trigger>(rows, "trigger", &k)?.ok_or_else(|| anyhow::anyhow!("queue without trigger"))?;
        remove_queue(rows, &k)?;
        if let Some((error, breaker_threshold)) = q.failure {
            refuse(rows, &Refused { admission: q.admission, error, breaker_threshold }, now)?;
        } else {
            admit_firing(rows, &k, &q.admission, q.rearm.as_ref(), Some(&t), now)?;
        }
    }
    Ok(())
}

fn drain_pending(rows: &mut dyn Transaction, now: i64, limit: usize) -> anyhow::Result<()> {
    for ((_, name), _) in rows.scan("control_drain", "", None, limit)? {
        rows.delete("control_drain", &name)?;
        drain_queues(rows, &name, now)?;
    }
    Ok(())
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
    for k in &defs.triggers {
        remove_queue(rows, k)?;
    }
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
    if is_stopped(rows)? {
        return Ok(Fired::Changed);
    }
    anyhow::ensure!(f.refused.is_none() || f.admit.is_empty(), "a firing admits its runs or records a refusal");
    let current: Option<Trigger> = get(rows, "trigger", f.key)?;
    if current.as_ref() != f.expected {
        return Ok(Fired::Changed);
    }
    // Validate ingress before any policy outcome: a skipped or buffered event
    // is consumed too, and a conflicting payload must never cancel a run.
    if let Some(event) = f.ingress
        && tombstone(rows, event, now)?.2
    {
        return Ok(Fired::Ingested);
    }
    let mut disposition = Fired::Committed;
    let mut next = f.next;
    let candidate = f.admit.first().or(f.refused.map(|r| &r.admission));
    if let (Some(guard), Some(a)) = (f.overlap, candidate) {
        anyhow::ensure!(f.admit.len() <= 1, "overlap policy requires one admission");
        anyhow::ensure!(guard.max_concurrent_runs > 0, "schedule ceiling must be positive");
        anyhow::ensure!(f.next.is_some() && f.busy.is_some(), "overlap firing requires next and busy triggers");
        // A slot already admitted is joined, even after its ingress
        // tombstone expires. Replacement must never cancel its own run.
        if find_run(rows, &a.id)?.is_none() {
            let limit = if guard.policy == Overlap::Replace { usize::MAX } else { 1 };
            let live = rows.scan(&partition("trigger_live", f.key), "", None, limit)?;
            if guard.policy == Overlap::Replace {
                for id in live.values() {
                    let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid trigger run index"))?;
                    cancel_run(rows, id, now)?;
                }
            }
            if !capacity(rows, &a.name, guard.max_concurrent_runs)? {
                next = Some(guard.ceiling);
                disposition = Fired::Ceiling;
            } else if !live.is_empty() && guard.policy != Overlap::Allow && guard.policy != Overlap::Replace {
                if guard.policy == Overlap::Queue && queued(rows, f.key)?.is_none() {
                    let q = Queued {
                        version: 1,
                        admission: a.clone(),
                        rearm: f.rearm.cloned(),
                        max_concurrent_runs: guard.max_concurrent_runs,
                        failure: f.refused.map(|r| (r.error.clone(), r.breaker_threshold)),
                    };
                    put(rows, "trigger_queue", f.key, &q)?;
                    rows.put(&partition("queue_name", &a.name), f.key, &json!(f.key))?;
                    disposition = Fired::Queued;
                } else {
                    next = f.busy;
                    disposition = Fired::Busy(live.values().next().and_then(Value::as_str).unwrap_or_default().into());
                }
            }
        }
    } else if f.overlap.is_some() && !f.admit.is_empty() {
        anyhow::bail!("overlap policy requires one admission");
    } else if let Some(busy) = f.busy
        && (!f.admit.is_empty() || f.refused.is_some())
        && let Some(last) = get::<String>(rows, "trigger_run", f.key)?
        && find_run(rows, &last)?.is_some_and(|s| s.run.ended_ms.is_none())
    {
        next = Some(busy);
        disposition = Fired::Busy(last);
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
    store_trigger(rows, f.key, current.as_ref(), next)?;
    if disposition == Fired::Committed {
        for a in f.admit {
            admit_firing(rows, f.key, a, f.rearm, next, now)?;
        }
    }
    if disposition == Fired::Committed
        && let Some(r) = f.refused
    {
        refuse(rows, r, now)?;
        put(rows, "trigger_run", f.key, &r.admission.id)?;
    }
    Ok(disposition)
}

fn activate(
    rows: &mut dyn Transaction,
    name: &str,
    version: u64,
    epoch: Option<u64>,
    arming: &Arming,
) -> anyhow::Result<Option<Definitions>> {
    let mut defs = load_definitions(rows, name)?.unwrap_or_default();
    anyhow::ensure!(defs.versions.contains_key(&version), "unknown definition version");
    if epoch.is_some_and(|e| e != defs.epoch) {
        return Ok(None);
    }
    // The only transition that starts an epoch and the only one that clears
    // a trip. A trip's notice stays until the host acknowledges it.
    defs.active = Some(version);
    defs.epoch = defs.epoch.checked_add(1).ok_or_else(|| anyhow::anyhow!("activation epoch overflow"))?;
    defs.consecutive_faults = 0;
    defs.tripped = None;
    rows.delete("paused", name)?;
    arm(rows, &mut defs, &arming.triggers)?;
    store_definitions(rows, name, &defs)?;
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
    if kind == "done" && rows.get(&types_of(&link.parent), &component(&t))?.is_some() {
        return Ok(());
    }
    if kind == "done" {
        // A terminal child can never report waiting again. Close the automatic
        // registration, while retaining any explicit listen for that type.
        let mut parent = run(rows, &link.parent)?;
        let waiting = format!("waiting:{id}");
        let had = parent.awaits.contains_key(&waiting);
        close(&mut parent, std::slice::from_ref(&waiting));
        if had && !parent.awaits.contains_key(&waiting) {
            store_run(rows, &parent)?;
        }
    }
    notify(rows, &link.parent, &t, &data, now)
}
fn terminate(rows: &mut dyn Transaction, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, id)?;
    let root = snapshot.run.root.as_deref().unwrap_or(id);
    let spent: u64 = get(rows, "budget", root)?.unwrap_or(0);
    snapshot.budget_ended = Some(spent);
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
    snapshot.addressed_waits.clear();
    snapshot.listeners.clear();
    snapshot.subscriptions.clear();
    snapshot.waits.clear();
    store_run(rows, &snapshot)?;
    // The tree ends with its root (its owned descendants are cancelled
    // below, in this commit): nothing can spend its budget any more.
    if snapshot.run.root.is_none() && rows.get("budget", id)?.is_some() {
        rows.delete("budget", id)?;
    }
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
    let mut names = std::collections::BTreeSet::from([snapshot.run.admission.name.clone()]);
    let mut parents = vec![id.to_string()];
    while let Some(parent) = parents.pop() {
        for mut child in indexed_runs(rows, &partition("children", &parent), "", None, usize::MAX)? {
            // The indexed read already holds this child's primary row in
            // the transaction; reuse its indexes instead of reading it twice.
            let before = indexes(&child);
            child.budget_ended = Some(spent);
            child.run.state = RunState::Cancelled;
            child.run.outcome = Some(Status::Cancelled);
            child.run.ended_ms = Some(now);
            child.run.fence = None;
            child.run.lease_until_ms = None;
            child.awaits.clear();
            child.addressed_waits.clear();
            child.listeners.clear();
            child.subscriptions.clear();
            names.insert(child.run.admission.name.clone());
            child.waits.clear();
            parents.push(child.run.admission.id.clone());
            store_run_indexed(rows, &child, &before)?;
            lifecycle(rows, &child.run.admission.id, &Status::Cancelled, now)?;
        }
    }
    lifecycle(rows, id, status, now)?;
    for name in names {
        drain_queues(rows, &name, now)?;
    }
    Ok(())
}
fn cancel_run(rows: &mut dyn Transaction, id: &str, now: i64) -> anyhow::Result<bool> {
    let Some(s) = find_run(rows, id)? else { return Ok(false) };
    if s.run.ended_ms.is_some() {
        return Ok(false);
    }
    terminate(rows, id, &Status::Cancelled, now)?;
    Ok(true)
}
/// The notice of a trip, keyed by its time, then the definition and epoch.
fn notice_key(name: &str, trip: &Trip) -> String {
    timed(trip.at_ms, &format!("{}:{:016x}", component(name), trip.epoch))
}
/// A terminal outcome's breaker observation, in the terminal commit. Only a
/// run admitted in the active version's current epoch counts, and only
/// while the breaker has not tripped. A trip reports, and nothing else: the
/// active version and the epoch stay, the triggers go, and the notice is
/// stored in this commit, so a trip is never committed unreported.
fn breaker(
    rows: &mut dyn Transaction,
    run: &SavedRun,
    fault: Option<&WorkflowError>,
    success: bool,
    threshold: u32,
    now: i64,
) -> anyhow::Result<()> {
    let name = &run.admission.name;
    let Some(mut defs) = load_definitions(rows, name)? else {
        return Ok(());
    };
    if defs.active != Some(run.admission.version) || run.epoch != Some(defs.epoch) || defs.tripped.is_some() {
        return Ok(());
    }
    match fault {
        None if success && defs.consecutive_faults > 0 => defs.consecutive_faults = 0,
        Some(reason) => {
            defs.consecutive_faults = defs.consecutive_faults.saturating_add(1);
            let threshold = threshold.max(1);
            if defs.consecutive_faults >= threshold {
                let trip = Trip {
                    manual: None,
                    version: run.admission.version,
                    epoch: defs.epoch,
                    faults: defs.consecutive_faults,
                    threshold,
                    run: run.admission.id.clone(),
                    reason: reason.clone(),
                    at_ms: now,
                };
                arm(rows, &mut defs, &BTreeMap::new())?;
                rows.put("notice", &notice_key(name, &trip), &trip.event(name))?;
                defs.tripped = Some(trip);
            }
        }
        None => return Ok(()),
    }
    store_definitions(rows, name, &defs)
}

// ── 6. the RunStore implementation ─────────────────────────────────────────

impl RunStore for TransactionalStore {
    fn control(
        &self,
        request: &control::Request,
        command: &control::Command,
        now: i64,
    ) -> anyhow::Result<control::Reply> {
        self.update(|rows| control_command(rows, request, command, now, &self.policy))
    }
    fn stopped(&self) -> anyhow::Result<bool> {
        self.update(is_stopped)
    }
    fn control_entry(&self, id: &str) -> anyhow::Result<Option<control::Entry>> {
        self.update(|rows| get(rows, "control_command", id))
    }
    fn control_history(
        &self,
        run: Option<&str>,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Entry>> {
        self.update(|rows| {
            let kind = run.map_or_else(|| "control_history".into(), |r| partition("control_history_run", r));
            let (found, next) = page_rows(rows, &kind, &kind, None, None, cursor, limit)?;
            let items = found.into_values().map(|v| Ok(serde_json::from_value(v)?)).collect::<anyhow::Result<_>>()?;
            Ok(control::Page { items, next })
        })
    }
    fn query_runs(
        &self,
        filter: &control::Runs,
        history: bool,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<SavedRun>> {
        self.update(|rows| query_runs(rows, filter, history, cursor, limit))
    }
    fn pending_waits(
        &self,
        f: &control::Waits,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Wait>> {
        self.update(|rows| {
            let query = serde_json::to_string(&("waits", f))?;
            let (found, next) = page_rows(rows, &wait_kind(f), &query, f.since_ms, f.before_ms, cursor, limit)?;
            let mut items = Vec::new();
            for v in found.into_values() {
                let w: control::Wait = serde_json::from_value(v)?;
                if f.definition.as_ref().is_none_or(|d| *d == w.definition)
                    && f.run.as_ref().is_none_or(|r| *r == w.run)
                    && f.event_type.as_ref().is_none_or(|t| w.waiting.types.contains(t))
                {
                    items.push(w);
                }
            }
            Ok(control::Page { items, next })
        })
    }
    fn describe_wait(&self, f: &Fence, k: &str, wait: &owf::events::PendingWait, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            let mut wait = wait.clone();
            if let Some(old) = s.waits.get(k) {
                wait.since_ms = old.since_ms;
            }
            if s.waits.get(k) != Some(&wait) {
                s.waits.insert(k.into(), wait);
                store_run(rows, &s)?;
            }
            Ok(())
        })
    }
    fn inspect_control(&self, id: &str) -> anyhow::Result<Option<control::Inspection>> {
        self.update(|rows| {
            let Some(s) = find_run(rows, id)? else {
                return Ok(None);
            };
            let waits = s.waits.clone();
            let root = s.run.root.as_deref().unwrap_or(id);
            let budget_spent = match s.budget_ended {
                Some(spent) => spent,
                None => get::<u64>(rows, "budget", root)?.unwrap_or(0),
            };
            let children = indexed_runs(rows, &partition("control_parent", id), "", None, usize::MAX)?
                .into_iter()
                .map(|s| s.run)
                .collect();
            let runtime = &s.run.admission.runtime;
            let budget_limit = runtime["limits"]["max_jumps"].as_u64().map(|host| {
                owf::model::parse_yaml(&s.run.admission.definition).map_or(host, |w| w.budget.max_jumps(host))
            });
            let trace_id = runtime["trace_id"].as_str().map(str::to_owned);
            let span_id = runtime["span_id"].as_str().map(str::to_owned);
            let snapshot = snapshot(rows, s)?;
            let calls = owf::durable::call_journal(&snapshot.records);
            Ok(Some(control::Inspection {
                snapshot,
                calls,
                waits,
                children,
                budget_spent,
                budget_limit,
                trace_id,
                span_id,
            }))
        })
    }

    fn policy(&self) -> Policy {
        self.policy
    }
    fn admit(&self, a: &Admission, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| insert(rows, a, now))
    }
    fn start(&self, a: &Admission, now: i64) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            let Some(mut s) = admitted(rows, a, now)? else {
                return Ok(None);
            };
            let fence = claim_run(&mut s, now, &owner)?;
            store_run(rows, &s)?;
            Ok(Some(fence))
        })
    }
    fn claim_as(&self, id: &str, now: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>> {
        let owner = uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            if is_stopped(rows)? {
                return Ok(None);
            }
            let Some(mut s) = find_run(rows, id)? else {
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
    fn check_lease(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| checked(rows, f, now, self.policy.skew_ms).map(|_| ()))
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
            let mut s = authority(rows, f, now, self.policy.skew_ms)?;
            s.run.state = RunState::Ready;
            s.run.fence = None;
            s.run.lease_until_ms = None;
            s.run.wake_at_ms = now;
            store_run(rows, &s)
        })
    }
    // One ordered range read of the run's own record rows.
    fn load_records(&self, f: &Fence, now: i64) -> anyhow::Result<BTreeMap<String, Record>> {
        self.update(|rows| {
            checked(rows, f, now, self.policy.skew_ms)?;
            rows.scan(&records_of(&f.run_id), "", None, usize::MAX)?
                .into_iter()
                .map(|((_, key), value)| Ok((key, serde_json::from_value(value)?)))
                .collect()
        })
    }

    // The fence check reads the run row and the save writes only the record's
    // row, in one serializable commit: an owner replaced before the commit
    // conflicts and fails the check on retry. Only a run's first record also
    // writes the run row, to mark evidence for `sweep`.
    fn save_record(&self, f: &Fence, k: &str, r: &Record, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            let kind = records_of(&f.run_id);
            let old = rows.get(&kind, k)?;
            let value = serde_json::to_value(r)?;
            if matches!(r, Record::Event { .. } | Record::Expired { .. }) {
                anyhow::ensure!(old.as_ref() == Some(&value), "event records must be committed by take_event");
            }
            if matches!(r, Record::Budget { .. } | Record::Exceeded { .. }) {
                anyhow::ensure!(old.as_ref() == Some(&value), "budget records must be committed by spend");
            }
            if let Some(old) = &old {
                check_transition(&serde_json::from_value(old.clone())?, r)?;
            }
            if old.as_ref() != Some(&value) {
                rows.put(&kind, k, &value)?;
            }
            if !s.evidence {
                s.evidence = true;
                store_run(rows, &s)?;
            }
            Ok(())
        })
    }
    fn register_wait(&self, f: &Fence, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            for t in types {
                s.addressed_waits.insert(t.clone());
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
            let id = f.run_id.as_str();
            if let Some(saved) = rows.get(&records_of(id), k)? {
                let Record::Event { event } = serde_json::from_value(saved)? else {
                    anyhow::bail!("invalid event checkpoint")
                };
                return Ok(Some(event));
            }
            // A copy past its mailbox time is consumed by its expiry record,
            // never by this listen.
            expire(rows, &mut s, now)?;
            // The first pending copy, in arrival order, of a listed type or,
            // for `*`, published: one indexed row per listed type.
            let mut kinds: Vec<String> = types.iter().map(|t| pending_of(id, t)).collect();
            if types.iter().any(|t| t == "*") {
                kinds.push(published_of(id));
            }
            let mut candidate: Option<u64> = None;
            for kind in kinds {
                if let Some(n) = rows.scan(&kind, "", None, 1)?.into_values().next() {
                    let n = n.as_u64().ok_or_else(|| anyhow::anyhow!("invalid pending event index"))?;
                    candidate = Some(candidate.map_or(n, |c| c.min(n)));
                }
            }
            let result = if let Some(n) = candidate {
                let stored = copy(rows, id, n)?;
                let event = envelope(&stored)?;
                if let Some(child) = stored.type_.strip_prefix("done:") {
                    s.listeners.remove(&format!("child:{child}"));
                }
                let mailbox = stored.expires_at_ms.is_some();
                consume(rows, &mut s, n, stored, k)?;
                if mailbox {
                    refresh_mailbox(rows, &mut s)?;
                }
                s.listeners.remove(k);
                s.waits.retain(|_, w| w.position != k);
                close(&mut s, types);
                put(rows, &records_of(id), k, &Record::Event { event: event.clone() })?;
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
            s.waits.remove(k);
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
            if is_stopped(rows)? {
                return Ok(Vec::new());
            }
            let keys = rows.scan("trigger_due", "", Some(&until(now)), limit)?;
            indexed_triggers(rows, keys)
        })
    }
    fn triggers_for(&self, t: &str, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.update(|rows| {
            if is_stopped(rows)? {
                return Ok(Vec::new());
            }
            let mut keys = rows.scan(&partition("trigger_type", t), "", None, limit)?;
            if t != "*" && keys.len() < limit {
                keys.extend(rows.scan(&partition("trigger_type", "*"), "", None, limit - keys.len())?);
            }
            indexed_triggers(rows, keys)
        })
    }
    fn queued(&self, key: &str) -> anyhow::Result<Option<Admission>> {
        self.update(|rows| Ok(queued(rows, key)?.map(|q| q.admission)))
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
                    if pending_wanted(rows, &s, now)? {
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
                    Status::Faulted(e) if !matches!(outcome, Status::Interrupted(_)) => Some(e),
                    _ => None,
                };
                let success = matches!(status, Status::Completed(_));
                breaker(rows, &s.run, fault, success, threshold, now)?;
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
            let fresh = admitted(rows, a, now)?;
            let inserted = fresh.is_some();
            let mut c = match fresh {
                Some(c) => c,
                None => run(rows, &a.id)?,
            };
            if inserted {
                c.run.child = Some(link.clone());
                c.run.root = Some(p.run.root.clone().unwrap_or_else(|| parent.run_id.clone()));
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
    // The fence check reads the run row, the decision is the record's row and
    // the count the tree's `budget` row: one commit, three rows (the run row
    // again only for a run's first evidence).
    fn spend(&self, f: &Fence, k: &str, limit: u64, now: i64) -> anyhow::Result<Record> {
        self.update(|rows| {
            let mut s = checked(rows, f, now, self.policy.skew_ms)?;
            let kind = records_of(&f.run_id);
            if let Some(saved) = rows.get(&kind, k)? {
                let record: Record = serde_json::from_value(saved)?;
                anyhow::ensure!(matches!(record, Record::Budget { .. }), "invalid budget record at {k}");
                return Ok(record);
            }
            let root = s.run.root.clone().unwrap_or_else(|| f.run_id.clone());
            let spent: u64 = get(rows, "budget", &root)?.unwrap_or(0);
            let granted = spent < limit;
            let spent = spent + u64::from(granted);
            if granted {
                put(rows, "budget", &root, &spent)?;
            }
            // A refusal names the run's first one, decided in the commit
            // that refused it.
            let first = if granted {
                None
            } else {
                match rows.get(&kind, EXCEEDED)? {
                    Some(saved) => match serde_json::from_value(saved)? {
                        Record::Exceeded { by } => Some(by),
                        _ => anyhow::bail!("invalid first refusal record"),
                    },
                    None => {
                        rows.put(&kind, EXCEEDED, &serde_json::to_value(Record::Exceeded { by: k.into() })?)?;
                        Some(k.to_string())
                    }
                }
            };
            let record = Record::Budget { granted, spent, first };
            rows.put(&kind, k, &serde_json::to_value(&record)?)?;
            if !s.evidence {
                s.evidence = true;
                store_run(rows, &s)?;
            }
            Ok(record)
        })
    }
    fn due_as(&self, now: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>> {
        self.update(|rows| {
            if is_stopped(rows)? {
                return Ok(Vec::new());
            }
            drain_pending(rows, now, limit)?;
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
        self.update(|rows| cancel_run(rows, id, now))
    }

    fn sweep(&self, now: i64, kept: i64) -> anyhow::Result<Swept> {
        anyhow::ensure!(kept >= 0, "negative retention");
        self.update(|rows| {
            let mut swept = Swept::default();
            let horizon = now.saturating_sub(kept);
            for mut s in indexed_runs(rows, "finished", "", Some(&until(horizon)), usize::MAX)? {
                if s.run.ended_ms.is_some_and(|at| at <= horizon) {
                    swept.records += forget(rows, &mut s)?;
                    store_run(rows, &s)?;
                }
            }
            // Unfinished runs whose mailbox holds a copy past its time.
            for mut s in indexed_runs(rows, "mailbox", "", Some(&until(now)), usize::MAX)? {
                swept.expired += expire(rows, &mut s, now)?;
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
        self.update(|rows| Ok(find_run(rows, id)?.map(|s| s.run)))
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
                    .map(|v| Ok(stored_run(v)?.run))
                    .collect()
            }
        })
    }
    fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>> {
        self.update(|rows| {
            let Some(s) = find_run(rows, id)? else {
                return Ok(None);
            };
            Ok(Some(snapshot(rows, s)?))
        })
    }

    fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        self.update(|rows| {
            let mut defs = load_definitions(rows, name)?.unwrap_or_default();
            let version = defs
                .versions
                .last_key_value()
                .map_or(Some(1), |(v, _)| v.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("definition version overflow"))?;
            let d = Definition { name: name.into(), version, yaml: yaml.into() };
            defs.versions.insert(version, d.clone());
            store_definitions(rows, name, &defs)?;
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
        self.update(|rows| Ok(load_definitions(rows, name)?.unwrap_or_default()))
    }
    // One ordered range read of at most `limit` notice rows.
    fn notices(&self, limit: usize) -> anyhow::Result<Vec<(String, Value)>> {
        self.update(|rows| Ok(rows.scan("notice", "", None, limit)?.into_iter().map(|((_, k), v)| (k, v)).collect()))
    }
    fn acknowledge(&self, key: &str) -> anyhow::Result<bool> {
        self.update(|rows| {
            if rows.get("notice", key)?.is_none() {
                return Ok(false);
            }
            rows.delete("notice", key)?;
            Ok(true)
        })
    }
}

// ── 7. operator commands and bounded indexes ──────────────────────────────

fn run_kinds(r: &SavedRun, history: bool) -> Vec<String> {
    let base = if history { "control_end" } else { "control_run" };
    let name = partition(base, &r.admission.name);
    let kinds = vec![base.into(), name];
    kinds
}
fn run_kind(filter: &control::Runs, history: bool) -> String {
    if !history && let Some(parent) = &filter.parent {
        return partition("control_parent", parent);
    }
    let base = if history { "control_end" } else { "control_run" };
    filter.definition.as_ref().map_or_else(|| base.into(), |d| partition(base, d))
}
fn wait_kinds(w: &control::Wait) -> Vec<String> {
    let name = partition("control_wait_definition", &w.definition);
    let mut kinds = vec!["control_wait".into(), name.clone(), partition("control_wait_run", &w.run)];
    for t in &w.waiting.types {
        kinds.push(partition("control_wait_type", t));
        kinds.push(format!("{name}:type:{}", component(t)));
    }
    kinds.sort();
    kinds.dedup();
    kinds
}
fn wait_kind(f: &control::Waits) -> String {
    if let Some(r) = &f.run {
        return partition("control_wait_run", r);
    }
    let name = f.definition.as_ref().map(|d| partition("control_wait_definition", d));
    match (name, &f.event_type) {
        (Some(n), Some(t)) => format!("{n}:type:{}", component(t)),
        (Some(n), None) => n,
        (None, Some(t)) => partition("control_wait_type", t),
        (None, None) => "control_wait".into(),
    }
}
fn page_rows(
    rows: &mut dyn Transaction,
    kind: &str,
    query: &str,
    since: Option<i64>,
    before: Option<i64>,
    cursor: Option<&control::Cursor>,
    limit: usize,
) -> anyhow::Result<(Rows, Option<control::Cursor>)> {
    anyhow::ensure!((1..=control::MAX_PAGE).contains(&limit), "page limit must be 1..={}", control::MAX_PAGE);
    anyhow::ensure!(since.zip(before).is_none_or(|(a, b)| a <= b), "invalid time window");
    let start = match cursor {
        Some(c) => {
            anyhow::ensure!(c.query == query, "cursor belongs to another query");
            c.after.clone()
        }
        None => since.map_or_else(String::new, time_key),
    };
    let end = before.map(time_key);
    if end.as_ref().is_some_and(|end| start >= *end) {
        return Ok((Rows::new(), None));
    }
    let found = rows.scan(kind, &start, end.as_deref(), limit + usize::from(cursor.is_some()))?;
    let found: Rows = found.into_iter().filter(|((_, k), _)| cursor.is_none_or(|c| *k > c.after)).take(limit).collect();
    let next = if found.len() == limit {
        found.last_key_value().map(|((_, k), _)| control::Cursor { query: query.into(), after: k.clone() })
    } else {
        None
    };
    Ok((found, next))
}
fn query_runs(
    rows: &mut dyn Transaction,
    f: &control::Runs,
    history: bool,
    cursor: Option<&control::Cursor>,
    limit: usize,
) -> anyhow::Result<control::Page<SavedRun>> {
    let query = serde_json::to_string(&("runs", f, history))?;
    let kind = run_kind(f, history);
    let (found, next) = page_rows(rows, &kind, &query, f.since_ms, f.before_ms, cursor, limit)?;
    let mut items = Vec::new();
    for v in found.into_values() {
        let id = v.as_str().ok_or_else(|| anyhow::anyhow!("invalid control run index"))?;
        let r = run(rows, id)?.run;
        if f.definition.as_ref().is_none_or(|d| *d == r.admission.name)
            && f.state.as_ref().is_none_or(|state| *state == r.state)
            && f.parent.as_ref().is_none_or(|p| r.child.as_ref().is_some_and(|c| c.parent == *p))
        {
            items.push(r);
        }
    }
    Ok(control::Page { items, next })
}
fn suspend(rows: &mut dyn Transaction, name: &str, defs: &mut Definitions) -> anyhow::Result<()> {
    let mut triggers = BTreeMap::new();
    let mut queues = BTreeMap::new();
    for k in &defs.triggers {
        if let Some(t) = get(rows, "trigger", k)? {
            triggers.insert(k.clone(), t);
        }
        if let Some(queue) = rows.get("trigger_queue", k)? {
            queues.insert(k.clone(), queue);
        }
    }
    put(rows, "paused", name, &control::Paused { epoch: defs.epoch, triggers, queues })?;
    arm(rows, defs, &BTreeMap::new())
}
fn normalize_command(command: &control::Command) -> control::Command {
    match command {
        control::Command::Respond { run, wait, event } => {
            let mut event = payload(event);
            if let Value::Object(m) = &mut event {
                m.entry("specversion").or_insert(json!("1.0"));
            }
            control::Command::Respond { run: run.clone(), wait: wait.clone(), event }
        }
        c => c.clone(),
    }
}
fn control_command(
    rows: &mut dyn Transaction,
    request: &control::Request,
    command: &control::Command,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<control::Reply> {
    use control::{Command, Reply};
    anyhow::ensure!(
        !request.id.trim().is_empty() && !request.actor.trim().is_empty() && !request.reason.trim().is_empty(),
        "control request requires id, actor and reason"
    );
    let canonical = normalize_command(command);
    if let Some(entry) = get::<control::Entry>(rows, "control_command", &request.id)? {
        anyhow::ensure!(entry.request == *request && entry.command == canonical, "control request identity conflict");
        return Ok(entry.reply);
    }
    let reply = match command {
        Command::Cancel { run: id } => {
            let changed = find_run(rows, id)?.is_some_and(|s| s.run.ended_ms.is_none());
            if changed {
                terminate(rows, id, &Status::Cancelled, now)?;
            }
            Reply::Changed(changed)
        }
        Command::Stop { enabled } => {
            let changed = is_stopped(rows)? != *enabled;
            if changed {
                if *enabled {
                    put(rows, "control", "stopped", enabled)?;
                } else {
                    rows.delete("control", "stopped")?;
                    drain_pending(rows, now, control::MAX_PAGE)?;
                }
            }
            Reply::Changed(changed)
        }
        Command::Pause { definition: name, note } => {
            let mut defs = load_definitions(rows, name)?.ok_or_else(|| anyhow::anyhow!("unknown definition"))?;
            let changed = defs.tripped.is_none();
            if changed {
                let version = defs.active.ok_or_else(|| anyhow::anyhow!("definition has no active version"))?;
                suspend(rows, name, &mut defs)?;
                defs.tripped = Some(Trip {
                    manual: Some(control::ManualTrip {
                        actor: request.actor.clone(),
                        reason: request.reason.clone(),
                        note: note.clone(),
                    }),
                    version,
                    epoch: defs.epoch,
                    faults: defs.consecutive_faults,
                    threshold: 0,
                    run: String::new(),
                    reason: WorkflowError {
                        type_: "manual".into(),
                        status: 409,
                        title: Some("Definition paused".into()),
                        detail: Some(request.reason.clone()),
                        instance: None,
                    },
                    at_ms: now,
                });
                store_definitions(rows, name, &defs)?;
            }
            Reply::Changed(changed)
        }
        Command::Resume { definition: name } => {
            let mut defs = load_definitions(rows, name)?.ok_or_else(|| anyhow::anyhow!("unknown definition"))?;
            anyhow::ensure!(
                defs.tripped.as_ref().is_none_or(|t| t.manual.is_some()),
                "a breaker trip requires activation"
            );
            let changed = defs.tripped.is_some();
            if changed {
                let paused: control::Paused =
                    get(rows, "paused", name)?.ok_or_else(|| anyhow::anyhow!("missing suspended triggers"))?;
                anyhow::ensure!(paused.epoch == defs.epoch, "suspended activation changed");
                defs.tripped = None;
                arm(rows, &mut defs, &paused.triggers)?;
                for (key, value) in &paused.queues {
                    let q: Queued = serde_json::from_value(value.clone())?;
                    anyhow::ensure!(q.version == 1 && q.admission.name == *name, "invalid suspended queue");
                    rows.put("trigger_queue", key, value)?;
                    put(rows, &partition("queue_name", name), key, key)?;
                }
                rows.delete("paused", name)?;
                store_definitions(rows, name, &defs)?;
                drain_queues(rows, name, now)?;
            }
            Reply::Changed(changed)
        }
        Command::Respond { run: id, wait: key, event } => {
            let identity = identity(event).ok_or_else(|| anyhow::anyhow!("response requires event source and id"))?;
            let kind = partition("control_response", id);
            let dedup = component(&identity);
            if let Some(entry) = get::<control::Entry>(rows, &kind, &dedup)? {
                anyhow::ensure!(entry.command == canonical, "response event identity conflict");
                entry.reply
            } else {
                let s = run(rows, id)?;
                let w = s.waits.get(key).ok_or_else(|| anyhow::anyhow!("wait is no longer pending"))?;
                anyhow::ensure!(
                    s.run.ended_ms.is_none() && w.timeout_ms.is_none_or(|at| now < at),
                    "wait ended or timed out"
                );
                let mut event = event.clone();
                if let Value::Object(m) = &mut event {
                    m.entry("specversion").or_insert(json!("1.0"));
                    let time = chrono::DateTime::from_timestamp_millis(now)
                        .ok_or_else(|| anyhow::anyhow!("invalid response timestamp"))?
                        .to_rfc3339();
                    m.entry("time").or_insert(json!(time));
                }
                anyhow::ensure!(w.accepts(&event).map_err(anyhow::Error::msg)?, "event does not match wait");
                let result = Reply::Responded(deliver(rows, &event, Some(id), now, policy)?);
                let sequence: u64 = get(rows, &identities_of(id), &dedup)?
                    .ok_or_else(|| anyhow::anyhow!("response was not accepted"))?;
                // A validated answer is also offered to a wildcard listen.
                // Ordinary addressed deliveries retain their type-only routing.
                if w.types.iter().any(|t| t == "*") {
                    let mut stored = copy(rows, id, sequence)?;
                    anyhow::ensure!(stored.consumed_key.is_none(), "response event already consumed by another wait");
                    if stored.delivered {
                        stored.delivered = false;
                        let mut current = run(rows, id)?;
                        current.run.wake_at_ms = current.run.wake_at_ms.min(now);
                        current.published = current
                            .published
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("published counter overflow"))?;
                        put(rows, &events_of(id), &self::sequence(sequence), &stored)?;
                        put(rows, &published_of(id), &self::sequence(sequence), &sequence)?;
                        let live = stored.expires_at_ms.map_or_else(
                            || format!("~:{}", self::sequence(sequence)),
                            |at| timed(at, &self::sequence(sequence)),
                        );
                        put(rows, &live_published_of(id), &live, &sequence)?;
                        store_run(rows, &current)?;
                    }
                }
                put(
                    rows,
                    &kind,
                    &dedup,
                    &control::Entry {
                        request: request.clone(),
                        command: canonical.clone(),
                        at_ms: now,
                        reply: result.clone(),
                    },
                )?;
                result
            }
        }
    };
    let entry = control::Entry { request: request.clone(), command: canonical, at_ms: now, reply: reply.clone() };
    put(rows, "control_command", &request.id, &entry)?;
    let k = timed(now, &component(&request.id));
    put(rows, "control_history", &k, &entry)?;
    if let Command::Cancel { run } | Command::Respond { run, .. } = command {
        put(rows, &partition("control_history_run", run), &k, &entry)?;
    }
    Ok(reply)
}

fn snapshot(rows: &mut dyn Transaction, s: StoredRun) -> anyhow::Result<RunSnapshot> {
    let records = rows
        .scan(&records_of(&s.run.admission.id), "", None, usize::MAX)?
        .into_iter()
        .map(|((_, k), v)| Ok((k, serde_json::from_value(v)?)))
        .collect::<anyhow::Result<_>>()?;
    let events = rows
        .scan(&events_of(&s.run.admission.id), "", None, usize::MAX)?
        .into_values()
        .map(|v| Ok(serde_json::from_value(v)?))
        .collect::<anyhow::Result<_>>()?;
    Ok(RunSnapshot {
        run: s.run,
        records,
        awaits: s.awaits,
        listeners: s.listeners,
        events,
        subscriptions: s.subscriptions,
    })
}
