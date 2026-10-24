//! Schedules and published events: 1. the triggers a definition's
//! `schedule` keeps in the store, armed in the commit that activates it;
//! 2. firing due triggers (`every`, `cron`, `after`), with the missed-slot
//! and overlap policies; 3. publishing an event to the runs listening for it
//! and to event-triggered schedules (`on`), each ingress event at most once;
//! 4. handing the store's notices (breaker trips) on to the host.
//!
//! The store holds generic triggers (`ouro_durable::Trigger`): indexed by
//! the time they are next due and by the event types they react to, and
//! changed only by compare-and-swap together with the runs they admit. The
//! DSL stays here: cron, intervals and event strategies are computed by the
//! runtime. Nothing scans definitions or runs: the worker reads the due
//! index (a range ending now), and a publish reads the trigger index of the
//! event's type.
//!
//! Every activation, and nothing else, starts an activation epoch. The
//! activation's commit replaces the definition's triggers with the activated
//! version's (a version without a schedule leaves none); a breaker trip
//! removes them in its own commit, and only the next activation arms any
//! again. Each trigger's state names its epoch, so a trigger of an older
//! epoch is never the stored one again: it cannot fire, and an old run's end
//! cannot re-arm the new epoch's trigger (the store re-arms only the exact
//! trigger the run's firing stored).
//!
//! A slot's run id is derived from the slot (`schedule:<name>:every:<ms>`,
//! `…:cron:<ms>`, `…:after:<epoch>:<n>`, `…:on:<digest of the events>`) and
//! the trigger moves to the next slot in the commit that admits it, so
//! several processes start each run once. `document.metadata.overlap` chooses
//! skip, allow, queue (one durable slot) or replace (normal cancellation)
//! over the trigger's unfinished runs. The store decides it and the host's
//! definition ceiling in the firing's commit. A missed slot under
//! `catchup: none` is skipped too. Skips stay in the trigger's state (`skips`,
//! the last `skipped` slots); time skips are traced.
//!
//! A slot whose run this host refuses to admit (a function the definition
//! calls is not registered, or it no longer validates) is a per-definition
//! outcome, never an error of the scheduler: the firing consumes the slot
//! and records the run as ended faulted at its admission, which counts
//! toward the definition's breaker (`ouro_durable::Firing::refused`). Only
//! such deterministic refusals do: an admission interrupted by the host (its
//! schema resolver failing, `ResourceUnavailable`) keeps the slot in the
//! trigger, uncounted, and retries it after a capped backoff.
//!
//! `on` keeps the events matched so far in its trigger's state and replays
//! them through the same consumer a `listen` uses (`owf::events`), so `all`,
//! correlation and `until` mean what they mean in a listen. An identified
//! event is taken from ingress once, by the first trigger (in key order)
//! that wants it, under a tombstone with its own lifetime: one event starts
//! at most one run. CHOICE: no listener run is involved (an earlier version
//! kept one per schedule): a run exists only once its events have arrived.
use crate::{
    Runtime,
    execution::{Admitter, ResourceUnavailable},
    telemetry,
};
use opentelemetry::KeyValue;
use ouro_durable::{
    Admission, Arming, DefinitionTripped, Definitions, Fired, Firing, Overlap, OverlapGuard, Rearm, Refused, RunStore,
    Trigger,
};
use owf::{
    events::{self, Consumer, Offer},
    expr::{self, Args},
    model::{self, Catchup, Schedule, Strategy, Until},
    run::WorkflowError,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

/// How many due triggers one call fires, and how many triggers one event
/// may reach; more wait for the next tick.
const BATCH: usize = 256;

/// How many skipped slots a trigger keeps in its state.
const SKIPS_KEPT: usize = 16;

/// The trigger state of a slot whose admission was interrupted, waiting for
/// its retry: `{ slot, failures, error }`.
const RETRY: &str = "retry";

fn key(name: &str, kind: &str) -> String {
    format!("{name}#{kind}")
}

/// FNV-1a: a stable digest, the same in every process.
fn digest(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{hash:016x}")
}

fn millis(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX).max(1)
}

/// The first `every` slot after `after`: slots are aligned to the epoch.
fn next_every(every: i64, after: i64) -> i64 {
    after - after.rem_euclid(every) + every
}

