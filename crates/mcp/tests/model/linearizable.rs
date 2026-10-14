// Linearizability: did a set of overlapping calls behave as if they ran one
// at a time, in some order that respects real time? A port of
// packages/mcp/model/linearizable.ts.
//
// The harness records, for every call made concurrently against the real
// server, when it was sent, when it was answered and what the answer was. The
// history is linearizable when some sequential order of the calls — where a
// call that finished before another started also comes first — makes the
// model give exactly those answers.
//
// One exception is allowed: a write that lost a compare-and-swap race may
// answer "raced" (a version_conflict without a current version) and change
// nothing.

use std::collections::HashSet;

use crate::answers::{Answer, model_answer, raced, same_answer};
use crate::calls::Call;
use crate::model::World;

#[derive(Debug, Clone)]
pub struct Event {
    pub who: String,
    pub call: Call,
    pub answer: Answer,
    pub start: f64,
    pub end: f64,
}

// Depth-first search over orders, remembering (calls done, world) pairs
// already explored. Exponential in the worst case; histories here are a few
// dozen calls at most.
pub fn linearizable(events: &[Event], start: &World) -> bool {
    assert!(events.len() <= 30, "a history of {} calls is too long to search", events.len());
    let all: u32 = if events.len() == 32 { u32::MAX } else { (1u32 << events.len()) - 1 };
    let mut seen: HashSet<(u32, String)> = HashSet::new();
    go(events, all, 0, start, &mut seen)
}

fn go(events: &[Event], all: u32, done: u32, w: &World, seen: &mut HashSet<(u32, String)>) -> bool {
    if done == all {
        return true;
    }
    if !seen.insert((done, format!("{w:?}"))) {
        return false;
    }
    let pending: Vec<usize> = (0..events.len()).filter(|i| done & (1 << i) == 0).collect();
    // Nothing that started after the earliest pending answer can go first.
    let horizon = pending.iter().map(|i| events[*i].end).fold(f64::INFINITY, f64::min);
    for i in pending {
        let e = &events[i];
        if e.start > horizon {
            continue;
        }
        let next = if same_answer(&e.answer, &raced()) {
            Some(w.clone())
        } else {
            let (w2, answer) = model_answer(w, &e.call);
            same_answer(&answer, &e.answer).then_some(w2)
        };
        if let Some(next) = next
            && go(events, all, done | (1 << i), &next, seen)
        {
            return true;
        }
    }
    false
}
