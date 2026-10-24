//! Bounded linearizability over public observations and independent handles.
//! The reference states contain only the facts relevant to a batch; UUID owners
//! are represented by the operation that successfully acquired the generation.
use super::{Adapter, StoreDriver};
use ouro_durable::{Fence, RunStore, Trigger, conformance::admission};
use owf::run::Status;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Debug)]
enum Op {
    Take(String),
    Publish,
    Finish,
    Swap(usize),
    Claim,
    StaleSave,
}
#[derive(Clone, Debug, PartialEq)]
enum Reply {
    Empty,
    Value(Value),
    Bool(bool),
    Rejected,
}
#[derive(Clone, Debug)]
struct Completed {
    op: Op,
    begin: usize,
    end: usize,
    reply: Reply,
}
#[derive(Clone, Debug, Default, PartialEq)]
struct Abstract {
    terminal: bool,
    pending: bool,
    published: bool,
    consumed: Option<String>,
    records: BTreeMap<String, Value>,
    trigger: Option<usize>,
    admitted: Vec<usize>,
    claimed: bool,
}
impl Abstract {
    fn apply(&mut self, op: &Op) -> Reply {
        match op {
            Op::Take(key) => {
                if let Some(value) = self.records.get(key) {
                    Reply::Value(value.clone())
                } else if self.pending {
                    self.pending = false;
                    self.consumed = Some(key.clone());
                    self.records.insert(key.clone(), envelope());
                    Reply::Value(envelope())
                } else {
                    Reply::Empty
                }
            }
            Op::Publish => {
                if self.terminal {
                    Reply::Bool(false)
                } else {
                    self.pending = true;
                    self.published = true;
                    Reply::Bool(true)
                }
            }
            Op::Finish => {
                self.terminal = true;
                Reply::Bool(true)
            }
            Op::Swap(winner) => {
                if self.trigger.is_some() {
                    Reply::Bool(false)
                } else {
                    self.trigger = Some(*winner);
                    self.admitted.push(*winner);
                    Reply::Bool(true)
                }
            }
            Op::Claim => {
                if self.claimed {
                    Reply::Bool(false)
                } else {
                    self.claimed = true;
                    Reply::Bool(true)
                }
            }
            Op::StaleSave => Reply::Rejected,
        }
    }
}

// Explore every order extending response-before-invocation. Matching replies
// alone is insufficient: the final independent snapshot must also match.
fn linearizable(initial: &Abstract, history: &[Completed], final_state: &Abstract) -> bool {
    fn search(state: Abstract, history: &[Completed], used: &mut [bool], final_state: &Abstract) -> bool {
        if used.iter().all(|x| *x) {
            return state == *final_state;
        }
        for (index, event) in history.iter().enumerate() {
            if used[index] || history.iter().enumerate().any(|(j, other)| !used[j] && other.end < event.begin) {
                continue;
            }
            let mut next = state.clone();
            if next.apply(&event.op) != event.reply {
                continue;
            }
            used[index] = true;
            if search(next, history, used, final_state) {
                return true;
            }
            used[index] = false;
        }
        false
    }
    search(initial.clone(), history, &mut vec![false; history.len()], final_state)
}