/// The first `cron` occurrence (UTC) after `after`.
fn next_cron(cron: &str, after: i64) -> anyhow::Result<i64> {
    let cron: croner::Cron = cron.parse()?;
    let at = chrono::DateTime::from_timestamp_millis(after).ok_or_else(|| anyhow::anyhow!("invalid time"))?;
    Ok(cron.find_next_occurrence(&at, false)?.timestamp_millis())
}

/// The first slot of a time trigger after `after`.
fn next_slot(schedule: &Schedule, kind: &str, after: i64) -> anyhow::Result<Option<i64>> {
    Ok(match (kind, schedule) {
        ("every", Schedule { every: Some(every), .. }) => Some(next_every(millis(*every), after)),
        ("cron", Schedule { cron: Some(cron), .. }) => Some(next_cron(cron, after)?),
        _ => None,
    })
}

/// The active version's schedule.
fn active(definitions: &Definitions) -> anyhow::Result<Option<Schedule>> {
    let Some(d) = definitions.active.and_then(|v| definitions.versions.get(&v)) else { return Ok(None) };
    Ok(model::parse_yaml(&d.yaml)?.schedule)
}

/// Whether a trigger belongs to the definition's current, untripped epoch.
fn current(definitions: &Definitions, trigger: &Trigger) -> bool {
    definitions.tripped.is_none() && trigger.state["epoch"] == json!(definitions.epoch)
}

// ── 1. arming ──────────────────────────────────────────────────────────────

/// The triggers `yaml`'s schedule arms for `name` in `epoch`. A version
/// without a schedule arms none.
fn arming(name: &str, yaml: &str, epoch: u64, now: i64) -> anyhow::Result<BTreeMap<String, Trigger>> {
    let wf = model::parse_yaml(yaml)?;
    let Some(s) = wf.schedule else { return Ok(BTreeMap::new()) };
    let state = |kind: &str, extra: Value| {
        let mut state = json!({ "name": name, "kind": kind, "epoch": epoch });
        if let (Value::Object(m), Value::Object(e)) = (&mut state, extra) {
            m.extend(e);
        }
        state
    };
    let mut out = BTreeMap::new();
    for kind in ["every", "cron"] {
        if let Some(slot) = next_slot(&s, kind, now)? {
            out.insert(
                key(name, kind),
                Trigger { at_ms: Some(slot), types: Vec::new(), state: state(kind, json!({})) },
            );
        }
    }
    if s.after.is_some() {
        let trigger = Trigger { at_ms: Some(now), types: Vec::new(), state: state("after", json!({ "n": 1 })) };
        out.insert(key(name, "after"), trigger);
    }
    if let Some(on) = &s.on {
        let types = events::routes(on, &Args::new());
        out.insert(key(name, "on"), Trigger { at_ms: None, types, state: state("on", json!({ "events": [] })) });
    }
    Ok(out)
}

impl Runtime {
    /// Activates `version` of `name` in a new epoch: its triggers replace
    /// the definition's ones in the activation's commit.
    pub(crate) fn activate_armed(&self, name: &str, version: u64) -> anyhow::Result<()> {
        for _ in 0..16 {
            let definitions = self.store.definitions(name)?;
            let d = definitions.versions.get(&version).ok_or_else(|| anyhow::anyhow!("unknown definition version"))?;
            let triggers = arming(name, &d.yaml, definitions.epoch + 1, self.clock.now_ms())?;
            if self.store.activate_with(name, version, definitions.epoch, &Arming { triggers })?.is_some() {
                return Ok(());
            }
        }
        anyhow::bail!("definition {name} changed too often to activate")
    }
    /// The triggers `name`'s current activation armed, with their state
    /// (the next slot, skipped slots, matched events).
    pub fn triggers(&self, name: &str) -> anyhow::Result<Vec<(String, Trigger)>> {
        let definitions = self.store.definitions(name)?;
        let mut out = Vec::new();
        for k in definitions.triggers {
            if let Some(t) = self.store.trigger(&k)? {
                out.push((k, t));
            }
        }
        Ok(out)
    }
    /// Buffered slots, with their pinned definitions, inputs and bindings.
    /// They are admitted when this trigger's live runs have ended and the
    /// definition has capacity. A new activation discards its old buffers.
    pub fn queued(&self, name: &str) -> anyhow::Result<Vec<Admission>> {
        let mut queued = Vec::new();
        for k in self.store.definitions(name)?.triggers {
            queued.extend(self.store.queued(&k)?);
        }
        Ok(queued)
    }
}

