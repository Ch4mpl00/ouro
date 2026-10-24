//! Bounded linearizability against the contract model. Each batch of
//! `spec/contract_concurrent.qnt` names a setup history and commands that run
//! at once; the model lists, for every order of the commands, each command's
//! reply and the state after all of them. Here the setup runs through the
//! serial bridge, the commands start together at a barrier on independent
//! store handles (independent SQLite connections), and some order that
//! respects real time (no command placed before one that ended before it
//! began) must match every reply and a fresh projection of the store.
use super::{
    Adapter, CALLS, Command, INGRESS, NAME, SCHEDULE, StoreDriver, TICK, View, call_record, event, ingress_event,
    run_id, schedule_trigger, status, worker,
};
use anyhow::{Context, ensure};
use ouro_durable::{Admission, Arming, Fence, Fired, Firing, IdentityConflict, Rearm, RunStore, Trigger};
use owf::durable::CallState;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Debug, Deserialize)]
struct Cmd {
    op: String,
    r: usize,
    w: usize,
    i: usize,
    value: i64,
}
#[derive(Deserialize)]
struct Order {
    order: Vec<usize>,
    replies: Vec<i64>,
    #[serde(rename = "final")]
    end: View,
}
#[derive(Deserialize)]
struct Batch {
    name: String,
    setup: Vec<Cmd>,
    commands: Vec<Cmd>,
    orders: Vec<Order>,
}
#[derive(Clone, Debug)]
struct Completed {
    begin: usize,
    end: usize,
    reply: i64,
}

