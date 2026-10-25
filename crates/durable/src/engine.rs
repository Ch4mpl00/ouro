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
use ows::{
    durable::{CallState, EXCEEDED, VERSION, check_transition},
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
    /// The content digest the admitted version had in the catalog when the
    /// run was admitted, copied from there and never recomputed: empty for
    /// a version the catalog did not hold. Admissions and trips compare
    /// stored digests only, so a later change to how digests are computed
    /// cannot alter the meaning of rows already written.
    digest: String,
    awaits: BTreeMap<String, i64>,
    listeners: BTreeMap<String, Vec<String>>,
    subscriptions: BTreeMap<String, Vec<String>>,
    waits: BTreeMap<String, ows::events::PendingWait>,
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

/// A definition's runs that ended within one bucket with annotation totals
/// (`totals:<hex name>`, keyed by the bucket's start): their count and sums.
#[derive(Default, Serialize, Deserialize)]
struct Bucket {
    runs: u64,
    annotations: ows::durable::Totals,
}
fn bucket_start(ms: i64) -> i64 {
    ms.saturating_sub(ms.rem_euclid(control::TOTALS_BUCKET_MS))
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
async fn get<T: DeserializeOwned>(rows: &mut dyn Transaction, kind: &str, id: &str) -> anyhow::Result<Option<T>> {
    rows.get(kind, id).await?.map(serde_json::from_value).transpose().map_err(Into::into)
}
async fn put(rows: &mut dyn Transaction, kind: &str, id: &str, value: &impl Serialize) -> anyhow::Result<()> {
    rows.put(kind, id, &serde_json::to_value(value)?).await
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
async fn find_run(rows: &mut dyn Transaction, id: &str) -> anyhow::Result<Option<StoredRun>> {
    rows.get("run", id).await?.map(stored_run).transpose()
}
async fn run(rows: &mut dyn Transaction, id: &str) -> anyhow::Result<StoredRun> {
    find_run(rows, id).await?.ok_or_else(|| anyhow::anyhow!("unknown run {id}"))
}
/// The layout version of a `definitions` row. The earlier layout (none: it
/// kept a predecessor and its triggers for a breaker rollback, and a boolean
/// trip) is refused, never read as a definition that cannot be tripped.
/// 4: each version's latest activation (`Definitions::activations`).
/// 5: each version's content digest, and the digest a trip refuses by.
/// 6: whether the breaker tripped (`Trip::breaker`), so a pause can stand on
/// top of a breaker's trip and `resume` lift only the pause.
const DEFINITIONS_VERSION: u64 = 6;
async fn load_definitions(rows: &mut dyn Transaction, name: &str) -> anyhow::Result<Option<Definitions>> {
    let Some(value) = rows.get("definitions", name).await? else { return Ok(None) };
    Ok(Some(decode_definitions(name, value)?))
}
fn decode_definitions(name: &str, mut value: Value) -> anyhow::Result<Definitions> {
    let version = value.as_object_mut().and_then(|m| m.remove("version")).and_then(|v| v.as_u64());
    anyhow::ensure!(
        version == Some(DEFINITIONS_VERSION),
        "unsupported layout {} of the definitions row of {name} (expected {DEFINITIONS_VERSION}); earlier layouts are not converted",
        version.map_or_else(|| "none".into(), |v| v.to_string())
    );
    Ok(serde_json::from_value(value)?)
}
async fn store_definitions(rows: &mut dyn Transaction, name: &str, defs: &Definitions) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(defs)?;
    if let Value::Object(m) = &mut value {
        m.insert("version".into(), json!(DEFINITIONS_VERSION));
    }
    rows.put("definitions", name, &value).await
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
        // A held run keeps its wake-up (and lease) but is due to nobody:
        // the index is rebuilt from the row when it is released.
        let due = if s.run.held.is_some() {
            None
        } else if s.run.state == RunState::Running {
            s.run.lease_until_ms
        } else {
            Some(s.run.wake_at_ms)
        };
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
async fn store_run(rows: &mut dyn Transaction, snapshot: &StoredRun) -> anyhow::Result<()> {
    let id = &snapshot.run.admission.id;
    let old = find_run(rows, id).await?;
    let before = old.as_ref().map(indexes).unwrap_or_default();
    store_run_indexed(rows, snapshot, &before).await
}
async fn store_run_indexed(rows: &mut dyn Transaction, snapshot: &StoredRun, before: &Rows) -> anyhow::Result<()> {
    let id = &snapshot.run.admission.id;
    let after = indexes(snapshot);
    for ((kind, id), value) in crate::backend::changes(before, &after) {
        if let Some(value) = value {
            rows.put(kind, id, value).await?;
        } else {
            rows.delete(kind, id).await?;
        }
    }
    put(rows, "run", id, snapshot).await
}
async fn indexed_runs(
    rows: &mut dyn Transaction,
    kind: &str,
    start: &str,
    end: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<StoredRun>> {
    let mut runs = Vec::new();
    for id in rows.scan(kind, start, end, limit).await?.into_values() {
        runs.push(run(rows, id.as_str().ok_or_else(|| anyhow::anyhow!("invalid run index"))?).await?);
    }
    Ok(runs)
}

// ── 2. admission, leases and compatibility ─────────────────────────────────

/// The current owner's authority: it acts only while its lease has more
/// than the skew bound left, so no other worker can have taken over yet.
async fn authority(rows: &mut dyn Transaction, fence: &Fence, now: i64, skew: i64) -> anyhow::Result<StoredRun> {
    let snapshot = run(rows, &fence.run_id).await?;
    anyhow::ensure!(
        snapshot.run.state == RunState::Running
            && snapshot.run.fence.as_ref() == Some(fence)
            && snapshot.run.lease_until_ms.is_some_and(|until| until > now.saturating_add(skew)),
        "run {} lease expired or was replaced",
        fence.run_id
    );
    Ok(snapshot)
}
async fn is_stopped(rows: &mut dyn Transaction) -> anyhow::Result<bool> {
    Ok(get::<bool>(rows, "control", "stopped").await?.unwrap_or(false))
}
async fn running(rows: &mut dyn Transaction) -> anyhow::Result<()> {
    if is_stopped(rows).await? {
        return Err(control::Stopped.into());
    }
    Ok(())
}
/// Every fenced step: no emergency stop, current ownership, and no operator
/// hold. A hold refuses with `control::Held`, the worker's cue to release.
async fn checked(rows: &mut dyn Transaction, fence: &Fence, now: i64, skew: i64) -> anyhow::Result<StoredRun> {
    running(rows).await?;
    let s = authority(rows, fence, now, skew).await?;
    not_held(&s)?;
    Ok(s)
}
fn not_held(s: &StoredRun) -> anyhow::Result<()> {
    if s.run.held.is_some() {
        return Err(control::Held(s.run.admission.id.clone()).into());
    }
    Ok(())
}
fn claimable(r: &SavedRun, now: i64) -> bool {
    if r.held.is_some() {
        return false;
    }
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
    // A running run is due from its lease's end: a takeover records that
    // instant as its wake-up, so `wake_at_ms` of a claimed run is always
    // when it became due (schedule-to-start latency). A running run's own
    // due position is its lease, so nothing else reads it.
    if r.state == RunState::Running
        && let Some(until) = r.lease_until_ms
    {
        r.wake_at_ms = until;
    }
    let fence = Fence { run_id: r.admission.id.clone(), owner: owner.into(), generation: r.generation };
    r.fence = Some(fence.clone());
    r.lease_until_ms = Some(now.saturating_add(LEASE_MS));
    r.state = RunState::Running;
    Ok(fence)
}
/// Admits a run once. Its admission instant and the activation epoch it
/// belongs to (when it runs the active version) are fixed here. A tripped
/// definition refuses, by any path, a new run whose definition has the
/// tripped content digest, whatever its version number, in the same commit
/// that reads the trip, and a version the catalog does not hold; other
/// content is admitted (a pinned start) and never counts toward the breaker. A manual pause refuses every admission of the
/// definition, whatever its content. An existing run is still joined.
async fn insert(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<bool> {
    let Some(snapshot) = admitted(rows, admission, now).await? else { return Ok(false) };
    store_run(rows, &snapshot).await?;
    Ok(true)
}
/// The row a new run starts with, not stored yet; `None`: already admitted.
async fn admitted(rows: &mut dyn Transaction, admission: &Admission, now: i64) -> anyhow::Result<Option<StoredRun>> {
    if let Some(existing) = find_run(rows, &admission.id).await? {
        anyhow::ensure!(existing.run.admission.name == admission.name, "run already belongs to another definition");
        return Ok(None);
    }
    running(rows).await?;
    let defs = load_definitions(rows, &admission.name).await?;
    if let Some(refusal) = barred(defs.as_ref(), admission) {
        return Err(refusal.into());
    }
    let digest = digest(defs.as_ref(), admission);
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
            annotations: Default::default(),
            held: None,
        },
        version: VERSION,
        digest,
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
/// The content digest of the admitted version as the catalog stores it
/// (`Definition::digest`): nothing is hashed here. Empty for a version the
/// catalog does not hold.
fn digest(defs: Option<&Definitions>, admission: &Admission) -> String {
    defs.and_then(|d| d.versions.get(&admission.version)).map_or_else(String::new, |d| d.digest.clone())
}
/// The refusal a standing trip gives a new run of `admission`: a pause
/// refuses every one; an automatic trip, the tripped content digest, compared
/// as stored (`Trip::digest`). A version the catalog does not hold has no
/// digest that could show its content to be other: refused (fail closed).
fn barred(defs: Option<&Definitions>, admission: &Admission) -> Option<DefinitionTripped> {
    let trip = defs?.tripped.as_ref()?;
    let digest = digest(defs, admission);
    (trip.manual.is_some() || digest.is_empty() || digest == trip.digest).then(|| trip.refusal(&admission.name))
}
/// After `drain_marked` drained (`drained`), the refusal the new run of
/// `admission` now meets if the drain tripped its definition (a promoted
/// refused slot reaching the threshold). The admission returns it inside a
/// committed transaction (`Ok(Err(refusal))`) with nothing else written: the
/// drain and its trip are kept, and the refusal names a stored trip.
async fn drained_refusal(
    rows: &mut dyn Transaction,
    drained: bool,
    admission: &Admission,
) -> anyhow::Result<Option<anyhow::Error>> {
    if !drained || find_run(rows, &admission.id).await?.is_some() {
        return Ok(None);
    }
    let defs = load_definitions(rows, &admission.name).await?;
    Ok(barred(defs.as_ref(), admission).map(Into::into))
}
/// A refused trigger run (`Firing::refused`): admitted under the same rules
/// as `insert` and ended faulted in the same write, so it is never due and
/// never claimed. Its fault feeds the breaker as a terminal one would. A
/// run already stored under its id is joined: nothing is counted again.
async fn refuse(rows: &mut dyn Transaction, refused: &Refused, now: i64) -> anyhow::Result<()> {
    let Some(mut snapshot) = admitted(rows, &refused.admission, now).await? else { return Ok(()) };
    let status = Status::Faulted(refused.error.clone());
    snapshot.run.state = RunState::Failed;
    snapshot.run.outcome = Some(status);
    snapshot.run.ended_ms = Some(now);
    store_run(rows, &snapshot).await?;
    breaker(rows, &snapshot.run, Some(&refused.error), false, refused.breaker_threshold, now).await
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
async fn copy(rows: &mut dyn Transaction, run: &str, n: u64) -> anyhow::Result<StoredEvent> {
    get(rows, &events_of(run), &sequence(n)).await?.ok_or_else(|| anyhow::anyhow!("dangling event index {run}/{n}"))
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
async fn push(rows: &mut dyn Transaction, s: &mut StoredRun, event: StoredEvent) -> anyhow::Result<()> {
    let id = s.run.admission.id.clone();
    let n = s.accepted;
    s.accepted = n.checked_add(1).ok_or_else(|| anyhow::anyhow!("event sequence overflow"))?;
    put(rows, &events_of(&id), &sequence(n), &event).await?;
    if let Some(identity) = event.envelope.as_ref().and_then(identity) {
        rows.put(&identities_of(&id), &component(&identity), &json!(n)).await?;
        // A delivered identity leads a redelivery back to its recipient for
        // as long as the recipient keeps that evidence.
        if event.delivered {
            rows.put(&partition("delivered", &identity), &id, &json!(id)).await?;
        }
    }
    if rows.get(&types_of(&id), &component(&event.type_)).await?.is_none() {
        rows.put(&types_of(&id), &component(&event.type_), &json!(n)).await?;
    }
    for (kind, k, value) in pending_rows(&id, n, &event) {
        rows.put(&kind, &k, &value).await?;
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
async fn consume(
    rows: &mut dyn Transaction,
    s: &mut StoredRun,
    n: u64,
    mut event: StoredEvent,
    k: &str,
) -> anyhow::Result<()> {
    let id = s.run.admission.id.clone();
    for (kind, key, _) in pending_rows(&id, n, &event) {
        rows.delete(&kind, &key).await?;
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
    put(rows, &events_of(&id), &sequence(n), &event).await
}
/// The earliest expiry left among pending mailbox copies: one indexed row.
async fn refresh_mailbox(rows: &mut dyn Transaction, s: &mut StoredRun) -> anyhow::Result<()> {
    s.mailbox = match rows.scan(&expiring_of(&s.run.admission.id), "", None, 1).await?.into_values().next() {
        Some(v) => Some(serde_json::from_value::<(i64, u64)>(v)?.0),
        None => None,
    };
    Ok(())
}
/// Consumes every pending copy whose mailbox time is over with an observable
/// `Record::Expired`, keyed by its position. Reads only the expired copies.
async fn expire(rows: &mut dyn Transaction, s: &mut StoredRun, now: i64) -> anyhow::Result<usize> {
    if s.mailbox.is_none_or(|at| at > now) {
        return Ok(0);
    }
    let id = s.run.admission.id.clone();
    let due = rows.scan(&expiring_of(&id), "", Some(&until(now)), usize::MAX).await?;
    for value in due.values() {
        let (_, n) = serde_json::from_value::<(i64, u64)>(value.clone())?;
        let event = copy(rows, &id, n).await?;
        let k = format!("mailbox#expired[{n}]");
        put(rows, &records_of(&id), &k, &Record::Expired { event: envelope(&event)? }).await?;
        consume(rows, s, n, event, &k).await?;
    }
    refresh_mailbox(rows, s).await?;
    Ok(due.len())
}
/// Whether a pending, unexpired copy is one the run waits or is subscribed
/// for. The counters answer unless a copy has expired unconsumed; then one
/// bounded read per waited or subscribed type with pending copies.
async fn pending_wanted(rows: &mut dyn Transaction, s: &StoredRun, now: i64) -> anyhow::Result<bool> {
    let id = &s.run.admission.id;
    let fresh = s.mailbox.is_none_or(|at| at > now);
    // Copies expiring after `now` (or never), at most `limit` of them.
    let until = until(now);
    for t in s.awaits.keys() {
        if s.pending.contains_key(t) && (fresh || !rows.scan(&live_of(id, t), &until, None, 1).await?.is_empty()) {
            return Ok(true);
        }
    }
    for t in s.subscriptions.values().flatten() {
        if t == "*"
            && s.published > 0
            && (fresh || !rows.scan(&live_published_of(id), &until, None, 1).await?.is_empty())
        {
            return Ok(true);
        }
        // A type keeps at most one pending lifecycle notification, so two
        // live copies include a CloudEvent if one is live.
        if s.pending.get(t).is_some_and(|p| p.cloud > 0)
            && (fresh || rows.scan(&live_of(id, t), &until, None, 2).await?.values().any(|cloud| cloud == &json!(true)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Accepts `event` into the run, unless the run already holds its identity
/// (pending, taken or expired): `false` for the same payload, an
/// `IdentityConflict` for another. Wakes a run that waits for it.
async fn accept(
    rows: &mut dyn Transaction,
    snapshot: &mut StoredRun,
    event: StoredEvent,
    now: i64,
) -> anyhow::Result<bool> {
    if let Some(id) = event.envelope.as_ref().and_then(identity) {
        let run = snapshot.run.admission.id.clone();
        if let Some(n) = get::<u64>(rows, &identities_of(&run), &component(&id)).await? {
            let held = copy(rows, &run, n).await?;
            if event.envelope.as_ref().map(payload) != held.envelope.as_ref().map(payload) {
                return Err(IdentityConflict(id.replace('\u{0}', "/")).into());
            }
            return Ok(false);
        }
    }
    if wanted(snapshot, &event, now) {
        snapshot.run.wake_at_ms = snapshot.run.wake_at_ms.min(now);
    }
    push(rows, snapshot, event).await?;
    Ok(true)
}
async fn deliver(
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
        Some(target) => find_run(rows, target).await?,
        // A redelivery goes back to the run that accepted it first; a new
        // event to the oldest eligible registration of its type.
        None => match indexed_runs(rows, &partition("delivered", &id), "", None, 1).await?.pop() {
            Some(first) => Some(first),
            None => indexed_runs(rows, &partition("wait", t), "", None, 1).await?.pop(),
        },
    };
    let Some(mut snapshot) = candidate else {
        return Ok(None);
    };
    let recipient = snapshot.run.admission.id.clone();
    if snapshot.run.ended_ms.is_some() && rows.get(&identities_of(&recipient), &component(&id)).await?.is_none() {
        return Ok(None);
    }
    if accept(rows, &mut snapshot, stored, now).await? {
        store_run(rows, &snapshot).await?;
    }
    Ok(Some(recipient))
}
/// A child's lifecycle notification to its parent, which registered for it
/// when it started the child: one pending copy per type, no identity.
async fn notify(rows: &mut dyn Transaction, parent: &str, t: &str, data: &Value, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, parent).await?;
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
    push(rows, &mut snapshot, event).await?;
    snapshot.run.wake_at_ms = now;
    store_run(rows, &snapshot).await
}
/// Reads only the runs indexed under the event's type and `*` (or the
/// target): bounded by the subscribers, never by the runs stored.
async fn publish(
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
        Some(id) => find_run(rows, id).await?.into_iter().collect(),
        None => {
            let mut runs = indexed_runs(rows, &partition("subs", t), "", None, usize::MAX).await?;
            for s in indexed_runs(rows, &partition("subs", "*"), "", None, usize::MAX).await? {
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
        if eligible && accept(rows, &mut snapshot, stored.clone(), now).await? {
            receivers.push(snapshot.run.admission.id.clone());
            store_run(rows, &snapshot).await?;
        }
    }
    Ok(receivers)
}
/// Removes a finished run's evidence: its records and copies with all their
/// rows. Reads only that run's rows; returns how many records it held.
async fn forget(rows: &mut dyn Transaction, s: &mut StoredRun) -> anyhow::Result<usize> {
    let id = s.run.admission.id.clone();
    let records = rows.scan(&records_of(&id), "", None, usize::MAX).await?;
    for (kind, k) in records.keys() {
        rows.delete(kind, k).await?;
    }
    for ((kind, k), value) in rows.scan(&events_of(&id), "", None, usize::MAX).await? {
        let event: StoredEvent = serde_json::from_value(value)?;
        let n = u64::from_str_radix(&k, 16)?;
        if event.consumed_key.is_none() {
            for (kind, key, _) in pending_rows(&id, n, &event) {
                rows.delete(&kind, &key).await?;
            }
        }
        if let Some(identity) = event.envelope.as_ref().and_then(identity) {
            rows.delete(&identities_of(&id), &component(&identity)).await?;
            if event.delivered {
                rows.delete(&partition("delivered", &identity), &id).await?;
            }
        }
        rows.delete(&kind, &k).await?;
    }
    for (kind, k) in rows.scan(&types_of(&id), "", None, usize::MAX).await?.into_keys() {
        rows.delete(&kind, &k).await?;
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
async fn queued(rows: &mut dyn Transaction, k: &str) -> anyhow::Result<Option<Queued>> {
    rows.get("trigger_queue", k).await?.map(decode_queue).transpose()
}
fn decode_queue(value: Value) -> anyhow::Result<Queued> {
    let q: Queued = serde_json::from_value(value)?;
    anyhow::ensure!(q.version == 1, "unsupported trigger queue layout");
    Ok(q)
}
async fn remove_queue(rows: &mut dyn Transaction, k: &str) -> anyhow::Result<()> {
    if let Some(q) = queued(rows, k).await? {
        rows.delete("trigger_queue", k).await?;
        rows.delete(&partition("queue_name", &q.admission.name), k).await?;
    }
    Ok(())
}
async fn capacity(rows: &mut dyn Transaction, name: &str, max: usize) -> anyhow::Result<bool> {
    Ok(max > 0 && rows.scan(&partition("live_name", name), "", None, max).await?.len() < max)
}
async fn admit_firing(
    rows: &mut dyn Transaction,
    k: &str,
    a: &Admission,
    rearm: Option<&Rearm>,
    next: Option<&Trigger>,
    now: i64,
) -> anyhow::Result<()> {
    if insert(rows, a, now).await? {
        let mut s = run(rows, &a.id).await?;
        s.trigger = Some(k.into());
        s.rearm = rearm.zip(next).map(|(r, t)| (r.clone(), t.clone()));
        store_run(rows, &s).await?;
    }
    put(rows, "trigger_run", k, &a.id).await
}
async fn drain_queues(rows: &mut dyn Transaction, name: &str, now: i64) -> anyhow::Result<()> {
    if is_stopped(rows).await? {
        if !rows.scan(&partition("queue_name", name), "", None, 1).await?.is_empty() {
            put(rows, "control_drain", name, &true).await?;
        }
        return Ok(());
    }
    for ((_, k), _) in rows.scan(&partition("queue_name", name), "", None, usize::MAX).await? {
        let Some(q) = queued(rows, &k).await? else {
            // A prior promotion in this batch may trip and disarm the
            // definition, removing the remaining buffered slots.
            anyhow::ensure!(rows.get(&partition("queue_name", name), &k).await?.is_none(), "dangling queue index");
            continue;
        };
        if !rows.scan(&partition("trigger_live", &k), "", None, 1).await?.is_empty()
            || !capacity(rows, name, q.max_concurrent_runs).await?
        {
            continue;
        }
        let t = get::<Trigger>(rows, "trigger", &k).await?.ok_or_else(|| anyhow::anyhow!("queue without trigger"))?;
        remove_queue(rows, &k).await?;
        if let Some((error, breaker_threshold)) = q.failure {
            refuse(rows, &Refused { admission: q.admission, error, breaker_threshold }, now).await?;
        } else {
            admit_firing(rows, &k, &q.admission, q.rearm.as_ref(), Some(&t), now).await?;
        }
    }
    Ok(())
}

/// Drains a definition's deferred queue before anything else is admitted to
/// it. A clear drains a bounded batch of markers, so a definition past the
/// batch keeps its marker; without this a later slot of the same trigger, or
/// another start that takes the room, would pass the buffered slot over.
/// `contract.qnt`'s `settle` promotes right after the clear, so a buffered slot
/// always precedes later ones; this is the same order, reached lazily and in
/// the admitting commit. One point read when nothing is deferred.
/// Whether it drained anything (`drained_refusal`).
async fn drain_marked(rows: &mut dyn Transaction, name: &str, now: i64) -> anyhow::Result<bool> {
    if get::<bool>(rows, "control_drain", name).await?.is_some() && !is_stopped(rows).await? {
        rows.delete("control_drain", name).await?;
        drain_queues(rows, name, now).await?;
        return Ok(true);
    }
    Ok(false)
}

async fn drain_pending(rows: &mut dyn Transaction, now: i64, limit: usize) -> anyhow::Result<()> {
    for ((_, name), _) in rows.scan("control_drain", "", None, limit).await? {
        rows.delete("control_drain", &name).await?;
        drain_queues(rows, &name, now).await?;
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
async fn store_trigger(
    rows: &mut dyn Transaction,
    key_: &str,
    before: Option<&Trigger>,
    after: Option<&Trigger>,
) -> anyhow::Result<()> {
    let old = before.map(|t| trigger_indexes(key_, t)).unwrap_or_default();
    let new = after.map(|t| trigger_indexes(key_, t)).unwrap_or_default();
    for ((kind, id), value) in crate::backend::changes(&old, &new) {
        if let Some(value) = value {
            rows.put(kind, id, value).await?;
        } else {
            rows.delete(kind, id).await?;
        }
    }
    match after {
        Some(t) => put(rows, "trigger", key_, t).await,
        None if before.is_some() => rows.delete("trigger", key_).await,
        None => Ok(()),
    }
}
async fn indexed_triggers(rows: &mut dyn Transaction, keys: Rows) -> anyhow::Result<Vec<(String, Trigger)>> {
    let mut out: Vec<(String, Trigger)> = Vec::new();
    for value in keys.into_values() {
        let k = value.as_str().ok_or_else(|| anyhow::anyhow!("invalid trigger index"))?.to_owned();
        if out.iter().any(|(o, _)| *o == k) {
            continue;
        }
        let t = get(rows, "trigger", &k).await?.ok_or_else(|| anyhow::anyhow!("dangling trigger index {k}"))?;
        out.push((k, t));
    }
    Ok(out)
}
/// Replaces the triggers a definition owns with `wanted`, in this commit.
async fn arm(
    rows: &mut dyn Transaction,
    defs: &mut Definitions,
    wanted: &BTreeMap<String, Trigger>,
) -> anyhow::Result<()> {
    for k in &defs.triggers {
        remove_queue(rows, k).await?;
    }
    for k in defs.triggers.iter().filter(|k| !wanted.contains_key(*k)) {
        let current: Option<Trigger> = get(rows, "trigger", k).await?;
        store_trigger(rows, k, current.as_ref(), None).await?;
    }
    for (k, t) in wanted {
        let current: Option<Trigger> = get(rows, "trigger", k).await?;
        store_trigger(rows, k, current.as_ref(), Some(t)).await?;
    }
    defs.triggers = wanted.keys().cloned().collect();
    Ok(())
}
/// `Some(true)`: a trigger already took this event (same payload);
/// `Some(false)`: its identity is free (absent or expired).
async fn tombstone(
    rows: &mut dyn Transaction,
    event: &Value,
    now: i64,
) -> anyhow::Result<(String, Option<Tombstone>, bool)> {
    let id = identity(event).ok_or_else(|| anyhow::anyhow!("an ingress event needs a source and an id"))?;
    let k = component(&id);
    let stored: Option<Tombstone> = get(rows, "ingress", &k).await?;
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
/// `Ok(Err(refusal))`: refused by a trip its own deferred drain committed
/// (`drained_refusal`); the drain is kept, nothing else is written.
async fn fire(
    rows: &mut dyn Transaction,
    f: &Firing<'_>,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<anyhow::Result<Fired>> {
    if is_stopped(rows).await? {
        return Ok(Ok(Fired::Changed));
    }
    anyhow::ensure!(f.refused.is_none() || f.admit.is_empty(), "a firing admits its runs or records a refusal");
    let mut drained = false;
    if let Some(a) = f.admit.first().or(f.refused.map(|r| &r.admission)) {
        drained = drain_marked(rows, &a.name, now).await?;
    }
    let current: Option<Trigger> = get(rows, "trigger", f.key).await?;
    if current.as_ref() != f.expected {
        // A trip disarms the definition's triggers: a drain that tripped
        // it lands here for any trigger it armed.
        return Ok(Ok(Fired::Changed));
    }
    for a in f.admit.iter().chain(f.refused.map(|r| &r.admission)) {
        if let Some(refusal) = drained_refusal(rows, drained, a).await? {
            return Ok(Err(refusal));
        }
    }
    fire_checked(rows, f, current, now, policy).await.map(Ok)
}
async fn fire_checked(
    rows: &mut dyn Transaction,
    f: &Firing<'_>,
    current: Option<Trigger>,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<Fired> {
    // Validate ingress before any policy outcome: a skipped or buffered event
    // is consumed too, and a conflicting payload must never cancel a run.
    if let Some(event) = f.ingress
        && tombstone(rows, event, now).await?.2
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
        if find_run(rows, &a.id).await?.is_none() {
            let limit = if guard.policy == Overlap::Replace { usize::MAX } else { 1 };
            let live = rows.scan(&partition("trigger_live", f.key), "", None, limit).await?;
            if guard.policy == Overlap::Replace {
                for id in live.values() {
                    let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid trigger run index"))?;
                    cancel_run(rows, id, now).await?;
                }
            }
            if !capacity(rows, &a.name, guard.max_concurrent_runs).await? {
                next = Some(guard.ceiling);
                disposition = Fired::Ceiling;
            } else if !live.is_empty() && guard.policy != Overlap::Allow && guard.policy != Overlap::Replace {
                if guard.policy == Overlap::Queue && queued(rows, f.key).await?.is_none() {
                    let q = Queued {
                        version: 1,
                        admission: a.clone(),
                        rearm: f.rearm.cloned(),
                        max_concurrent_runs: guard.max_concurrent_runs,
                        failure: f.refused.map(|r| (r.error.clone(), r.breaker_threshold)),
                    };
                    put(rows, "trigger_queue", f.key, &q).await?;
                    rows.put(&partition("queue_name", &a.name), f.key, &json!(f.key)).await?;
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
        && let Some(last) = get::<String>(rows, "trigger_run", f.key).await?
        && find_run(rows, &last).await?.is_some_and(|s| s.run.ended_ms.is_none())
    {
        next = Some(busy);
        disposition = Fired::Busy(last);
    }
    if let Some(event) = f.ingress {
        let (k, old, used) = tombstone(rows, event, now).await?;
        if used {
            return Ok(Fired::Ingested);
        }
        if let Some(old) = old {
            rows.delete("ingress_due", &timed(old.expires_at_ms, &k)).await?;
        }
        let expires_at_ms = now.saturating_add(policy.ingress_ms);
        put(rows, "ingress", &k, &Tombstone { payload: payload(event), expires_at_ms }).await?;
        rows.put("ingress_due", &timed(expires_at_ms, &k), &json!(k)).await?;
    }
    store_trigger(rows, f.key, current.as_ref(), next).await?;
    if disposition == Fired::Committed {
        for a in f.admit {
            admit_firing(rows, f.key, a, f.rearm, next, now).await?;
        }
    }
    if disposition == Fired::Committed
        && let Some(r) = f.refused
    {
        refuse(rows, r, now).await?;
        put(rows, "trigger_run", f.key, &r.admission.id).await?;
    }
    Ok(disposition)
}

async fn activate(
    rows: &mut dyn Transaction,
    name: &str,
    version: u64,
    epoch: Option<u64>,
    arming: &Arming,
    activation: &Activation,
) -> anyhow::Result<Option<Definitions>> {
    let mut defs = load_definitions(rows, name).await?.unwrap_or_default();
    anyhow::ensure!(defs.versions.contains_key(&version), "unknown definition version");
    if epoch.is_some_and(|e| e != defs.epoch) {
        return Ok(None);
    }
    // Only activation starts an epoch and clears an automatic trip. Manual
    // pauses can resume in place; notices stay until the host acknowledges them.
    defs.active = Some(version);
    defs.epoch = defs.epoch.checked_add(1).ok_or_else(|| anyhow::anyhow!("activation epoch overflow"))?;
    defs.consecutive_faults = 0;
    defs.tripped = None;
    let Activation { at_ms, actor, note } = activation.clone();
    defs.activations.insert(version, Activated { epoch: defs.epoch, at_ms, actor, note });
    rows.delete("paused", name).await?;
    arm(rows, &mut defs, &arming.triggers).await?;
    store_definitions(rows, name, &defs).await?;
    Ok(Some(defs))
}

// ── 5. termination and the breaker ─────────────────────────────────────────

async fn lifecycle(rows: &mut dyn Transaction, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
    let Some(link) = run(rows, id).await?.run.child else {
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
    if kind == "done" && rows.get(&types_of(&link.parent), &component(&t)).await?.is_some() {
        return Ok(());
    }
    if kind == "done" {
        // A terminal child can never report waiting again. Close the automatic
        // registration, while retaining any explicit listen for that type.
        let mut parent = run(rows, &link.parent).await?;
        let waiting = format!("waiting:{id}");
        let had = parent.awaits.contains_key(&waiting);
        close(&mut parent, std::slice::from_ref(&waiting));
        if had && !parent.awaits.contains_key(&waiting) {
            store_run(rows, &parent).await?;
        }
    }
    notify(rows, &link.parent, &t, &data, now).await
}
async fn terminate(rows: &mut dyn Transaction, id: &str, status: &Status, now: i64) -> anyhow::Result<()> {
    let mut snapshot = run(rows, id).await?;
    let root = snapshot.run.root.as_deref().unwrap_or(id);
    let spent: u64 = get(rows, "budget", root).await?.unwrap_or(0);
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
    snapshot.run.held = None;
    snapshot.awaits.clear();
    snapshot.addressed_waits.clear();
    snapshot.listeners.clear();
    snapshot.subscriptions.clear();
    snapshot.waits.clear();
    store_run(rows, &snapshot).await?;
    // The tree ends with its root (its owned descendants are cancelled
    // below, in this commit): nothing can spend its budget any more.
    if snapshot.run.root.is_none() && rows.get("budget", id).await?.is_some() {
        rows.delete("budget", id).await?;
    }
    // Only the trigger the firing stored is re-armed: a later activation
    // (a new epoch) replaced it, and an old run cannot move the new one.
    if let Some((rearm, armed)) = &snapshot.rearm
        && let Some(t) = get::<Trigger>(rows, "trigger", &rearm.trigger).await?
        && t == *armed
        && t.at_ms.is_none()
    {
        let due = Trigger { at_ms: Some(now.saturating_add(rearm.delay_ms)), ..t.clone() };
        store_trigger(rows, &rearm.trigger, Some(&t), Some(&due)).await?;
    }
    // Iterative cascade avoids stack growth on arbitrarily nested child runs.
    let mut names = std::collections::BTreeSet::from([snapshot.run.admission.name.clone()]);
    let mut ended = BTreeMap::new();
    tally(&mut ended, &snapshot.run);
    let mut parents = vec![id.to_string()];
    while let Some(parent) = parents.pop() {
        for mut child in indexed_runs(rows, &partition("children", &parent), "", None, usize::MAX).await? {
            // The indexed read already holds this child's primary row in
            // the transaction; reuse its indexes instead of reading it twice.
            let before = indexes(&child);
            child.budget_ended = Some(spent);
            child.run.state = RunState::Cancelled;
            child.run.outcome = Some(Status::Cancelled);
            child.run.ended_ms = Some(now);
            child.run.fence = None;
            child.run.lease_until_ms = None;
            child.run.held = None;
            child.awaits.clear();
            child.addressed_waits.clear();
            child.listeners.clear();
            child.subscriptions.clear();
            names.insert(child.run.admission.name.clone());
            tally(&mut ended, &child.run);
            child.waits.clear();
            parents.push(child.run.admission.id.clone());
            store_run_indexed(rows, &child, &before).await?;
            lifecycle(rows, &child.run.admission.id, &Status::Cancelled, now).await?;
        }
    }
    lifecycle(rows, id, status, now).await?;
    for name in names {
        drain_queues(rows, &name, now).await?;
    }
    for (name, (runs, annotations)) in ended {
        add_bucket(rows, &name, now, runs, &annotations).await?;
    }
    Ok(())
}
/// Adds the totals of runs that ended now to `name`'s bucket: one row read
/// and written, in the terminal commit. Runs ending against the same
/// definition within the hour share it, as they share its breaker row; a
/// run without annotation totals writes nothing.
async fn add_bucket(
    rows: &mut dyn Transaction,
    name: &str,
    now: i64,
    runs: u64,
    annotations: &ows::durable::Totals,
) -> anyhow::Result<()> {
    let (kind, k) = (partition("totals", name), time_key(bucket_start(now)));
    let mut bucket: Bucket = get(rows, &kind, &k).await?.unwrap_or_default();
    bucket.runs = bucket.runs.saturating_add(runs);
    ows::durable::accumulate(&mut bucket.annotations, annotations);
    put(rows, &kind, &k, &bucket).await
}
/// Notes an ending run's annotation totals under its definition.
fn tally(ended: &mut BTreeMap<String, (u64, ows::durable::Totals)>, run: &SavedRun) {
    if run.annotations.is_empty() {
        return;
    }
    let (runs, totals) = ended.entry(run.admission.name.clone()).or_default();
    *runs += 1;
    ows::durable::accumulate(totals, &run.annotations);
}
async fn cancel_run(rows: &mut dyn Transaction, id: &str, now: i64) -> anyhow::Result<bool> {
    let Some(s) = find_run(rows, id).await? else { return Ok(false) };
    if s.run.ended_ms.is_some() {
        return Ok(false);
    }
    terminate(rows, id, &Status::Cancelled, now).await?;
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
async fn breaker(
    rows: &mut dyn Transaction,
    run: &SavedRun,
    fault: Option<&WorkflowError>,
    success: bool,
    threshold: u32,
    now: i64,
) -> anyhow::Result<()> {
    let name = &run.admission.name;
    let Some(mut defs) = load_definitions(rows, name).await? else {
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
                    breaker: true,
                    version: run.admission.version,
                    digest: defs.versions.get(&run.admission.version).map(|d| d.digest.clone()).unwrap_or_default(),
                    epoch: defs.epoch,
                    faults: defs.consecutive_faults,
                    threshold,
                    run: run.admission.id.clone(),
                    reason: reason.clone(),
                    at_ms: now,
                };
                arm(rows, &mut defs, &BTreeMap::new()).await?;
                rows.put("notice", &notice_key(name, &trip), &trip.event(name)).await?;
                defs.tripped = Some(trip);
            }
        }
        None => return Ok(()),
    }
    store_definitions(rows, name, &defs).await
}

// ── 6. the RunStore implementation ─────────────────────────────────────────

#[async_trait::async_trait]
impl RunStore for TransactionalStore {
    async fn apply_control(
        &self,
        request: &control::Request,
        command: &control::Command,
        now: i64,
    ) -> anyhow::Result<control::Applied> {
        self.update(|rows| Box::pin(async move { control_command(rows, request, command, now, &self.policy).await }))
            .await
    }
    async fn stopped(&self) -> anyhow::Result<bool> {
        self.read(|rows| Box::pin(async move { is_stopped(rows).await })).await
    }
    async fn control_entry(&self, id: &str) -> anyhow::Result<Option<control::Entry>> {
        self.read(|rows| Box::pin(async move { get(rows, "control_command", id).await })).await
    }
    async fn control_history(
        &self,
        run: Option<&str>,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Entry>> {
        self.read(|rows| {
            Box::pin(async move {
                let kind = run.map_or_else(|| "control_history".into(), |r| partition("control_history_run", r));
                let (found, next) = page_rows(rows, &kind, &kind, None, None, cursor, limit).await?;
                let items =
                    found.into_values().map(|v| Ok(serde_json::from_value(v)?)).collect::<anyhow::Result<_>>()?;
                Ok(control::Page { items, next })
            })
        })
        .await
    }
    async fn query_runs(
        &self,
        filter: &control::Runs,
        history: bool,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<SavedRun>> {
        self.read(|rows| Box::pin(async move { query_runs(rows, filter, history, cursor, limit).await })).await
    }
    async fn query_definitions(
        &self,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::DefinitionSummary>> {
        self.read(|rows| {
            Box::pin(async move {
                let (found, next) = page_rows(rows, "definitions", "definitions", None, None, cursor, limit).await?;
                let mut items = Vec::new();
                for ((_, name), value) in found {
                    let defs = decode_definitions(&name, value)?;
                    items.push(definition_summary(rows, &name, defs).await?);
                }
                Ok(control::Page { items, next })
            })
        })
        .await
    }
    async fn definition_control(&self, name: &str) -> anyhow::Result<Option<control::DefinitionSummary>> {
        self.read(|rows| {
            Box::pin(async move {
                let Some(defs) = load_definitions(rows, name).await? else { return Ok(None) };
                Ok(Some(definition_summary(rows, name, defs).await?))
            })
        })
        .await
    }
    async fn pending_waits(
        &self,
        f: &control::Waits,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Wait>> {
        self.read(|rows| {
            Box::pin(async move {
                let query = serde_json::to_string(&("waits", f))?;
                let (found, next) =
                    page_rows(rows, &wait_kind(f), &query, f.since_ms, f.before_ms, cursor, limit).await?;
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
        })
        .await
    }
    // One point read of the worker's row; the row and its expiry entry are
    // rewritten, whatever else the store holds.
    async fn beat(&self, worker: &control::Worker) -> anyhow::Result<()> {
        anyhow::ensure!(!worker.id.is_empty(), "a worker needs an id");
        self.update(|rows| {
            Box::pin(async move {
                let old: Option<control::Worker> = get(rows, "worker", &worker.id).await?;
                if let Some(old) = &old {
                    if old.beat_ms > worker.beat_ms {
                        return Ok(());
                    }
                    if old.expires_ms != worker.expires_ms {
                        rows.delete("worker_expiry", &timed(old.expires_ms, &component(&old.id))).await?;
                    }
                }
                rows.put("worker_expiry", &timed(worker.expires_ms, &component(&worker.id)), &json!(worker.id)).await?;
                put(rows, "worker", &worker.id, worker).await
            })
        })
        .await
    }
    async fn workers(
        &self,
        cursor: Option<&control::Cursor>,
        limit: usize,
    ) -> anyhow::Result<control::Page<control::Worker>> {
        self.read(|rows| {
            Box::pin(async move {
                let (found, next) = page_rows(rows, "worker", "workers", None, None, cursor, limit).await?;
                let items =
                    found.into_values().map(|v| Ok(serde_json::from_value(v)?)).collect::<anyhow::Result<_>>()?;
                Ok(control::Page { items, next })
            })
        })
        .await
    }
    async fn describe_wait(&self, f: &Fence, k: &str, wait: &ows::events::PendingWait, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                let mut wait = wait.clone();
                if let Some(old) = s.waits.get(k) {
                    wait.since_ms = old.since_ms;
                }
                if s.waits.get(k) != Some(&wait) {
                    s.waits.insert(k.into(), wait);
                    store_run(rows, &s).await?;
                }
                Ok(())
            })
        })
        .await
    }
    async fn inspect_control(&self, id: &str) -> anyhow::Result<Option<control::Inspection>> {
        self.read(|rows| {
            Box::pin(async move {
                let Some(s) = find_run(rows, id).await? else {
                    return Ok(None);
                };
                let waits = s.waits.clone();
                let root = s.run.root.as_deref().unwrap_or(id);
                let budget_spent = match s.budget_ended {
                    Some(spent) => spent,
                    None => get::<u64>(rows, "budget", root).await?.unwrap_or(0),
                };
                let children = indexed_runs(rows, &partition("control_parent", id), "", None, usize::MAX)
                    .await?
                    .into_iter()
                    .map(|s| s.run)
                    .collect();
                let runtime = &s.run.admission.runtime;
                let budget_limit = runtime["limits"]["max_jumps"].as_u64().map(|host| {
                    ows::model::parse_yaml(&s.run.admission.definition).map_or(host, |w| w.budget.max_jumps(host))
                });
                let pinned = s.run.admission.pinned();
                let digest = s.digest.clone();
                let trace_id = runtime["trace_id"].as_str().map(str::to_owned);
                let span_id = runtime["span_id"].as_str().map(str::to_owned);
                let snapshot = snapshot(rows, s).await?;
                let calls = ows::durable::call_journal(&snapshot.records);
                let annotations = snapshot.run.annotations.clone();
                Ok(Some(control::Inspection {
                    snapshot,
                    pinned,
                    digest,
                    calls,
                    annotations,
                    waits,
                    children,
                    budget_spent,
                    budget_limit,
                    trace_id,
                    span_id,
                }))
            })
        })
        .await
    }

    // One bounded range read of the definition's bucket rows.
    async fn totals(&self, name: &str, since: i64, before: i64) -> anyhow::Result<control::DefinitionTotals> {
        use control::{ControlError, MAX_TOTALS_BUCKETS, TOTALS_BUCKET_MS};
        if since > before {
            return Err(ControlError::InvalidInput("invalid time window".into()).into());
        }
        let start = bucket_start(since);
        let end = bucket_start(before.saturating_sub(1)).saturating_add(TOTALS_BUCKET_MS).max(start);
        let buckets = usize::try_from(end.saturating_sub(start) / TOTALS_BUCKET_MS).unwrap_or(usize::MAX);
        if buckets > MAX_TOTALS_BUCKETS {
            return Err(ControlError::InvalidInput(format!(
                "a totals window covers at most {MAX_TOTALS_BUCKETS} buckets of {TOTALS_BUCKET_MS} ms"
            ))
            .into());
        }
        self.read(|rows| {
            Box::pin(async move {
                let found = rows
                    .scan(&partition("totals", name), &time_key(start), Some(&time_key(end)), MAX_TOTALS_BUCKETS)
                    .await?;
                let mut totals = control::DefinitionTotals {
                    definition: name.into(),
                    since_ms: start,
                    before_ms: end,
                    runs: 0,
                    annotations: Default::default(),
                    buckets: Vec::new(),
                };
                for ((_, k), value) in found {
                    let bucket: Bucket = serde_json::from_value(value)?;
                    let start_ms = i64::from_be_bytes(u64::from_str_radix(&k, 16)?.to_be_bytes()) ^ i64::MIN;
                    totals.runs = totals.runs.saturating_add(bucket.runs);
                    ows::durable::accumulate(&mut totals.annotations, &bucket.annotations);
                    totals.buckets.push(control::TotalsBucket {
                        start_ms,
                        runs: bucket.runs,
                        annotations: bucket.annotations,
                    });
                }
                Ok(totals)
            })
        })
        .await
    }

    fn policy(&self) -> Policy {
        self.policy
    }
    async fn admit(&self, a: &Admission, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| {
            Box::pin(async move {
                let drained = drain_marked(rows, &a.name, now).await?;
                if let Some(refusal) = drained_refusal(rows, drained, a).await? {
                    return Ok(Err(refusal));
                }
                insert(rows, a, now).await.map(Ok)
            })
        })
        .await?
    }
    async fn start(&self, a: &Admission, now: i64) -> anyhow::Result<Option<Fence>> {
        let owner = &uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            Box::pin(async move {
                let drained = drain_marked(rows, &a.name, now).await?;
                if let Some(refusal) = drained_refusal(rows, drained, a).await? {
                    return Ok(Err(refusal));
                }
                let Some(mut s) = admitted(rows, a, now).await? else {
                    return Ok(Ok(None));
                };
                let fence = claim_run(&mut s, now, owner)?;
                store_run(rows, &s).await?;
                Ok(Ok(Some(fence)))
            })
        })
        .await?
    }
    async fn claim_as(&self, id: &str, now: i64, worker: &Compatibility) -> anyhow::Result<Option<Fence>> {
        let owner = &uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            Box::pin(async move {
                if is_stopped(rows).await? {
                    return Ok(None);
                }
                let Some(mut s) = find_run(rows, id).await? else {
                    return Ok(None);
                };
                if !claimable(&s.run, now) || !worker.admits(s.run.admission.application_version()) {
                    return Ok(None);
                }
                let fence = claim_run(&mut s, now, owner)?;
                store_run(rows, &s).await?;
                Ok(Some(fence))
            })
        })
        .await
    }
    async fn check_lease(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| Box::pin(async move { checked(rows, f, now, self.policy.skew_ms).await.map(|_| ()) })).await
    }
    async fn renew(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                s.run.lease_until_ms = Some(now.saturating_add(LEASE_MS));
                store_run(rows, &s).await
            })
        })
        .await
    }
    async fn release(&self, f: &Fence, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = authority(rows, f, now, self.policy.skew_ms).await?;
                s.run.state = RunState::Ready;
                s.run.fence = None;
                s.run.lease_until_ms = None;
                s.run.wake_at_ms = now;
                store_run(rows, &s).await
            })
        })
        .await
    }
    // One ordered range read of the run's own record rows.
    async fn load_records(&self, f: &Fence, now: i64) -> anyhow::Result<BTreeMap<String, Record>> {
        self.update(|rows| {
            Box::pin(async move {
                checked(rows, f, now, self.policy.skew_ms).await?;
                rows.scan(&records_of(&f.run_id), "", None, usize::MAX)
                    .await?
                    .into_iter()
                    .map(|((_, key), value)| Ok((key, serde_json::from_value(value)?)))
                    .collect()
            })
        })
        .await
    }

    // The fence check reads the run row and the save writes only the record's
    // row, in one serializable commit: an owner replaced before the commit
    // conflicts and fails the check on retry. A run's first record also
    // writes the run row, to mark evidence for `sweep`, and so does a call's
    // answer with numeric annotations, which adds them to the run's totals:
    // only the save that completes the call does, never a replay of it (a
    // completed record is immutable, and saving it again changes nothing).
    async fn save_record(&self, f: &Fence, k: &str, r: &Record, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                running(rows).await?;
                let mut s = authority(rows, f, now, self.policy.skew_ms).await?;
                let kind = records_of(&f.run_id);
                let old = rows.get(&kind, k).await?;
                // A hold refuses every step but the answer of a call already
                // dispatched: its intent is saved and the world may have
                // changed, so the answer is recorded, never lost.
                if s.run.held.is_some() {
                    let intent = old.as_ref().map(|o| serde_json::from_value::<Record>(o.clone())).transpose()?;
                    let answer = matches!(
                        (&intent, r),
                        (
                            Some(Record::Call { state: CallState::Pending, .. }),
                            Record::Call { state: CallState::Done(_) | CallState::NotExecuted(_), .. }
                        )
                    );
                    if !answer {
                        not_held(&s)?;
                    }
                }
                let value = serde_json::to_value(r)?;
                if matches!(r, Record::Event { .. } | Record::Expired { .. }) {
                    anyhow::ensure!(old.as_ref() == Some(&value), "event records must be committed by take_event");
                }
                if matches!(r, Record::Budget { .. } | Record::Exceeded { .. }) {
                    anyhow::ensure!(old.as_ref() == Some(&value), "budget records must be committed by spend");
                }
                let mut completed = false;
                if let Some(old) = &old {
                    let old: Record = serde_json::from_value(old.clone())?;
                    check_transition(&old, r)?;
                    completed = matches!(old, Record::Call { state: CallState::Done(_), .. });
                }
                if old.as_ref() != Some(&value) {
                    rows.put(&kind, k, &value).await?;
                }
                let mut changed = !s.evidence;
                s.evidence = true;
                if let Record::Call { state: CallState::Done(_), annotations, .. } = r
                    && !completed
                {
                    let before = s.run.annotations.clone();
                    ows::durable::accumulate(&mut s.run.annotations, ows::durable::numbers(annotations));
                    changed |= s.run.annotations != before;
                }
                if changed {
                    store_run(rows, &s).await?;
                }
                Ok(())
            })
        })
        .await
    }
    async fn register_wait(&self, f: &Fence, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                for t in types {
                    s.addressed_waits.insert(t.clone());
                    s.awaits.entry(t.clone()).or_insert(now);
                }
                store_run(rows, &s).await
            })
        })
        .await
    }
    async fn unregister_wait(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                s.listeners.remove(k);
                close(&mut s, types);
                store_run(rows, &s).await
            })
        })
        .await
    }
    async fn deliver(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Option<String>> {
        self.update(|rows| Box::pin(async move { deliver(rows, event, target, now, &self.policy).await })).await
    }
    async fn take_event(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<Option<Value>> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                let id = f.run_id.as_str();
                if let Some(saved) = rows.get(&records_of(id), k).await? {
                    let Record::Event { event } = serde_json::from_value(saved)? else {
                        anyhow::bail!("invalid event checkpoint")
                    };
                    return Ok(Some(event));
                }
                // A copy past its mailbox time is consumed by its expiry record,
                // never by this listen.
                expire(rows, &mut s, now).await?;
                // The first pending copy, in arrival order, of a listed type or,
                // for `*`, published: one indexed row per listed type.
                let mut kinds: Vec<String> = types.iter().map(|t| pending_of(id, t)).collect();
                if types.iter().any(|t| t == "*") {
                    kinds.push(published_of(id));
                }
                let mut candidate: Option<u64> = None;
                for kind in kinds {
                    if let Some(n) = rows.scan(&kind, "", None, 1).await?.into_values().next() {
                        let n = n.as_u64().ok_or_else(|| anyhow::anyhow!("invalid pending event index"))?;
                        candidate = Some(candidate.map_or(n, |c| c.min(n)));
                    }
                }
                let result = if let Some(n) = candidate {
                    let stored = copy(rows, id, n).await?;
                    let event = envelope(&stored)?;
                    if let Some(child) = stored.type_.strip_prefix("done:") {
                        s.listeners.remove(&format!("child:{child}"));
                    }
                    let mailbox = stored.expires_at_ms.is_some();
                    consume(rows, &mut s, n, stored, k).await?;
                    if mailbox {
                        refresh_mailbox(rows, &mut s).await?;
                    }
                    s.listeners.remove(k);
                    s.waits.retain(|_, w| w.position != k);
                    close(&mut s, types);
                    put(rows, &records_of(id), k, &Record::Event { event: event.clone() }).await?;
                    Some(event)
                } else {
                    s.listeners.insert(k.into(), types.to_vec());
                    None
                };
                store_run(rows, &s).await?;
                Ok(result)
            })
        })
        .await
    }
    async fn subscribe(&self, f: &Fence, k: &str, types: &[String], now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                s.subscriptions.insert(k.into(), types.to_vec());
                store_run(rows, &s).await
            })
        })
        .await
    }
    async fn unsubscribe(&self, f: &Fence, k: &str, now: i64) -> anyhow::Result<()> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                s.subscriptions.remove(k);
                s.waits.remove(k);
                store_run(rows, &s).await
            })
        })
        .await
    }
    async fn publish(&self, event: &Value, target: Option<&str>, now: i64) -> anyhow::Result<Vec<String>> {
        self.update(|rows| Box::pin(async move { publish(rows, event, target, now, &self.policy).await })).await
    }
    async fn ingested(&self, event: &Value, now: i64) -> anyhow::Result<bool> {
        self.read(|rows| Box::pin(async move { Ok(tombstone(rows, event, now).await?.2) })).await
    }
    async fn trigger(&self, key: &str) -> anyhow::Result<Option<Trigger>> {
        self.read(|rows| Box::pin(async move { get(rows, "trigger", key).await })).await
    }
    async fn due_triggers(&self, now: i64, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.read(|rows| {
            Box::pin(async move {
                if is_stopped(rows).await? {
                    return Ok(Vec::new());
                }
                let keys = rows.scan("trigger_due", "", Some(&until(now)), limit).await?;
                indexed_triggers(rows, keys).await
            })
        })
        .await
    }
    async fn triggers_for(&self, t: &str, limit: usize) -> anyhow::Result<Vec<(String, Trigger)>> {
        self.read(|rows| {
            Box::pin(async move {
                if is_stopped(rows).await? {
                    return Ok(Vec::new());
                }
                let mut keys = rows.scan(&partition("trigger_type", t), "", None, limit).await?;
                if t != "*" && keys.len() < limit {
                    keys.extend(rows.scan(&partition("trigger_type", "*"), "", None, limit - keys.len()).await?);
                }
                indexed_triggers(rows, keys).await
            })
        })
        .await
    }
    async fn queued(&self, key: &str) -> anyhow::Result<Option<Admission>> {
        self.read(|rows| Box::pin(async move { Ok(queued(rows, key).await?.map(|q| q.admission)) })).await
    }
    async fn fire(&self, firing: &Firing<'_>, now: i64) -> anyhow::Result<Fired> {
        self.update(|rows| Box::pin(async move { fire(rows, firing, now, &self.policy).await })).await?
    }
    async fn finish(&self, f: &Fence, outcome: &Status, now: i64, threshold: u32) -> anyhow::Result<Status> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                let mut status = outcome.clone();
                match &status {
                    Status::Waiting { wake_at_ms } => {
                        s.run.state = RunState::Waiting;
                        s.run.interruptions = 0;
                        s.run.wake_at_ms = *wake_at_ms;
                        if pending_wanted(rows, &s, now).await? {
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
                store_run(rows, &s).await?;
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
                    breaker(rows, &s.run, fault, success, threshold, now).await?;
                    terminate(rows, &f.run_id, &status, now).await?;
                } else {
                    lifecycle(rows, &f.run_id, &status, now).await?;
                }
                Ok(status)
            })
        })
        .await
    }
    async fn start_child(
        &self,
        parent: &Fence,
        a: &Admission,
        link: &ChildLink,
        now: i64,
    ) -> anyhow::Result<Option<Fence>> {
        let owner = &uuid::Uuid::new_v4().to_string();
        self.update(|rows| {
            Box::pin(async move {
                let mut p = checked(rows, parent, now, self.policy.skew_ms).await?;
                anyhow::ensure!(link.parent == parent.run_id && a.id != parent.run_id, "invalid child parent");
                // An owned child takes its definition's room like any start:
                // the deferred buffered slot goes first.
                let drained = drain_marked(rows, &a.name, now).await?;
                if let Some(refusal) = drained_refusal(rows, drained, a).await? {
                    return Ok(Err(refusal));
                }
                for t in [format!("done:{}", a.id), format!("waiting:{}", a.id)] {
                    p.awaits.entry(t).or_insert(now);
                }
                p.listeners.insert(format!("child:{}", a.id), vec![format!("done:{}", a.id)]);
                store_run(rows, &p).await?;
                let fresh = admitted(rows, a, now).await?;
                let inserted = fresh.is_some();
                let mut c = match fresh {
                    Some(c) => c,
                    None => run(rows, &a.id).await?,
                };
                if inserted {
                    c.run.child = Some(link.clone());
                    c.run.root = Some(p.run.root.clone().unwrap_or_else(|| parent.run_id.clone()));
                }
                anyhow::ensure!(c.run.child.as_ref() == Some(link), "child belongs to another invocation");
                let fence = if inserted { Some(claim_run(&mut c, now, owner)?) } else { None };
                store_run(rows, &c).await?;
                if let Some(outcome) = c.run.outcome {
                    lifecycle(rows, &a.id, &outcome, now).await?;
                } else if c.run.state == RunState::Waiting {
                    lifecycle(rows, &a.id, &Status::Waiting { wake_at_ms: 0 }, now).await?;
                }
                Ok(Ok(fence))
            })
        })
        .await?
    }
    async fn child_outcome(&self, parent: &str, k: &str, child: &str) -> anyhow::Result<Option<Status>> {
        self.read(|rows| {
            Box::pin(async move {
                let s = run(rows, child).await?;
                anyhow::ensure!(
                    s.run.child.as_ref().is_some_and(|l| l.parent == parent && l.key == k),
                    "child belongs to another parent or key"
                );
                Ok(s.run.outcome)
            })
        })
        .await
    }
    // The fence check reads the run row, the decision is the record's row and
    // the count the tree's `budget` row: one commit, three rows (the run row
    // again only for a run's first evidence).
    async fn spend(&self, f: &Fence, k: &str, limit: u64, now: i64) -> anyhow::Result<Record> {
        self.update(|rows| {
            Box::pin(async move {
                let mut s = checked(rows, f, now, self.policy.skew_ms).await?;
                let kind = records_of(&f.run_id);
                if let Some(saved) = rows.get(&kind, k).await? {
                    let record: Record = serde_json::from_value(saved)?;
                    anyhow::ensure!(matches!(record, Record::Budget { .. }), "invalid budget record at {k}");
                    return Ok(record);
                }
                let root = s.run.root.clone().unwrap_or_else(|| f.run_id.clone());
                let spent: u64 = get(rows, "budget", &root).await?.unwrap_or(0);
                let granted = spent < limit;
                let spent = spent + u64::from(granted);
                if granted {
                    put(rows, "budget", &root, &spent).await?;
                }
                // A refusal names the run's first one, decided in the commit
                // that refused it.
                let first = if granted {
                    None
                } else {
                    match rows.get(&kind, EXCEEDED).await? {
                        Some(saved) => match serde_json::from_value(saved)? {
                            Record::Exceeded { by } => Some(by),
                            _ => anyhow::bail!("invalid first refusal record"),
                        },
                        None => {
                            rows.put(&kind, EXCEEDED, &serde_json::to_value(Record::Exceeded { by: k.into() })?)
                                .await?;
                            Some(k.to_string())
                        }
                    }
                };
                let record = Record::Budget { granted, spent, first };
                rows.put(&kind, k, &serde_json::to_value(&record)?).await?;
                if !s.evidence {
                    s.evidence = true;
                    store_run(rows, &s).await?;
                }
                Ok(record)
            })
        })
        .await
    }
    async fn due_as(&self, now: i64, limit: usize, worker: &Compatibility) -> anyhow::Result<Vec<String>> {
        self.update(|rows| {
            Box::pin(async move {
                if is_stopped(rows).await? {
                    return Ok(Vec::new());
                }
                drain_pending(rows, now, limit).await?;
                let Some(own) = &worker.version else {
                    return rows
                        .scan("due", "", Some(&until(now)), limit)
                        .await?
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
                    for ((_, k), id) in rows.scan(&partition("due_app", v), "", Some(&until(now)), limit).await? {
                        let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid due index"))?.to_owned();
                        due.push((k, id));
                    }
                }
                due.sort();
                Ok(due.into_iter().take(limit).map(|(_, id)| id).collect())
            })
        })
        .await
    }

    async fn cancel(&self, id: &str, now: i64) -> anyhow::Result<bool> {
        self.update(|rows| Box::pin(async move { cancel_run(rows, id, now).await })).await
    }

    async fn sweep(&self, now: i64, kept: i64) -> anyhow::Result<Swept> {
        anyhow::ensure!(kept >= 0, "negative retention");
        self.update(|rows| {
            Box::pin(async move {
                let mut swept = Swept::default();
                let horizon = now.saturating_sub(kept);
                for mut s in indexed_runs(rows, "finished", "", Some(&until(horizon)), usize::MAX).await? {
                    if s.run.ended_ms.is_some_and(|at| at <= horizon) {
                        swept.records += forget(rows, &mut s).await?;
                        store_run(rows, &s).await?;
                    }
                }
                // Unfinished runs whose mailbox holds a copy past its time.
                for mut s in indexed_runs(rows, "mailbox", "", Some(&until(now)), usize::MAX).await? {
                    swept.expired += expire(rows, &mut s, now).await?;
                    store_run(rows, &s).await?;
                }
                for ((_, k), id) in rows.scan("ingress_due", "", Some(&until(now)), usize::MAX).await? {
                    let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid ingress index"))?.to_owned();
                    rows.delete("ingress_due", &k).await?;
                    rows.delete("ingress", &id).await?;
                    swept.tombstones += 1;
                }
                // Workers stale for long enough: a range of the expiry index.
                let stale = until(now.saturating_sub(control::WORKERS_KEPT_MS));
                for ((_, k), id) in rows.scan("worker_expiry", "", Some(&stale), usize::MAX).await? {
                    let id = id.as_str().ok_or_else(|| anyhow::anyhow!("invalid worker index"))?.to_owned();
                    rows.delete("worker_expiry", &k).await?;
                    rows.delete("worker", &id).await?;
                    swept.workers += 1;
                }
                Ok(swept)
            })
        })
        .await
    }
    async fn get(&self, id: &str) -> anyhow::Result<Option<SavedRun>> {
        self.read(|rows| Box::pin(async move { Ok(find_run(rows, id).await?.map(|s| s.run)) })).await
    }
    async fn list(&self, name: Option<&str>, prefix: &str) -> anyhow::Result<Vec<SavedRun>> {
        self.read(|rows| {
            Box::pin(async move {
                let end = prefix_end(prefix);
                if let Some(name) = name {
                    Ok(indexed_runs(rows, &partition("name", name), prefix, end.as_deref(), usize::MAX)
                        .await?
                        .into_iter()
                        .map(|s| s.run)
                        .collect())
                } else {
                    rows.scan("run", prefix, end.as_deref(), usize::MAX)
                        .await?
                        .into_values()
                        .map(|v| Ok(stored_run(v)?.run))
                        .collect()
                }
            })
        })
        .await
    }
    async fn inspect(&self, id: &str) -> anyhow::Result<Option<RunSnapshot>> {
        self.read(|rows| {
            Box::pin(async move {
                let Some(s) = find_run(rows, id).await? else {
                    return Ok(None);
                };
                Ok(Some(snapshot(rows, s).await?))
            })
        })
        .await
    }

    async fn save_definition(&self, name: &str, yaml: &str) -> anyhow::Result<Definition> {
        self.update(|rows| {
            Box::pin(async move {
                let mut defs = load_definitions(rows, name).await?.unwrap_or_default();
                let version = defs
                    .versions
                    .last_key_value()
                    .map_or(Some(1), |(v, _)| v.checked_add(1))
                    .ok_or_else(|| anyhow::anyhow!("definition version overflow"))?;
                let d = Definition { name: name.into(), version, yaml: yaml.into(), digest: digest_of(yaml) };
                defs.versions.insert(version, d.clone());
                store_definitions(rows, name, &defs).await?;
                Ok(d)
            })
        })
        .await
    }
    async fn activate(&self, name: &str, version: u64, now: i64) -> anyhow::Result<()> {
        let activation = &Activation::at(now);
        self.update(|rows| {
            Box::pin(async move { activate(rows, name, version, None, &Arming::default(), activation).await })
        })
        .await
        .map(|_| ())
    }
    async fn activate_with(
        &self,
        name: &str,
        version: u64,
        epoch: u64,
        arming: &Arming,
        activation: &Activation,
    ) -> anyhow::Result<Option<Definitions>> {
        self.update(|rows| {
            Box::pin(async move { activate(rows, name, version, Some(epoch), arming, activation).await })
        })
        .await
    }
    async fn definitions(&self, name: &str) -> anyhow::Result<Definitions> {
        self.read(|rows| Box::pin(async move { Ok(load_definitions(rows, name).await?.unwrap_or_default()) })).await
    }
    // One ordered range read of at most `limit` notice rows.
    async fn notices(&self, limit: usize) -> anyhow::Result<Vec<(String, Value)>> {
        self.read(|rows| {
            Box::pin(async move {
                Ok(rows.scan("notice", "", None, limit).await?.into_iter().map(|((_, k), v)| (k, v)).collect())
            })
        })
        .await
    }
    async fn acknowledge(&self, key: &str) -> anyhow::Result<bool> {
        self.update(|rows| {
            Box::pin(async move {
                if rows.get("notice", key).await?.is_none() {
                    return Ok(false);
                }
                rows.delete("notice", key).await?;
                Ok(true)
            })
        })
        .await
    }
}

