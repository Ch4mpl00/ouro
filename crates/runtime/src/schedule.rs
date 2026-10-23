//! Schedules and published events: 1. the triggers a definition's
//! `schedule` keeps in the store, armed on activation; 2. firing due
//! triggers (`every`, `cron`, `after`); 3. publishing an event to the runs
//! listening for it and to event-triggered schedules (`on`).
//!
//! The store holds generic triggers (`ouro_durable::Trigger`): indexed by
//! the time they are next due and by the event types they react to, and
//! changed only by compare-and-swap together with the runs they admit. The
//! DSL stays here: cron, intervals and event strategies are computed by the
//! runtime. Nothing scans definitions or runs: the worker reads the due
//! index (a range ending now), and a publish reads the trigger index of the
//! event's type.
//!
//! A slot's run id is derived from the slot (`schedule:<name>:every:<ms>`,
//! `…:cron:<ms>`, `…:after:<n>`, `…:on:<digest of the events>`) and the
//! trigger moves to the next slot in the commit that admits it, so several
//! processes start each run once.
//!
//! `on` keeps the events matched so far in its trigger's state and replays
//! them through the same consumer a `listen` uses (`owf::events`), so `all`,
//! correlation and `until` mean what they mean in a listen. CHOICE: no
//! listener run is involved (an earlier version kept one per schedule): a
//! run exists only once its events have arrived, nothing waits between two
//! generations of listeners, and a publish costs a read of the matching
//! triggers instead of a delivery to a run plus a later scan.
use crate::{Runtime, execution::Admitter};
use ouro_durable::{Rearm, RunStore, Trigger};
use owf::{
    events::{self, Consumer, Offer},
    expr::{self, Args},
    model::{self, Schedule, Strategy, Until},
};
use serde_json::{Value, json};

/// How many due triggers one call fires, and how many triggers one event
/// may reach; more wait for the next tick.
const BATCH: usize = 256;

const KINDS: [&str; 4] = ["every", "cron", "after", "on"];

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

/// The active definition's schedule and its `schedule` as written.
struct Active {
    schedule: Option<Schedule>,
    raw: Value,
}

fn active(definitions: &dyn RunStore, name: &str) -> anyhow::Result<Option<Active>> {
    let definitions = definitions.definitions(name)?;
    let Some(d) = definitions.active.and_then(|v| definitions.versions.get(&v)) else { return Ok(None) };
    let wf = model::parse_yaml(&d.yaml)?;
    Ok(Some(Active { schedule: wf.schedule, raw: wf.definition["schedule"].clone() }))
}

// ── 1. arming ──────────────────────────────────────────────────────────────