// ── 2. firing ──────────────────────────────────────────────────────────────

/// A time trigger's firing from `t` to `next`.
fn moved<'a>(key: &'a str, t: &'a Trigger, next: &'a Trigger, busy: Option<&'a Trigger>) -> Firing<'a> {
    Firing { key, expected: Some(t), next: Some(next), busy, ..Firing::default() }
}

/// A trigger's run: admitted, refused by this host (recorded with the slot,
/// `Firing::refused`), or not at all because its definition tripped since
/// the trigger was read (the trip removed the trigger: nothing to fire).
fn triggered(
    admitter: &Admitter,
    id: &str,
    name: &str,
    input: Value,
) -> anyhow::Result<Option<Result<Admission, Refused>>> {
    match admitter.triggered(id, name, input) {
        Ok(admitted) => Ok(Some(admitted)),
        Err(e) if e.is::<DefinitionTripped>() => Ok(None),
        Err(e) => Err(e),
    }
}

/// `t` with a skipped slot recorded in its state.
fn skipped(mut t: Trigger, slot: i64, reason: &str) -> Trigger {
    let mut skipped: Vec<Value> = t.state["skipped"].as_array().cloned().unwrap_or_default();
    let mut record = json!({ "slot": slot, "reason": reason });
    if reason == "overlap: ceiling" {
        record["overlap"] = json!("ceiling");
    }
    skipped.push(record);
    let excess = skipped.len().saturating_sub(SKIPS_KEPT);
    skipped.drain(..excess);
    t.state["skipped"] = Value::Array(skipped);
    t.state["skips"] = json!(t.state["skips"].as_u64().unwrap_or(0) + 1);
    t
}

impl Runtime {
    /// Starts the runs whose slots are due now and moves each trigger to its
    /// next slot in the same commit. A slot missed while no worker ran starts
    /// once, late (`catchup: latest`), or is skipped (`none`); either way the
    /// next slot is the first after now, so a scheduler that was down does
    /// not start a burst. Scheduled runs get `{}` as input. Returns the runs
    /// it admitted. The worker calls it on every tick: when nothing is due it
    /// reads no rows.
    ///
    /// A slot whose run this host refuses to admit (a function the
    /// definition calls is no longer registered, or the definition no longer
    /// validates) does not stop the others: the slot is consumed like an
    /// admitted one, and its run is recorded as ended faulted with the
    /// reason, under the slot's id (`get`, `list`). The fault counts toward
    /// the definition's breaker, so a definition that stays broken trips
    /// after `Config::breaker_threshold` slots, which removes its schedule
    /// and stores the trip's notice. CHOICE: consumed, not retried at the
    /// next tick: a retry would refire the same slot on every tick, and a
    /// consumed slot is never lost silently, it holds its failed run.
    ///
    /// An admission the host interrupts (its schema resolver fails,
    /// `ResourceUnavailable`) is no property of the definition (decision 5):
    /// the slot is neither consumed nor counted; the trigger keeps it and is
    /// due again after a capped backoff (`interrupted`). Only storage errors
    /// are returned.
    pub fn fire_due(&self) -> anyhow::Result<Vec<String>> {
        let now = self.clock.now_ms();
        let mut started = Vec::new();
        for (k, t) in self.store.due_triggers(now, BATCH)? {
            started.extend(self.fire(&k, &t, now)?);
        }
        Ok(started)
    }