// ── 7. operator commands and bounded indexes ──────────────────────────────

async fn definition_summary(
    rows: &mut dyn Transaction,
    name: &str,
    defs: Definitions,
) -> anyhow::Result<control::DefinitionSummary> {
    use control::{QueuedSlot, ScheduleCatchup, ScheduleKind, ScheduleSummary, VersionSummary};
    let mut schedules = Vec::new();
    if let Some(active) = defs.active.and_then(|v| defs.versions.get(&v))
        && let Some(schedule) = ows::model::parse_yaml(&active.yaml)?.schedule
    {
        let paused = if defs.tripped.as_ref().is_some_and(|t| t.manual.is_some()) {
            get::<control::Paused>(rows, "paused", name).await?
        } else {
            None
        };
        for (kind, configured) in [
            (ScheduleKind::Every, schedule.every.is_some()),
            (ScheduleKind::Cron, schedule.cron.is_some()),
            (ScheduleKind::After, schedule.after.is_some()),
            (ScheduleKind::At, schedule.at.is_some()),
            (ScheduleKind::On, schedule.on.is_some()),
        ] {
            if configured {
                schedules.push(ScheduleSummary {
                    kind,
                    next_due_ms: None,
                    overlap: schedule.overlap,
                    catchup: match schedule.catchup {
                        ows::model::Catchup::Latest => ScheduleCatchup::Latest,
                        ows::model::Catchup::None => ScheduleCatchup::None,
                    },
                    queued: None,
                });
            }
        }
        // Trigger ownership comes from the catalog, never from key naming.
        // Manual pauses keep the same observation in their suspended state.
        let keys: Vec<_> =
            paused.as_ref().map_or_else(|| defs.triggers.iter().collect(), |p| p.triggers.keys().collect());
        for key in keys {
            let trigger = match &paused {
                Some(p) => p.triggers.get(key).cloned(),
                None => get::<Trigger>(rows, "trigger", key).await?,
            };
            let Some(t) = trigger else { continue };
            let kind = match t.state["kind"].as_str() {
                Some("every") => ScheduleKind::Every,
                Some("cron") => ScheduleKind::Cron,
                Some("after") => ScheduleKind::After,
                Some("at") => ScheduleKind::At,
                Some("on") => ScheduleKind::On,
                _ => continue,
            };
            let Some(summary) = schedules.iter_mut().find(|s| s.kind == kind) else { continue };
            summary.next_due_ms = t.at_ms;
            let q = match &paused {
                Some(p) => p.queues.get(key).cloned().map(decode_queue).transpose()?,
                None => queued(rows, key).await?,
            };
            summary.queued = q.map(|q| QueuedSlot { run: q.admission.id, version: q.admission.version });
        }
    }
    Ok(control::DefinitionSummary {
        name: name.into(),
        versions: defs
            .versions
            .iter()
            .map(|(&version, d)| {
                let latest = defs.activations.get(&version);
                VersionSummary {
                    version,
                    digest: d.digest.clone(),
                    activated_ms: latest.map(|a| a.at_ms),
                    activated_epoch: latest.map(|a| a.epoch),
                    activated_by: latest.and_then(|a| a.actor.clone()),
                    activation_note: latest.and_then(|a| a.note.clone()),
                }
            })
            .collect(),
        active: defs.active,
        epoch: defs.epoch,
        consecutive_faults: defs.consecutive_faults,
        tripped: defs.tripped,
        schedules,
    })
}

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
async fn page_rows(
    rows: &mut dyn Transaction,
    kind: &str,
    query: &str,
    since: Option<i64>,
    before: Option<i64>,
    cursor: Option<&control::Cursor>,
    limit: usize,
) -> anyhow::Result<(Rows, Option<control::Cursor>)> {
    use control::ControlError;
    if !(1..=control::MAX_PAGE).contains(&limit) {
        return Err(ControlError::InvalidInput(format!("page limit must be 1..={}", control::MAX_PAGE)).into());
    }
    if since.zip(before).is_some_and(|(a, b)| a > b) {
        return Err(ControlError::InvalidInput("invalid time window".into()).into());
    }
    let start = match cursor {
        Some(c) => {
            if c.query != query {
                return Err(ControlError::InvalidInput("cursor belongs to another query".into()).into());
            }
            c.after.clone()
        }
        None => since.map_or_else(String::new, time_key),
    };
    let end = before.map(time_key);
    if end.as_ref().is_some_and(|end| start >= *end) {
        return Ok((Rows::new(), None));
    }
    let found = rows.scan(kind, &start, end.as_deref(), limit + usize::from(cursor.is_some())).await?;
    let found: Rows = found.into_iter().filter(|((_, k), _)| cursor.is_none_or(|c| *k > c.after)).take(limit).collect();
    let next = if found.len() == limit {
        found.last_key_value().map(|((_, k), _)| control::Cursor { query: query.into(), after: k.clone() })
    } else {
        None
    };
    Ok((found, next))
}
async fn query_runs(
    rows: &mut dyn Transaction,
    f: &control::Runs,
    history: bool,
    cursor: Option<&control::Cursor>,
    limit: usize,
) -> anyhow::Result<control::Page<SavedRun>> {
    let query = serde_json::to_string(&("runs", f, history))?;
    let kind = run_kind(f, history);
    let (found, next) = page_rows(rows, &kind, &query, f.since_ms, f.before_ms, cursor, limit).await?;
    let mut items = Vec::new();
    for v in found.into_values() {
        let id = v.as_str().ok_or_else(|| anyhow::anyhow!("invalid control run index"))?;
        let r = run(rows, id).await?.run;
        if f.definition.as_ref().is_none_or(|d| *d == r.admission.name)
            && f.state.as_ref().is_none_or(|state| *state == r.state)
            && f.parent.as_ref().is_none_or(|p| r.child.as_ref().is_some_and(|c| c.parent == *p))
        {
            items.push(r);
        }
    }
    Ok(control::Page { items, next })
}
/// Holds (`Some`) or releases (`None`) a live run and its live owned
/// descendants in this transaction: the cancel cascade's reach, found the
/// same way (the `children` index, level by level, bounded by the subtree),
/// each row rewritten once with its indexes. A run already in the wanted
/// state is left alone, so the answer is whether anything changed. An ended
/// run has nothing to freeze.
async fn set_hold(rows: &mut dyn Transaction, id: &str, hold: Option<control::Hold>) -> anyhow::Result<bool> {
    use control::{ControlError, Entity};
    let root =
        find_run(rows, id).await?.ok_or_else(|| ControlError::NotFound { entity: Entity::Run, id: id.into() })?;
    if root.run.ended_ms.is_some() {
        return Ok(false);
    }
    let mut tree = vec![root];
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next].run.admission.id.clone();
        next += 1;
        tree.extend(indexed_runs(rows, &partition("children", &parent), "", None, usize::MAX).await?);
    }
    let mut changed = false;
    for mut s in tree {
        if s.run.held.is_some() == hold.is_some() {
            continue;
        }
        let before = indexes(&s);
        s.run.held = hold.clone();
        store_run_indexed(rows, &s, &before).await?;
        changed = true;
    }
    Ok(changed)
}
async fn suspend(rows: &mut dyn Transaction, name: &str, defs: &mut Definitions) -> anyhow::Result<()> {
    let mut triggers = BTreeMap::new();
    let mut queues = BTreeMap::new();
    for k in &defs.triggers {
        if let Some(t) = get(rows, "trigger", k).await? {
            triggers.insert(k.clone(), t);
        }
        if let Some(queue) = rows.get("trigger_queue", k).await? {
            queues.insert(k.clone(), queue);
        }
    }
    put(rows, "paused", name, &control::Paused { epoch: defs.epoch, triggers, queues }).await?;
    arm(rows, defs, &BTreeMap::new()).await
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
async fn control_command(
    rows: &mut dyn Transaction,
    request: &control::Request,
    command: &control::Command,
    now: i64,
    policy: &Policy,
) -> anyhow::Result<control::Applied> {
    use control::{Applied, Command, ControlError, Entity, Reply};
    if request.id.trim().is_empty() || request.actor.trim().is_empty() || request.reason.trim().is_empty() {
        return Err(ControlError::InvalidInput("control request requires id, actor and reason".into()).into());
    }
    let canonical = normalize_command(command);
    if let Some(entry) = get::<control::Entry>(rows, "control_command", &request.id).await? {
        if entry.request != *request || entry.command != canonical {
            return Err(ControlError::RequestConflict { id: request.id.clone() }.into());
        }
        return Ok(Applied { reply: entry.reply, repeated: true });
    }
    let unknown = |name: &str| ControlError::NotFound { entity: Entity::Definition, id: name.into() };
    let reply = match command {
        Command::Cancel { run: id } => {
            let changed = find_run(rows, id).await?.is_some_and(|s| s.run.ended_ms.is_none());
            if changed {
                terminate(rows, id, &Status::Cancelled, now).await?;
            }
            Reply::Changed(changed)
        }
        Command::Hold { run: id, note } => {
            let hold = control::Hold {
                actor: request.actor.clone(),
                reason: request.reason.clone(),
                note: note.clone(),
                at_ms: now,
            };
            Reply::Changed(set_hold(rows, id, Some(hold)).await?)
        }
        Command::Release { run: id } => Reply::Changed(set_hold(rows, id, None).await?),
        Command::Stop { enabled } => {
            let changed = is_stopped(rows).await? != *enabled;
            if changed {
                if *enabled {
                    put(rows, "control", "stopped", enabled).await?;
                } else {
                    rows.delete("control", "stopped").await?;
                    drain_pending(rows, now, control::MAX_PAGE).await?;
                }
            }
            Reply::Changed(changed)
        }
        Command::Pause { definition: name, note } => {
            let mut defs = load_definitions(rows, name).await?.ok_or_else(|| unknown(name))?;
            let manual = control::ManualTrip {
                actor: request.actor.clone(),
                reason: request.reason.clone(),
                note: note.clone(),
            };
            let changed = defs.tripped.as_ref().is_none_or(|t| t.manual.is_none());
            if let Some(trip) = defs.tripped.as_mut().filter(|t| t.manual.is_none()) {
                // A breaker's trip already stopped the schedule and its
                // buffered slot; the pause stands beside its report, so every
                // admission is refused, other content too, until `resume`.
                trip.manual = Some(manual);
                suspend(rows, name, &mut defs).await?;
                store_definitions(rows, name, &defs).await?;
            } else if changed {
                let version = defs
                    .active
                    .ok_or_else(|| ControlError::InvalidState(format!("definition {name} has no active version")))?;
                suspend(rows, name, &mut defs).await?;
                defs.tripped = Some(Trip {
                    manual: Some(manual),
                    breaker: false,
                    version,
                    digest: defs.versions.get(&version).map(|d| d.digest.clone()).unwrap_or_default(),
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
                store_definitions(rows, name, &defs).await?;
            }
            Reply::Changed(changed)
        }
        Command::Resume { definition: name } => {
            let mut defs = load_definitions(rows, name).await?.ok_or_else(|| unknown(name))?;
            if defs.tripped.as_ref().is_some_and(|t| t.manual.is_none()) {
                return Err(ControlError::InvalidState(format!("{name}: a breaker trip requires activation")).into());
            }
            let changed = defs.tripped.is_some();
            if changed {
                let paused: control::Paused =
                    get(rows, "paused", name).await?.ok_or_else(|| anyhow::anyhow!("missing suspended triggers"))?;
                if paused.epoch != defs.epoch {
                    return Err(ControlError::InvalidState(format!("{name}: suspended activation changed")).into());
                }
                // Only the pause is lifted: a breaker's trip under it stays,
                // with nothing to restore (the trip disarmed the definition).
                defs.tripped = defs.tripped.take().filter(|t| t.breaker).map(|t| Trip { manual: None, ..t });
                arm(rows, &mut defs, &paused.triggers).await?;
                for (key, value) in &paused.queues {
                    let q: Queued = serde_json::from_value(value.clone())?;
                    anyhow::ensure!(q.version == 1 && q.admission.name == *name, "invalid suspended queue");
                    rows.put("trigger_queue", key, value).await?;
                    put(rows, &partition("queue_name", name), key, key).await?;
                }
                rows.delete("paused", name).await?;
                store_definitions(rows, name, &defs).await?;
                drain_queues(rows, name, now).await?;
            }
            Reply::Changed(changed)
        }
        Command::Respond { run: id, wait: key, event } => {
            let identity = identity(event)
                .ok_or_else(|| ControlError::InvalidInput("response requires event source and id".into()))?;
            let kind = partition("control_response", id);
            let dedup = component(&identity);
            let closed = || ControlError::WaitClosed { run: id.clone(), wait: key.clone() };
            if let Some(entry) = get::<control::Entry>(rows, &kind, &dedup).await? {
                if entry.command != canonical {
                    return Err(ControlError::ResponseConflict { identity: identity.replace('\u{0}', "/") }.into());
                }
                entry.reply
            } else {
                let s = find_run(rows, id)
                    .await?
                    .ok_or_else(|| ControlError::NotFound { entity: Entity::Run, id: id.clone() })?;
                let w = s.waits.get(key).ok_or_else(closed)?;
                if s.run.ended_ms.is_some() || w.timeout_ms.is_some_and(|at| now >= at) {
                    return Err(closed().into());
                }
                let mut event = event.clone();
                if let Value::Object(m) = &mut event {
                    m.entry("specversion").or_insert(json!("1.0"));
                    let time = chrono::DateTime::from_timestamp_millis(now)
                        .ok_or_else(|| anyhow::anyhow!("invalid response timestamp"))?
                        .to_rfc3339();
                    m.entry("time").or_insert(json!(time));
                }
                let matched = w.accepts(&event).map_err(|e| {
                    ControlError::InvalidState(format!("wait {key} of run {id} cannot be matched here: {e}"))
                })?;
                if !matched {
                    return Err(ControlError::WaitMismatch { run: id.clone(), wait: key.clone() }.into());
                }
                let result = Reply::Responded(deliver(rows, &event, Some(id), now, policy).await?);
                let sequence: u64 = get(rows, &identities_of(id), &dedup)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("response was not accepted"))?;
                // A validated answer is also offered to a wildcard listen.
                // Ordinary addressed deliveries retain their type-only routing.
                if w.types.iter().any(|t| t == "*") {
                    let mut stored = copy(rows, id, sequence).await?;
                    anyhow::ensure!(stored.consumed_key.is_none(), "response event already consumed by another wait");
                    if stored.delivered {
                        stored.delivered = false;
                        let mut current = run(rows, id).await?;
                        current.run.wake_at_ms = current.run.wake_at_ms.min(now);
                        current.published = current
                            .published
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("published counter overflow"))?;
                        put(rows, &events_of(id), &self::sequence(sequence), &stored).await?;
                        put(rows, &published_of(id), &self::sequence(sequence), &sequence).await?;
                        let live = stored.expires_at_ms.map_or_else(
                            || format!("~:{}", self::sequence(sequence)),
                            |at| timed(at, &self::sequence(sequence)),
                        );
                        put(rows, &live_published_of(id), &live, &sequence).await?;
                        store_run(rows, &current).await?;
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
                )
                .await?;
                result
            }
        }
    };
    let entry = control::Entry { request: request.clone(), command: canonical, at_ms: now, reply: reply.clone() };
    put(rows, "control_command", &request.id, &entry).await?;
    let k = timed(now, &component(&request.id));
    put(rows, "control_history", &k, &entry).await?;
    if let Command::Cancel { run }
    | Command::Respond { run, .. }
    | Command::Hold { run, .. }
    | Command::Release { run } = command
    {
        put(rows, &partition("control_history_run", run), &k, &entry).await?;
    }
    Ok(Applied { reply, repeated: false })
}

async fn snapshot(rows: &mut dyn Transaction, s: StoredRun) -> anyhow::Result<RunSnapshot> {
    let records = rows
        .scan(&records_of(&s.run.admission.id), "", None, usize::MAX)
        .await?
        .into_iter()
        .map(|((_, k), v)| Ok((k, serde_json::from_value(v)?)))
        .collect::<anyhow::Result<_>>()?;
    let events = rows
        .scan(&events_of(&s.run.admission.id), "", None, usize::MAX)
        .await?
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
