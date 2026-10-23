//! Published events and schedules: 1. publishing an event to the runs
//! listening for it and to event-triggered schedules (`schedule.on`);
//! 2. the runs `every`, `cron` and `after` owe, and the trigger runs behind
//! `on`; 3. the loop a host starts.
//!
//! Every start is idempotent: a run's id is derived from its slot, its
//! trigger run or the predecessor that owes it, and admission commits once
//! per id (a compare-and-swap in the store), so schedulers in several
//! processes start each run once.
//!
//! `schedule.on` is served by trigger runs: a run (named `<name>#on`) of a
//! generated definition that only listens with the schedule's strategy, so
//! filters, correlation, `all` and `until` mean what they mean in a
//! `listen`. Publishing hands each matching event to the open trigger run,
//! subscribed yet or not; once it completes, the scheduler starts the
//! workflow on the events it consumed (`read` applied) and opens the next
//! trigger run, moving over the events the previous one left unconsumed.
use crate::{Runtime, execution::Admitter};
use ouro_durable::{Definition, RunStore};
use owf::{
    events,
    model::{self, Schedule},
    run::Status,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The schedule and parsed document of each active definition that has one.
struct Scheduled {
    definition: Definition,
    schedule: Schedule,
    /// `schedule` as written, which identifies a trigger generation.
    raw: Value,
    document: Value,
}

fn scheduled(store: &dyn RunStore) -> anyhow::Result<Vec<Scheduled>> {
    let mut out = Vec::new();
    for name in store.definition_names()? {
        let definitions = store.definitions(&name)?;
        let Some(d) = definitions.active.and_then(|v| definitions.versions.get(&v)) else { continue };
        // A definition that no longer parses has no schedule to honour.
        let Ok(wf) = model::parse_yaml(&d.yaml) else { continue };
        if let Some(schedule) = wf.schedule {
            out.push(Scheduled {
                definition: d.clone(),
                schedule,
                raw: wf.definition["schedule"].clone(),
                document: wf.definition["document"].clone(),
            });
        }
    }
    Ok(out)
}

// ── 1. publishing ──────────────────────────────────────────────────────────

/// Delivers a CloudEvent to every run subscribed to it and to the open
/// trigger run of every `schedule.on` it may match. Idempotent per
/// `source` + `id`. Returns the receiving runs.
pub(crate) fn publish(
    store: &dyn RunStore,
    admitter: &Admitter,
    event: &Value,
    now: i64,
) -> anyhow::Result<Vec<String>> {
    let mut receivers = store.publish(event, None, now)?;
    let t = event.get("type").and_then(Value::as_str).unwrap_or_default();
    for s in scheduled(&*admitter.definitions)? {
        let Some(on) = &s.schedule.on else { continue };
        let routes = events::routes(on, &owf::expr::Args::new());
        if !routes.iter().any(|r| r == "*" || r == t) {
            continue;
        }
        let trigger = open_trigger(store, admitter, &s, now)?;
        receivers.extend(store.publish(event, Some(&trigger), now)?);
    }
    receivers.sort();
    receivers.dedup();
    Ok(receivers)
}

/// FNV-1a: a stable digest of a schedule, the same in every process.
fn digest(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{hash:016x}")
}

fn trigger_name(name: &str) -> String {
    format!("{name}#on")
}

fn trigger_prefix(s: &Scheduled) -> String {
    format!("schedule:{}:on:{}:", s.definition.name, digest(&s.raw.to_string()))
}

/// The trigger runs of this schedule generation, by sequence number.
fn triggers(store: &dyn RunStore, s: &Scheduled) -> anyhow::Result<Vec<(u64, ouro_durable::SavedRun)>> {
    let prefix = trigger_prefix(s);
    let mut runs: Vec<_> = store
        .list(Some(&trigger_name(&s.definition.name)), &prefix)?
        .into_iter()
        .filter_map(|r| Some((r.admission.id.strip_prefix(&prefix)?.parse::<u64>().ok()?, r)))
        .collect();
    runs.sort_by_key(|(n, _)| *n);
    Ok(runs)
}

/// The id of the open trigger run, admitting the next one when none is open.
fn open_trigger(store: &dyn RunStore, admitter: &Admitter, s: &Scheduled, now: i64) -> anyhow::Result<String> {
    let runs = triggers(store, s)?;
    if let Some((_, open)) = runs.iter().find(|(_, r)| r.ended_ms.is_none()) {
        return Ok(open.admission.id.clone());
    }
    let n = runs.last().map_or(1, |(n, _)| n + 1);
    let id = format!("{}{n:010}", trigger_prefix(s));
    let mut listen = json!({ "to": s.raw["on"] });
    if let Some(read) = s.raw.get("read") {
        listen["read"] = read.clone();
    }
    let definition = json!({
        "document": {
            "dsl": s.document["dsl"],
            "namespace": s.document["namespace"],
            "name": s.document["name"],
            "version": s.document["version"],
        },
        "do": [ { "trigger": { "listen": listen } } ],
    });
    let generated = Definition {
        name: trigger_name(&s.definition.name),
        version: s.definition.version,
        yaml: serde_json::to_string(&definition)?,
    };
    // Admission compares and sets on the id: a concurrent scheduler that got
    // there first leaves this one a no-op.
    store.admit(&admitter.of(&id, &generated, json!({}))?, now)?;
    Ok(id)
}

// ── 2. owed runs ───────────────────────────────────────────────────────────

impl Runtime {
    /// Starts the runs schedules owe for the time in `(from_ms, to_ms]`
    /// and serves trigger runs. CHOICE: `every` slots are aligned to the
    /// epoch and `cron` (UTC) slots are its occurrences; only the latest
    /// slot in the window starts, so a scheduler that was down does not
    /// start a burst of missed runs. The first `after` run starts when the
    /// definition is first seen active, each next one `after` the previous
    /// ended (in any terminal state). Scheduled runs get `{}` as input; a
    /// workflow started by `on` gets the array of consumed events.
    /// Returns the runs it admitted.
    pub fn schedule(&self, from_ms: i64, to_ms: i64) -> anyhow::Result<Vec<String>> {
        // Runs are admitted at the clock's time, due at once, whatever window
        // they were owed for.
        let now = self.clock.now_ms();
        let mut started = Vec::new();
        let mut admit = |id: String, name: &str, input: Value| -> anyhow::Result<()> {
            if self.store.get(&id)?.is_none() && self.store.admit(&self.admission(&id, name, input)?, now)? {
                started.push(id);
            }
            Ok(())
        };
        let all = scheduled(&*self.store)?;
        let mut triggered = Vec::new();
        for s in &all {
            let name = &s.definition.name;
            if let Some(every) = s.schedule.every {
                let every = i64::try_from(every.as_millis()).unwrap_or(i64::MAX).max(1);
                let slot = to_ms - to_ms.rem_euclid(every);
                if slot > from_ms {
                    admit(format!("schedule:{name}:every:{slot}"), name, json!({}))?;
                }
            }
            if let Some(cron) = &s.schedule.cron {
                let cron: croner::Cron = cron.parse()?;
                let at =
                    chrono::DateTime::from_timestamp_millis(to_ms).ok_or_else(|| anyhow::anyhow!("invalid time"))?;
                let slot = cron.find_previous_occurrence(&at, true)?.timestamp_millis();
                if slot > from_ms {
                    admit(format!("schedule:{name}:cron:{slot}"), name, json!({}))?;
                }
            }
            if let Some(after) = s.schedule.after {
                let prefix = format!("schedule:{name}:after:");
                let last = self
                    .store
                    .list(Some(name), &prefix)?
                    .into_iter()
                    .filter_map(|r| Some((r.admission.id.strip_prefix(&prefix)?.parse::<u64>().ok()?, r)))
                    .max_by_key(|(n, _)| *n);
                let after = i64::try_from(after.as_millis()).unwrap_or(i64::MAX);
                match last {
                    None => admit(format!("{prefix}{:010}", 1), name, json!({}))?,
                    Some((n, r)) if r.ended_ms.is_some_and(|ended| ended.saturating_add(after) <= to_ms) => {
                        admit(format!("{prefix}{:010}", n + 1), name, json!({}))?
                    }
                    Some(_) => {}
                }
            }
            if s.schedule.on.is_some() {
                triggered.extend(self.serve_triggers(s, now)?);
            }
        }
        started.extend(triggered);
        // A trigger run whose schedule changed or went away stops listening.
        let current: Vec<String> = all.iter().filter(|s| s.schedule.on.is_some()).map(trigger_prefix).collect();
        for r in self.store.list(None, "schedule:")? {
            if r.ended_ms.is_none()
                && r.admission.name.ends_with("#on")
                && !current.iter().any(|p| r.admission.id.starts_with(p))
            {
                self.store.cancel(&r.admission.id, now)?;
            }
        }
        Ok(started)
    }

    /// Starts the workflow for each completed trigger run, keeps one trigger
    /// run open and moves to it what the previous one left unconsumed.
    fn serve_triggers(&self, s: &Scheduled, now: i64) -> anyhow::Result<Vec<String>> {
        let name = &s.definition.name;
        let mut started = Vec::new();
        for (_, r) in triggers(&*self.store, s)? {
            let Some(Status::Completed(consumed)) = &r.outcome else { continue };
            let id = format!("{}:run", r.admission.id);
            if self.store.get(&id)?.is_none() && self.store.admit(&self.admission(&id, name, consumed.clone())?, now)? {
                started.push(id);
            }
        }
        let admitter = self.admitter();
        let open = open_trigger(&*self.store, &admitter, s, now)?;
        let runs = triggers(&*self.store, s)?;
        if let Some((_, previous)) = runs.iter().rev().find(|(_, r)| r.ended_ms.is_some())
            && let Some(snapshot) = self.store.inspect(&previous.admission.id)?
        {
            for e in snapshot.events.iter().filter(|e| e.consumed_key.is_none()) {
                if let Some(envelope) = &e.envelope {
                    self.store.publish(envelope, Some(&open), now)?;
                }
            }
        }
        Ok(started)
    }

    /// Delivers a CloudEvent (at least `id`, `source` and `type`) to every
    /// run listening for it and to the event-triggered schedules it may
    /// start. `specversion` and `time` are filled in when absent.
    /// Publishing the same `source` + `id` again delivers nothing new.
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

    // ── 3. the loop ────────────────────────────────────────────────────────

    /// Run on host startup, next to `worker`: every poll interval it starts
    /// what schedules owe since the previous tick (from its own start).
    pub async fn scheduler(&self, stop: CancellationToken) -> anyhow::Result<()> {
        let mut from = self.clock.now_ms();
        let mut tick = tokio::time::interval(self.config.poll_interval.max(Duration::from_millis(1)));
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    let to = self.clock.now_ms();
                    if to > from {
                        self.schedule(from, to)?;
                        from = to;
                    }
                }
            }
        }
    }
}