    fn fire(&self, k: &str, t: &Trigger, now: i64) -> anyhow::Result<Option<String>> {
        // A slot whose admission was interrupted keeps its identity while
        // its trigger waits for the retry (`retry`).
        let slot = t.state[RETRY]["slot"].as_i64().or(t.at_ms);
        let (Some(name), Some(kind), Some(slot)) = (t.state["name"].as_str(), t.state["kind"].as_str(), slot) else {
            anyhow::bail!("trigger {k} is not a schedule's");
        };
        let definitions = self.store.definitions(name)?;
        // Replaced by a newer epoch since it was read: its swap would fail.
        let Some(schedule) = active(&definitions)?.filter(|_| current(&definitions, t)) else { return Ok(None) };
        let mut next = t.clone();
        if let Value::Object(state) = &mut next.state {
            state.remove(RETRY);
        }
        let mut rearm = None;
        let mut busy = next.clone();
        let id = match kind {
            "every" | "cron" => {
                let Some(following) = next_slot(&schedule, kind, slot)? else { return Ok(None) };
                next.at_ms = next_slot(&schedule, kind, slot.max(now))?;
                busy.at_ms = next.at_ms;
                if following <= now && schedule.catchup == Catchup::None {
                    let skip = skipped(next, slot, "missed");
                    self.store.fire(&moved(k, t, &skip, None), now)?;
                    self.skip(name, kind, slot, "missed");
                    return Ok(None);
                }
                format!("schedule:{name}:{kind}:{slot}")
            }
            "after" => {
                let Some(after) = schedule.after else { return Ok(None) };
                let n = t.state["n"].as_u64().unwrap_or(1);
                // Not due again until this run ends: the store re-arms it then.
                next.at_ms = None;
                next.state["n"] = json!(n + 1);
                rearm = Some(Rearm { trigger: k.to_owned(), delay_ms: millis(after) });
                // A previous run still going: look again one delay later.
                busy.at_ms = Some(now.saturating_add(millis(after)));
                format!("schedule:{name}:after:{}:{n:010}", definitions.epoch)
            }
            _ => return Ok(None),
        };
        let ceiling = skipped(busy.clone(), slot, "overlap: ceiling");
        let busy = skipped(busy, slot, "overlap");
        let admission = match triggered(&self.admitter(), &id, name, json!({})) {
            Ok(Some(admission)) => admission,
            Ok(None) => return Ok(None),
            Err(e) if e.is::<ResourceUnavailable>() => return self.interrupted(k, t, slot, &e, now),
            Err(e) => return Err(e),
        };
        let refused = admission.as_ref().err();
        if refused.is_some()
            && let Some(rearm) = &rearm
        {
            // No run of a refused slot will end and re-arm `after`: it is
            // due again one delay from now.
            next.at_ms = Some(now.saturating_add(rearm.delay_ms));
        }
        let mut firing = moved(k, t, &next, Some(&busy));
        match &admission {
            Ok(admission) => {
                firing.admit = std::slice::from_ref(admission);
                firing.rearm = rearm.as_ref();
            }
            Err(refused) => firing.refused = Some(refused),
        }
        firing.overlap = Some(OverlapGuard {
            policy: schedule.overlap,
            max_concurrent_runs: self.config.max_concurrent_runs,
            ceiling: &ceiling,
        });
        let result = self.store.fire(&firing, now)?;
        if schedule.overlap == Overlap::Replace {
            self.signal_cancelled()?;
        }
        match result {
            Fired::Committed => match refused {
                Some(refused) => {
                    self.refused(name, kind, &id, &refused.error);
                    Ok(None)
                }
                None => Ok(Some(id)),
            },
            Fired::Busy(_) => {
                self.skip(name, kind, slot, "overlap");
                Ok(None)
            }
            Fired::Ceiling => {
                self.skip(name, kind, slot, "overlap: ceiling");
                Ok(None)
            }
            Fired::Changed | Fired::Ingested | Fired::Queued => Ok(None),
        }
    }

    /// The admission of `t`'s slot was interrupted (`ResourceUnavailable`):
    /// an infrastructure failure, not the definition's. The slot is neither
    /// consumed nor counted toward the breaker: the trigger keeps it
    /// (`retry`: the slot and the failures so far) and is due again after a
    /// capped backoff, 1 s doubling to five minutes, kept with the trigger
    /// so a restart does not shorten it and a waiting retry never holds the
    /// head of the due index. The worker goes on with the other triggers.
    fn interrupted(
        &self,
        k: &str,
        t: &Trigger,
        slot: i64,
        e: &anyhow::Error,
        now: i64,
    ) -> anyhow::Result<Option<String>> {
        let failures = t.state[RETRY]["failures"].as_u64().unwrap_or(0).saturating_add(1);
        let mut retried = t.clone();
        retried.at_ms = Some(retry_at(now, u32::try_from(failures).unwrap_or(u32::MAX)));
        retried.state[RETRY] = json!({ "slot": slot, "failures": failures, "error": e.to_string() });
        self.store.fire(&Firing { key: k, expected: Some(t), next: Some(&retried), ..Firing::default() }, now)?;
        telemetry::mark(
            &self.tracer,
            "schedule interrupted",
            vec![
                KeyValue::new("ouro.definition.name", t.state["name"].as_str().unwrap_or_default().to_string()),
                KeyValue::new("ouro.schedule.kind", t.state["kind"].as_str().unwrap_or_default().to_string()),
                KeyValue::new("ouro.schedule.slot", slot),
                KeyValue::new("ouro.schedule.failures", i64::try_from(failures).unwrap_or(i64::MAX)),
                KeyValue::new("ouro.schedule.failure", e.to_string()),
            ],
        );
        Ok(None)
    }

