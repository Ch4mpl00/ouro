// Linearizability: did a set of overlapping calls behave as if they ran one
// at a time, in some order that respects real time?
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

import { canonical, modelAnswer, RACED, sameAnswer, type Answer } from "./answers";
import type { Call } from "./calls";
import type { World } from "./model";

export interface Event {
  call: Call;
  answer: Answer;
  start: number;
  end: number;
}

const worldKey = (w: World): string => canonical([...w].sort(([a], [b]) => (a < b ? -1 : 1)));

// Depth-first search over orders, remembering (calls done, world) pairs
// already explored. Exponential in the worst case; histories here are a few
// dozen calls at most.
export function linearizable(events: readonly Event[], start: World): boolean {
  if (events.length > 30) throw new Error(`a history of ${events.length} calls is too long to search`);
  const all = 2 ** events.length - 1;
  const seen = new Set<string>();

  const go = (done: number, w: World): boolean => {
    if (done === all) return true;
    const key = `${done}|${worldKey(w)}`;
    if (seen.has(key)) return false;
    seen.add(key);

    const pending = events.map((_, i) => i).filter((i) => (done & (1 << i)) === 0);
    // Nothing that started after the earliest pending answer can go first.
    const horizon = Math.min(...pending.map((i) => events[i]!.end));
    for (const i of pending) {
      const e = events[i]!;
      if (e.start > horizon) continue;
      let next: World | null;
      if (sameAnswer(e.answer, RACED)) next = w;
      else {
        const [w2, answer] = modelAnswer(w, e.call);
        next = sameAnswer(answer, e.answer) ? w2 : null;
      }
      if (next !== null && go(done | (1 << i), next)) return true;
    }
    return false;
  };

  return go(0, start);
}