impl Runtime {
    /// Brings `name`'s triggers in line with its active definition: arms
    /// what its schedule declares, removes what it no longer does. Called on
    /// activation; four point reads and at most four swaps.
    pub(crate) fn arm(&self, name: &str) -> anyhow::Result<()> {
        let active = active(&*self.store, name)?;
        let now = self.clock.now_ms();
        for kind in KINDS {
            let k = key(name, kind);
            for _ in 0..16 {
                let current = self.store.trigger(&k)?;
                let desired = desired(name, kind, active.as_ref(), current.as_ref(), now)?;
                if desired == current
                    || self.store.swap_trigger(&k, current.as_ref(), desired.as_ref(), &[], None, now)?
                {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// The trigger a schedule wants for `kind`. One already in place for the
/// same schedule is kept: its slot, its `after` count, its matched events.
fn desired(
    name: &str,
    kind: &str,
    active: Option<&Active>,
    current: Option<&Trigger>,
    now: i64,
) -> anyhow::Result<Option<Trigger>> {
    let Some(Active { schedule: Some(s), raw }) = active else { return Ok(None) };
    let fingerprint = digest(&raw.to_string());
    let same = |t: &Trigger| t.state["schedule"] == fingerprint;
    let state = |extra: Value| {
        let mut state = json!({ "name": name, "kind": kind, "schedule": fingerprint });
        if let (Value::Object(m), Value::Object(e)) = (&mut state, extra) {
            m.extend(e);
        }
        state
    };
    Ok(match kind {
        "every" => s.every.map(|every| {
            current.filter(|t| same(t)).cloned().unwrap_or_else(|| Trigger {
                at_ms: Some(next_every(millis(every), now)),
                types: Vec::new(),
                state: state(json!({})),
            })
        }),
        "cron" => match &s.cron {
            None => None,
            Some(cron) => match current.filter(|t| same(t)) {
                Some(t) => Some(t.clone()),
                None => {
                    Some(Trigger { at_ms: Some(next_cron(cron, now)?), types: Vec::new(), state: state(json!({})) })
                }
            },
        },
        // The `after` chain survives new versions: a new one neither starts
        // a second chain nor restarts the count.
        "after" => s.after.map(|_| {
            current.cloned().unwrap_or_else(|| Trigger {
                at_ms: Some(now),
                types: Vec::new(),
                state: state(json!({ "n": 1 })),
            })
        }),
        _ => s.on.as_ref().map(|on| {
            current.filter(|t| same(t)).cloned().unwrap_or_else(|| Trigger {
                at_ms: None,
                types: events::routes(on, &Args::new()),
                state: state(json!({ "events": [] })),
            })
        }),
    })
}

// ── 2. firing ──────────────────────────────────────────────────────────────

impl Runtime {
    /// Starts the runs whose slots are due now and moves each trigger to its
    /// next slot in the same commit. CHOICE: a slot missed while no worker
    /// ran starts once, late, and the next slot is the first after now, so a
    /// scheduler that was down does not start a burst. Scheduled runs get
    /// `{}` as input. Returns the runs it admitted. The worker calls it on
    /// every tick: when nothing is due it reads no rows.
    pub fn fire_due(&self) -> anyhow::Result<Vec<String>> {
        let now = self.clock.now_ms();
        let mut started = Vec::new();
        for (k, t) in self.store.due_triggers(now, BATCH)? {
            started.extend(self.fire(&k, &t, now)?);
        }
        Ok(started)
    }

    fn fire(&self, k: &str, t: &Trigger, now: i64) -> anyhow::Result<Option<String>> {
        let (Some(name), Some(kind), Some(slot)) = (t.state["name"].as_str(), t.state["kind"].as_str(), t.at_ms) else {
            anyhow::bail!("trigger {k} is not a schedule's");
        };
        let schedule = active(&*self.store, name)?.and_then(|a| a.schedule);
        let mut next = t.clone();
        let mut rearm = None;
        let id = match (kind, &schedule) {
            ("every", Some(Schedule { every: Some(every), .. })) => {
                next.at_ms = Some(next_every(millis(*every), slot.max(now)));
                format!("schedule:{name}:every:{slot}")
            }
            ("cron", Some(Schedule { cron: Some(cron), .. })) => {
                next.at_ms = Some(next_cron(cron, slot.max(now))?);
                format!("schedule:{name}:cron:{slot}")
            }
            ("after", Some(Schedule { after: Some(after), .. })) => {
                let n = t.state["n"].as_u64().unwrap_or(1);
                // Not due again until this run ends: the store re-arms it then.
                next.at_ms = None;
                next.state["n"] = json!(n + 1);
                rearm = Some(Rearm { trigger: k.to_owned(), delay_ms: millis(*after) });
                format!("schedule:{name}:after:{n:010}")
            }
            // The schedule no longer declares it (another version is active).
            _ => {
                self.store.swap_trigger(k, Some(t), None, &[], None, now)?;
                return Ok(None);
            }
        };
        let admission = self.admission(&id, name, json!({}))?;
        let fired = self.store.swap_trigger(k, Some(t), Some(&next), &[admission], rearm.as_ref(), now)?;
        Ok(fired.then_some(id))
    }
}

// ── 3. publishing ──────────────────────────────────────────────────────────

impl Runtime {
    /// Delivers a CloudEvent (at least `id`, `source` and `type`) to every
    /// run listening for it and to the event-triggered schedules it may
    /// start. `specversion` and `time` are filled in when absent.
    /// Publishing the same `source` + `id` again delivers nothing new.
    /// Returns the runs it reached or started.
    pub fn publish(&self, mut event: Value) -> anyhow::Result<Vec<String>> {
        let now = self.clock.now_ms();
        let Value::Object(e) = &mut event else { anyhow::bail!("an event is an object") };
        for k in ["id", "source", "type"] {
            anyhow::ensure!(e.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()), "an event needs `{k}`");
        }
        e.entry("specversion").or_insert_with(|| json!(events::SPEC_VERSION));
        e.entry("time").or_insert_with(|| {
            json!(chrono::DateTime::from_timestamp_millis(now).map(|t| t.to_rfc3339()).unwrap_or_default())
        });
        publish(&*self.store, &self.admitter(), &event, now)
    }
}

/// The runs a published event reaches, and the runs it starts through the
/// `on` schedules indexed under its type (runs and triggers in `store`,
/// definitions where the admitter reads them).
pub(crate) fn publish(
    store: &dyn RunStore,
    admitter: &Admitter,
    event: &Value,
    now: i64,
) -> anyhow::Result<Vec<String>> {
    let mut reached = store.publish(event, None, now)?;
    let t = event.get("type").and_then(Value::as_str).unwrap_or_default();
    for (k, trigger) in store.triggers_for(t, BATCH)? {
        reached.extend(offer(store, admitter, &k, trigger, event, now)?);
    }
    Ok(reached)
}

/// Offers the event to an `on` trigger: its matched events plus this one
/// go through the schedule's strategy again; a complete match starts the
/// workflow and empties the trigger in one commit, a partial one is kept.
fn offer(
    store: &dyn RunStore,
    admitter: &Admitter,
    k: &str,
    mut trigger: Trigger,
    event: &Value,
    now: i64,
) -> anyhow::Result<Option<String>> {
    let same = |a: &Value| a.get("id") == event.get("id") && a.get("source") == event.get("source");
    for _ in 0..16 {
        let name = trigger.state["name"].as_str().unwrap_or_default().to_owned();
        let Some(Schedule { on: Some(on), read, .. }) = active(&*admitter.definitions, &name)?.and_then(|a| a.schedule)
        else {
            return Ok(None);
        };
        let mut matched: Vec<Value> = trigger.state["events"].as_array().cloned().unwrap_or_default();
        if matched.iter().any(same) {
            return Ok(None);
        }
        matched.push(event.clone());
        let Some(outcome) = consume(&on, &matched)? else { return Ok(None) };
        let mut next = trigger.clone();
        let mut admit = Vec::new();
        let mut id = None;
        match outcome {
            Some(consumed) => {
                next.state["events"] = json!([]);
                let ids: Vec<String> = matched.iter().map(|e| format!("{}\u{0}{}", e["source"], e["id"])).collect();
                let run = format!("schedule:{name}:on:{}", digest(&ids.join("\u{1}")));
                // The same events again (republished): that run exists.
                if store.get(&run)?.is_some() {
                    return Ok(None);
                }
                let input: Vec<Value> = consumed.iter().map(|e| events::read(e, read)).collect();
                admit.push(admitter.active(&run, &name, Value::Array(input))?);
                id = Some(run);
            }
            None => next.state["events"] = Value::Array(matched),
        }
        if store.swap_trigger(k, Some(&trigger), Some(&next), &admit, None, now)? {
            return Ok(id);
        }
        match store.trigger(k)? {
            Some(t) => trigger = t,
            None => return Ok(None),
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