    fn refused(&self, name: &str, kind: &str, run: &str, error: &WorkflowError) {
        telemetry::mark(
            &self.tracer,
            "schedule refused",
            vec![
                KeyValue::new("ouro.definition.name", name.to_string()),
                KeyValue::new("ouro.schedule.kind", kind.to_string()),
                KeyValue::new("ouro.run.id", run.to_string()),
                KeyValue::new("ouro.error.type", error.type_.clone()),
            ],
        );
    }

    fn skip(&self, name: &str, kind: &str, slot: i64, reason: &str) {
        telemetry::mark(
            &self.tracer,
            "schedule skip",
            vec![
                KeyValue::new("ouro.definition.name", name.to_string()),
                KeyValue::new("ouro.schedule.kind", kind.to_string()),
                KeyValue::new("ouro.schedule.slot", slot),
                KeyValue::new("ouro.schedule.reason", reason.to_string()),
            ],
        );
    }
}

// ── 3. publishing ──────────────────────────────────────────────────────────

impl Runtime {
    /// Delivers a CloudEvent (at least `id`, `source` and `type`) to every
    /// run listening for it and to the event-triggered schedules it may
    /// start. `specversion` and `time` are filled in when absent.
    /// Publishing the same `source` + `id` again delivers nothing new and
    /// starts nothing; another payload under it is an error. An `on`
    /// schedule the host refuses records a failed run (as `fire_due`); one
    /// whose admission it interrupts (`ResourceUnavailable`) does not take the
    /// event and the error is returned: publishing it again later is safe.
    /// Returns the runs it reached or started.
    pub fn publish(&self, mut event: Value) -> anyhow::Result<Vec<String>> {
        let now = self.clock.now_ms();
        complete(&mut event, now)?;
        publish(&*self.store, &self.admitter(), &event, now)
    }
}

/// Checks a CloudEvent's identity and type; fills `specversion` and `time`.
pub(crate) fn complete(event: &mut Value, now: i64) -> anyhow::Result<()> {
    let Value::Object(e) = event else { anyhow::bail!("an event is an object") };
    for k in ["id", "source", "type"] {
        anyhow::ensure!(e.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()), "an event needs `{k}`");
    }
    e.entry("specversion").or_insert_with(|| json!(events::SPEC_VERSION));
    e.entry("time").or_insert_with(|| {
        json!(chrono::DateTime::from_timestamp_millis(now).map(|t| t.to_rfc3339()).unwrap_or_default())
    });
    Ok(())
}

/// The runs a published event reaches, and the run it starts through the
/// `on` schedules indexed under its type (runs and triggers in `store`,
/// definitions where the admitter reads them).
pub(crate) fn publish(
    store: &dyn RunStore,
    admitter: &Admitter,
    event: &Value,
    now: i64,
) -> anyhow::Result<Vec<String>> {
    // An identity some trigger already took is not offered again (and
    // another payload under it is an error, before anything is accepted).
    let ingested = store.ingested(event, now)?;
    let mut reached = store.publish(event, None, now)?;
    if ingested {
        return Ok(reached);
    }
    let t = event.get("type").and_then(Value::as_str).unwrap_or_default();
    for (k, trigger) in store.triggers_for(t, BATCH)? {
        if let Taken::Yes(run) = offer(store, admitter, &k, trigger, event, now)? {
            reached.extend(run);
            break;
        }
    }
    Ok(reached)
}

/// Whether a trigger took the event from ingress, and the run it started.
enum Taken {
    No,
    Yes(Option<String>),
}