/// ITF's encodings of big integers, sets, tuples and maps as plain JSON.
fn plain(v: Value) -> Value {
    match v {
        Value::Object(mut m) => {
            if let Some(n) = m.get("#bigint").and_then(Value::as_str) {
                return json!(n.parse::<i64>().unwrap());
            }
            if let Some(Value::Array(items)) = m.remove("#set").or_else(|| m.remove("#tup")) {
                return Value::Array(items.into_iter().map(plain).collect());
            }
            if let Some(Value::Array(pairs)) = m.remove("#map") {
                let mut out = serde_json::Map::new();
                for pair in pairs {
                    let [k, v]: [Value; 2] = serde_json::from_value(pair).unwrap();
                    out.insert(plain(k).to_string(), plain(v));
                }
                return Value::Object(out);
            }
            Value::Object(m.into_iter().map(|(k, v)| (k, plain(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        other => other,
    }
}
/// The model's batches with their sequential answers (`quint` on PATH).
fn table() -> anyhow::Result<Vec<Batch>> {
    let directory = tempfile::tempdir()?;
    let out = directory.path().join("table.itf.json");
    let status = std::process::Command::new("quint")
        .arg("run")
        .arg(super::spec_path("contract_concurrent.qnt"))
        .args(["--init", "tableInit", "--step", "tableStep", "--max-samples", "1", "--max-steps", "1"])
        .args(["--seed", "1", "--verbosity", "0", "--out-itf"])
        .arg(&out)
        .status()?;
    ensure!(status.success(), "quint could not compute the batches");
    let itf: Value = serde_json::from_str(&std::fs::read_to_string(out)?)?;
    Ok(serde_json::from_value(plain(itf["states"][0]["table"].clone()))?)
}

/// What a racer needs that the serial bridge reads beforehand: the workers'
/// fences, admissions and the trigger a firing expects (a stale one loses
/// its compare-and-swap, as the model's later order would).
struct Prepared {
    now: i64,
    fences: Vec<Option<Fence>>,
    admissions: BTreeMap<usize, Admission>,
    schedule: Option<Trigger>,
    ingress: Option<Trigger>,
    epoch: u64,
}
impl Prepared {
    fn new(d: &StoreDriver, commands: &[Cmd]) -> anyhow::Result<Self> {
        let defs = d.store.definitions(NAME)?;
        let mut admissions = BTreeMap::new();
        for (k, c) in commands.iter().enumerate() {
            let slot = (c.op == "fire").then(|| -> anyhow::Result<(u64, i64)> {
                let t = d.store.trigger(SCHEDULE)?.context("schedule trigger")?;
                Ok((defs.epoch, t.state["slot"].as_i64().context("slot")?))
            });
            if matches!(c.op.as_str(), "fire" | "ingress") {
                admissions.insert(k, d.admission(c.r, c.value, 0, 0, slot.transpose()?)?);
            }
        }
        Ok(Self {
            now: d.ms(),
            fences: d.workers.iter().map(|x| x.fence.clone()).collect(),
            admissions,
            schedule: d.store.trigger(SCHEDULE)?,
            ingress: d.store.trigger(INGRESS)?,
            epoch: defs.epoch,
        })
    }
    fn fence(&self, w: usize) -> &Fence {
        self.fences[w].as_ref().expect("the batch's worker holds a fence")
    }
}

/// One command against one handle, answered as the model's `reply` reads it.
fn race(store: &dyn RunStore, p: &Prepared, k: usize, c: &Cmd) -> (i64, Option<Fence>) {
    let ms = p.now;
    let accepted = |result: anyhow::Result<Vec<String>>| match result {
        Ok(receivers) => i64::from(!receivers.is_empty()),
        Err(e) if e.is::<IdentityConflict>() => -1,
        Err(e) => panic!("{e}"),
    };
    let reply = match c.op.as_str() {
        "take" => store
            .take_event(p.fence(c.w), &format!("event{}", c.i), &["event".into()], ms)
            .unwrap()
            .map_or(-1, |e| e["data"].as_i64().unwrap()),
        "address" => accepted(store.publish(&event(c.i, c.value), Some(&run_id(c.r)), ms)),
        "publish" => accepted(store.publish(&event(c.i, c.value), None, ms)),
        "finish" => i64::from(store.finish(p.fence(c.w), &status(c.value + 1), ms, 2).is_ok()),
        "intent" => {
            let record = call_record(c.i, CallState::Pending);
            i64::from(store.save_record(p.fence(c.w), &format!("call{}", c.i), &record, ms).is_ok())
        }
        "claim" => {
            let fence = store.claim_as(&run_id(c.r), ms, &worker(c.w)).unwrap();
            return (i64::from(fence.is_some()), fence);
        }
        "fire" => {
            let Some(t) = p.schedule.as_ref().filter(|t| t.at_ms.is_some_and(|at| at <= ms)) else { return (0, None) };
            let slot = t.state["slot"].as_i64().unwrap();
            let (catchup, rearm) = (t.state["catchup"].as_i64().unwrap(), t.state["rearm"].as_bool().unwrap());
            let at = |at: Option<i64>| schedule_trigger(at, p.epoch, slot + 1, catchup, rearm);
            let (next, busy) = (at(if rearm { None } else { Some(ms / TICK + 2) }), at(Some(ms / TICK + 2)));
            let again = Rearm { trigger: SCHEDULE.into(), delay_ms: 2 * TICK };
            let firing = Firing {
                key: SCHEDULE,
                expected: Some(t),
                next: Some(&next),
                admit: std::slice::from_ref(&p.admissions[&k]),
                rearm: rearm.then_some(&again),
                ingress: None,
                busy: Some(&busy),
                refused: None,
                overlap: None,
            };
            i64::from(store.fire(&firing, ms).unwrap() == Fired::Committed)
        }
        "ingress" => {
            let e = ingress_event(c.i, c.value);
            match store.ingested(&e, ms) {
                Ok(false) => {}
                Ok(true) => return (0, None),
                Err(e) if e.is::<IdentityConflict>() => return (0, None),
                Err(e) => panic!("{e}"),
            }
            let t = p.ingress.as_ref().unwrap();
            let firing = Firing {
                key: INGRESS,
                expected: Some(t),
                next: Some(t),
                admit: std::slice::from_ref(&p.admissions[&k]),
                ingress: Some(&e),
                ..Firing::default()
            };
            i64::from(store.fire(&firing, ms).unwrap() == Fired::Committed)
        }
        "activate" => {
            let version = c.i as u64 + 1;
            let (catchup, rearm) = (c.value % 2, c.value >= 2);
            let mut arming = Arming::default();
            if version == 1 {
                arming
                    .triggers
                    .insert(SCHEDULE.into(), schedule_trigger(Some(ms / TICK + 2), p.epoch + 1, 0, catchup, rearm));
            }
            i64::from(store.activate_with(NAME, version, p.epoch, &arming).unwrap().is_some())
        }
        other => panic!("no concurrent form for {other}"),
    };
    (reply, None)
}

/// Whether some order of the model's matches: it respects real time, its
/// replies are the observed ones and its final state is the observed one.
fn linearizable<F: PartialEq>(orders: &[(&[usize], &[i64], F)], history: &[Completed], actual: &F) -> bool {
    orders.iter().any(|(order, replies, end)| {
        let timely =
            order.iter().enumerate().all(|(a, x)| order[a + 1..].iter().all(|y| history[*y].end > history[*x].begin));
        timely && history.iter().zip(replies.iter()).all(|(h, r)| h.reply == *r) && end == actual
    })
}

fn run(adapter: Adapter, batch: &Batch, round: usize) -> anyhow::Result<()> {
    let mut driver = StoreDriver::model(adapter)?;
    driver.initialize()?;
    for c in &batch.setup {
        driver.execute(&Command { op: c.op.clone(), r: c.r, w: c.w, i: c.i, value: c.value, fault: 0 })?;
    }
    let prepared = Prepared::new(&driver, &batch.commands)?;
    let stores = (0..batch.commands.len()).map(|_| driver.handle()).collect::<anyhow::Result<Vec<_>>>()?;
    let barrier = Barrier::new(batch.commands.len());
    let sequence = AtomicUsize::new(0);
    let results: Vec<(Completed, Option<Fence>)> = std::thread::scope(|scope| {
        let tasks: Vec<_> = batch
            .commands
            .iter()
            .enumerate()
            .zip(stores)
            .map(|((k, c), store)| {
                let (barrier, sequence, prepared) = (&barrier, &sequence, &prepared);
                scope.spawn(move || {
                    barrier.wait();
                    let begin = sequence.fetch_add(1, Ordering::SeqCst);
                    let (reply, fence) = race(store.as_ref(), prepared, k, c);
                    let end = sequence.fetch_add(1, Ordering::SeqCst);
                    (Completed { begin, end, reply }, fence)
                })
            })
            .collect();
        tasks.into_iter().map(|task| task.join().unwrap()).collect()
    });
    for ((_, fence), c) in results.iter().zip(&batch.commands) {
        if let Some(f) = fence {
            driver.owners.insert(f.owner.clone(), c.w as i64);
        }
    }
    // A fresh handle reads the outcome back.
    driver.store = driver.handle()?;
    let actual = <View as quint_connect::State<StoreDriver>>::from_driver(&driver)?;
    let history: Vec<_> = results.into_iter().map(|(h, _)| h).collect();
    let orders: Vec<_> = batch.orders.iter().map(|o| (o.order.as_slice(), o.replies.as_slice(), &o.end)).collect();
    ensure!(
        linearizable(&orders, &history, &&actual),
        "{} (round {round}): no order of the model's explains {history:?}\nactual: {actual:?}",
        batch.name
    );
    Ok(())
}
fn suite(adapter: Adapter) -> anyhow::Result<()> {
    let batches = table()?;
    ensure!(batches.len() == 8 && CALLS == 3, "the model's batches changed");
    for round in 0..12 {
        for batch in &batches {
            run(adapter, batch, round)?;
        }
    }
    Ok(())
}
#[test]
fn concurrent_memory() {
    super::with_quint("concurrent::concurrent_memory", || suite(Adapter::Memory));
}
#[test]
fn concurrent_sqlite() {
    super::with_quint("concurrent::concurrent_sqlite", || suite(Adapter::Sqlite));
}
#[test]
#[ignore = "requires TEST_DATABASE_URL with permission to create disposable databases"]
fn concurrent_postgres() {
    super::with_quint("concurrent::concurrent_postgres", || suite(Adapter::Postgres));
}

#[test]
fn checker_rejects_duplicate_acquisitions_and_impossible_real_time_order() {
    // Two takes of one copy under distinct keys: the model gives it to one.
    let orders = [(&[0, 1][..], &[4, -1][..], 0), (&[1, 0][..], &[-1, 4][..], 0)];
    let both: Vec<_> = (0..2).map(|k| Completed { begin: k, end: k + 2, reply: 4 }).collect();
    assert!(!linearizable(&orders, &both, &0));
    let one = vec![Completed { begin: 0, end: 2, reply: 4 }, Completed { begin: 1, end: 3, reply: -1 }];
    assert!(linearizable(&orders, &one, &0));
    // A publish that began after the finish ended cannot have reached the run.
    let orders = [(&[0, 1][..], &[1, 1][..], 0), (&[1, 0][..], &[0, 1][..], 1)];
    let late = vec![Completed { begin: 2, end: 3, reply: 1 }, Completed { begin: 0, end: 1, reply: 1 }];
    assert!(!linearizable(&orders, &late, &0));
    assert!(!linearizable(&orders, &late, &1));
}