fn envelope() -> Value {
    let mut event = ouro_durable::conformance::event("event", "event", json!(42));
    event["time"] = json!("1970-01-01T00:00:00+00:00");
    event
}
fn trigger(winner: usize) -> Trigger {
    Trigger { at_ms: Some(100), types: vec![], state: json!({"winner":winner}) }
}
fn execute(store: &dyn RunStore, fence: &Fence, op: &Op) -> Reply {
    match op {
        Op::Take(key) => match store.take_event(fence, key, &["event".into()], 0).unwrap() {
            Some(value) => Reply::Value(value),
            None => Reply::Empty,
        },
        Op::Publish => Reply::Bool(
            !store
                .publish(&json!({"source":"test","id":"event","type":"event","data":42}), None, 0)
                .unwrap()
                .is_empty(),
        ),
        Op::Finish => {
            assert_eq!(store.finish(fence, &Status::Completed(json!(42)), 0, 3).unwrap(), Status::Completed(json!(42)));
            Reply::Bool(true)
        }
        Op::Swap(winner) => Reply::Bool(
            store
                .swap_trigger("slot", None, Some(&trigger(*winner)), &[admission(&format!("slot-{winner}"))], None, 0)
                .unwrap(),
        ),
        Op::Claim => Reply::Bool(store.claim("batch", ouro_durable::LEASE_MS).unwrap().is_some()),
        Op::StaleSave => {
            let result = store.save_record(
                fence,
                "late",
                &owf::durable::Record::Timer { deadline_ms: 9 },
                ouro_durable::LEASE_MS,
            );
            if result.is_err() { Reply::Rejected } else { Reply::Bool(true) }
        }
    }
}
fn handles(driver: &StoreDriver, count: usize) -> Vec<Arc<dyn RunStore>> {
    (0..count)
        .map(|_| -> Arc<dyn RunStore> {
            match driver.adapter {
                Adapter::Sqlite => {
                    Arc::new(ouro_durable::sqlite::open(driver.directory.path().join("model.sqlite")).unwrap())
                }
                _ => driver.store.clone(),
            }
        })
        .collect()
}
fn batch(driver: &StoreDriver, fence: &Fence, ops: Vec<Op>) -> Vec<Completed> {
    let stores = handles(driver, ops.len());
    let barrier = Barrier::new(ops.len());
    let sequence = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let tasks: Vec<_> = ops
            .into_iter()
            .zip(stores)
            .map(|(op, store)| {
                let barrier = &barrier;
                let sequence = &sequence;
                scope.spawn(move || {
                    barrier.wait();
                    let begin = sequence.fetch_add(1, Ordering::SeqCst);
                    let reply = execute(store.as_ref(), fence, &op);
                    let end = sequence.fetch_add(1, Ordering::SeqCst);
                    Completed { op, begin, end, reply }
                })
            })
            .collect();
        tasks.into_iter().map(|task| task.join().unwrap()).collect()
    })
}
fn suite(adapter: Adapter) {
    for round in 0..12 {
        for scenario in 0..5 {
            let driver = StoreDriver::new(adapter).unwrap();
            let fence = driver.store.start(&admission("batch"), 0).unwrap().unwrap();
            let mut initial = Abstract::default();
            let ops = match scenario {
                0 | 1 => {
                    driver.store.register_wait(&fence, &["event".into()], 0).unwrap();
                    let event = ouro_durable::conformance::event("event", "event", json!(42));
                    assert_eq!(driver.store.deliver(&event, None, 0).unwrap(), Some("batch".into()));
                    initial.pending = true;
                    (0..4).map(|k| Op::Take(format!("take-{}", if scenario == 0 { k } else { 0 }))).collect()
                }
                2 => {
                    driver.store.subscribe(&fence, "listen", &["event".into()], 0).unwrap();
                    vec![Op::Publish, Op::Finish]
                }
                3 => vec![Op::Swap(0), Op::Swap(1)],
                _ => vec![Op::Claim, Op::Claim, Op::StaleSave],
            };
            let history = batch(&driver, &fence, ops);
            let audit = handles(&driver, 1).pop().unwrap();
            let snapshot = audit.inspect("batch").unwrap().unwrap();
            let mut actual = Abstract {
                terminal: snapshot.run.outcome.is_some(),
                pending: snapshot.events.iter().any(|e| e.consumed_key.is_none()),
                published: snapshot.events.iter().any(|e| e.envelope.is_some() && !e.delivered),
                consumed: snapshot.events.iter().find_map(|e| e.consumed_key.clone()),
                claimed: snapshot.run.generation > 1,
                ..Abstract::default()
            };
            for (key, record) in snapshot.records {
                match record {
                    owf::durable::Record::Event { event, .. } => {
                        actual.records.insert(key, event);
                    }
                    other => panic!("unexpected record: {other:?}"),
                }
            }
            if scenario == 3 {
                actual.trigger = audit
                    .trigger("slot")
                    .unwrap()
                    .map(|t| usize::try_from(t.state["winner"].as_u64().unwrap()).unwrap());
                actual.admitted = (0..2).filter(|k| audit.get(&format!("slot-{k}")).unwrap().is_some()).collect();
            }
            assert!(
                linearizable(&initial, &history, &actual),
                "round={round}, scenario={scenario}: {history:?}\nactual={actual:?}"
            );
        }
    }
}
#[test]
fn concurrent_memory() {
    suite(Adapter::Memory);
}
#[test]
fn concurrent_sqlite() {
    suite(Adapter::Sqlite);
}
#[test]
#[ignore = "requires TEST_DATABASE_URL with permission to create disposable databases"]
fn concurrent_postgres() {
    suite(Adapter::Postgres);
}

#[test]
fn checker_rejects_duplicate_acquisitions_and_impossible_real_time_order() {
    let initial = Abstract { pending: true, ..Abstract::default() };
    let history: Vec<_> = (0..2)
        .map(|k| Completed { op: Op::Take(format!("key{k}")), begin: k, end: k + 2, reply: Reply::Value(envelope()) })
        .collect();
    let mut final_state = initial.clone();
    final_state.apply(&history[1].op);
    assert!(!linearizable(&initial, &history, &final_state));
    let history = vec![
        Completed { op: Op::Finish, begin: 0, end: 1, reply: Reply::Bool(true) },
        Completed { op: Op::Publish, begin: 2, end: 3, reply: Reply::Bool(true) },
    ];
    let final_state = Abstract { terminal: true, pending: true, published: true, ..Abstract::default() };
    assert!(!linearizable(&Abstract::default(), &history, &final_state));
}