/// Offers the event to an `on` trigger: its matched events plus this one
/// go through the schedule's strategy again; a complete match starts the
/// workflow and empties the trigger in one commit, a partial one is kept.
/// Either way the commit takes the event's identity from ingress.
fn offer(
    store: &dyn RunStore,
    admitter: &Admitter,
    k: &str,
    mut trigger: Trigger,
    event: &Value,
    now: i64,
) -> anyhow::Result<Taken> {
    let same = |a: &Value| a.get("id") == event.get("id") && a.get("source") == event.get("source");
    for _ in 0..16 {
        let name = trigger.state["name"].as_str().unwrap_or_default().to_owned();
        let definitions = admitter.definitions.definitions(&name)?;
        let Some(Schedule { on: Some(on), read, overlap, .. }) =
            active(&definitions)?.filter(|_| current(&definitions, &trigger))
        else {
            return Ok(Taken::No);
        };
        let mut matched: Vec<Value> = trigger.state["events"].as_array().cloned().unwrap_or_default();
        if matched.iter().any(same) {
            // This trigger holds it already (its tombstone has expired).
            return Ok(Taken::Yes(None));
        }
        matched.push(event.clone());
        let Some(outcome) = consume(&on, &matched)? else { return Ok(Taken::No) };
        let mut next = trigger.clone();
        let mut admission = None;
        let mut id = None;
        match outcome {
            Some(consumed) => {
                next.state["events"] = json!([]);
                let ids: Vec<String> = matched.iter().map(|e| format!("{}\u{0}{}", e["source"], e["id"])).collect();
                let run = format!("schedule:{name}:on:{}", digest(&ids.join("\u{1}")));
                let input: Vec<Value> = consumed.iter().map(|e| events::read(e, read)).collect();
                // Refused like a time slot's run (`fire_due`): the match is
                // consumed and the failed run recorded in its place.
                let Some(triggered) = triggered(admitter, &run, &name, Value::Array(input))? else {
                    return Ok(Taken::No);
                };
                id = triggered.is_ok().then_some(run);
                admission = Some(triggered);
            }
            None => next.state["events"] = Value::Array(matched),
        }
        let busy = skipped(next.clone(), now, "overlap");
        let ceiling = skipped(next.clone(), now, "overlap: ceiling");
        let mut firing = Firing {
            key: k,
            expected: Some(&trigger),
            next: Some(&next),
            ingress: Some(event),
            busy: Some(&busy),
            overlap: Some(OverlapGuard {
                policy: overlap,
                max_concurrent_runs: admitter.max_concurrent_runs,
                ceiling: &ceiling,
            }),
            ..Firing::default()
        };
        match &admission {
            Some(Ok(admission)) => firing.admit = std::slice::from_ref(admission),
            Some(Err(refused)) => firing.refused = Some(refused),
            None => {}
        }
        let result = store.fire(&firing, now)?;
        if overlap == Overlap::Replace {
            admitter.signal_cancelled(store)?;
        }
        match result {
            Fired::Committed => {
                return Ok(Taken::Yes(id));
            }
            Fired::Ingested | Fired::Busy(_) | Fired::Queued | Fired::Ceiling => return Ok(Taken::Yes(None)),
            Fired::Changed if store.stopped()? => return Ok(Taken::No),
            Fired::Changed => match store.trigger(k)? {
                Some(t) => trigger = t,
                None => return Ok(Taken::No),
            },
        }
    }
    anyhow::bail!("trigger {k} changed too often to offer an event")
}

/// Replays `events` through the strategy. `None`: the last one is not
/// wanted. `Some(None)`: wanted, not complete yet. `Some(Some(consumed))`:
/// complete, with the events that belong to the output (`until` ones excluded).
#[allow(clippy::option_option)]
fn consume(strategy: &Strategy, events: &[Value]) -> anyhow::Result<Option<Option<Vec<Value>>>> {
    let args = Args::new();
    let mut consumer = Consumer::new(strategy);
    let mut consumed = Vec::new();
    let mut last = Offer::Ignored;
    for e in events {
        last = consumer.offer(e, &Value::Null, &args).map_err(|e| anyhow::anyhow!(e))?;
        if last == Offer::Consumed {
            consumed.push(e.clone());
            if let Strategy::Any { until: Some(Until::Condition(c)), .. } = strategy {
                let data: Vec<Value> = consumed.iter().map(|e| events::read(e, model::Read::Data)).collect();
                if expr::truthy(&expr::eval(c, &Value::Array(data), &args).map_err(|e| anyhow::anyhow!(e))?) {
                    consumer.stop();
                }
            }
        }
    }
    Ok(match last {
        Offer::Ignored => None,
        _ if consumer.done() => Some(Some(consumed)),
        _ => Some(None),
    })
}

// ── 4. notices ─────────────────────────────────────────────────────────────

/// How many notices one call hands on; more wait for the next tick.
const NOTICES: usize = 64;
/// A notice the sink refused waits 1 s, then twice as long after each
/// further failure, at most five minutes.
const NOTICE_RETRY_MS: i64 = 1_000;
const NOTICE_RETRY_MAX_MS: i64 = 300_000;

/// When a notice that failed `failures` times may be tried again.
fn retry_at(now: i64, failures: u32) -> i64 {
    let delay = NOTICE_RETRY_MS.saturating_mul(1_i64 << failures.saturating_sub(1).min(20));
    now.saturating_add(delay.min(NOTICE_RETRY_MAX_MS))
}

impl Runtime {
    /// Hands each pending notice of the store on, oldest first: a breaker
    /// trip (`ouro_durable::TRIPPED`, `Trip::event`), committed with the trip
    /// itself. It goes to the runs and `on` schedules listening for its type,
    /// as `publish` does, then to the event sink, and is then acknowledged.
    /// At least once: a hand-off interrupted before its acknowledgement is
    /// repeated with the same `source` + `id`, which `publish` deduplicates
    /// and a sink must. A sink failure leaves the notice for a later call,
    /// after a capped exponential backoff of its own (1 s doubling to five
    /// minutes); a storage error is returned. The worker calls it on every
    /// tick; returns how many notices were acknowledged.
    pub async fn notify(&self, stop: &CancellationToken) -> anyhow::Result<usize> {
        let notices = self.store.notices(NOTICES)?;
        // Forget the backoff of notices no longer pending (acknowledged
        // elsewhere), so the map stays bounded by the pending page.
        self.notice_backoff.lock().unwrap().retain(|k, _| notices.iter().any(|(n, _)| n == k));
        let mut acknowledged = 0;
        for (k, event) in notices {
            if stop.is_cancelled() {
                break;
            }
            let now = self.clock.now_ms();
            if self.notice_backoff.lock().unwrap().get(&k).is_some_and(|(_, at)| *at > now) {
                continue;
            }
            let failed = |k: String, e: &dyn std::fmt::Display| {
                let mut backoff = self.notice_backoff.lock().unwrap();
                let failures = backoff.get(&k).map_or(0, |(n, _)| *n).saturating_add(1);
                backoff.insert(k, (failures, retry_at(self.clock.now_ms(), failures)));
                drop(backoff);
                self.noticed(&event, Some(&e.to_string()));
            };
            // An `on` schedule whose admission was interrupted (its schema
            // resolver failed) leaves the notice for a later hand-off, like a
            // sink failure; a publish repeated under the same identity
            // delivers nothing twice.
            match publish(&*self.store, &self.admitter(), &event, now) {
                Err(e) if e.is::<ResourceUnavailable>() => {
                    failed(k, &e);
                    continue;
                }
                result => result?,
            };
            if let Some(sink) = &self.capabilities.sink {
                let sent = tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    sent = sink.emit(&event) => sent,
                };
                if let Err(e) = sent {
                    failed(k, &e);
                    continue;
                }
            }
            self.noticed(&event, None);
            if self.store.acknowledge(&k)? {
                acknowledged += 1;
            }
            self.notice_backoff.lock().unwrap().remove(&k);
        }
        Ok(acknowledged)
    }

    fn noticed(&self, event: &Value, failure: Option<&str>) {
        let text = |k: &str| event[k].as_str().unwrap_or_default().to_string();
        let mut attributes = vec![
            KeyValue::new("ouro.event.type", text("type")),
            KeyValue::new("ouro.event.source", text("source")),
            KeyValue::new("ouro.event.id", text("id")),
        ];
        if let Some(failure) = failure {
            attributes.push(KeyValue::new("ouro.notice.failure", failure.to_string()));
        }
        telemetry::mark(&self.tracer, if failure.is_some() { "notice failed" } else { "notice" }, attributes);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn notice_retries_back_off_exponentially_up_to_a_cap() {
        let delays: Vec<i64> = [1, 2, 3, 9, 10, u32::MAX].iter().map(|n| super::retry_at(0, *n)).collect();
        assert_eq!(delays, vec![1_000, 2_000, 4_000, 256_000, 300_000, 300_000]);
    }
}
