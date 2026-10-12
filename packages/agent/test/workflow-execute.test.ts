// The workflow executor under generated plans and faults. Every leaf step
// carries a marker; the fake engine looks the marker up in a generated
// script (succeed or fail, after a delay, honouring the abort signal or not)
// and logs when each call starts and ends. The rules then say, from the plan
// and the script alone, which calls may run, in what order, what the result
// is, what the store holds — and that nothing is still running, or still
// writing, once execute has returned.

import fc from "fast-check";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { describe, expect, it } from "vitest";
import { z } from "zod";
import type { Decision, DecideRequest, DecisionClient } from "../src/decisions";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { nullTracer } from "../src/tracing";
import type { ExecFailureReason } from "../src/workflow/execute";
import { createExecutor, type EngineSurface } from "../src/workflow/execute";
import type { LeafStep, Step, Workflow } from "../src/workflow/dsl";
import { createStore } from "../src/workflow/variables";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 150);

// ── plans and scripts ────────────────────────────────────────────────────────

type Kind = "tool" | "llm_compose" | "llm_agent" | "code_agent" | "decide";
interface Script {
  fail: boolean;
  delayMs: number;
  // Whether the call stops when its abort signal fires.
  honoursAbort: boolean;
}
interface Leaf {
  kind: Kind;
  marker: string;
  bind: string | undefined;
  script: Script;
}
type Item = { kind: "leaf"; leaf: Leaf } | { kind: "parallel"; leaves: Leaf[] } | { kind: "terminal" } | { kind: "replan"; context: string[] };

const script = fc.record({
  fail: fc.oneof({ weight: 3, arbitrary: fc.constant(false) }, { weight: 1, arbitrary: fc.constant(true) }),
  delayMs: fc.constantFrom(0, 0, 1, 3, 8),
  honoursAbort: fc.boolean(),
});
const kind = fc.constantFrom<Kind>("tool", "llm_compose", "llm_agent", "code_agent", "decide");
const leafShape = fc.record({ kind, script, bound: fc.boolean() });
const itemShape = fc.oneof(
  { weight: 6, arbitrary: leafShape.map((l) => ({ kind: "leaf" as const, l })) },
  { weight: 2, arbitrary: fc.array(leafShape, { minLength: 2, maxLength: 5 }).map((ls) => ({ kind: "parallel" as const, ls })) },
  { weight: 1, arbitrary: fc.constant({ kind: "terminal" as const }) },
  { weight: 1, arbitrary: fc.constant({ kind: "replan" as const }) },
);

// Markers and binds are unique per plan; a replan carries every bind so far
// plus one that was never bound.
const plan: fc.Arbitrary<Item[]> = fc.array(itemShape, { minLength: 1, maxLength: 6 }).map((shapes) => {
  let n = 0;
  const leaf = (l: { kind: Kind; script: Script; bound: boolean }): Leaf => {
    n++;
    return { kind: l.kind, marker: `m${n}`, bind: l.bound || l.kind !== "tool" ? `b${n}` : undefined, script: l.script };
  };
  const binds: string[] = [];
  return shapes.map((s): Item => {
    if (s.kind === "leaf") {
      const x = leaf(s.l);
      if (x.bind) binds.push(x.bind);
      return { kind: "leaf", leaf: x };
    }
    if (s.kind === "parallel") {
      const leaves = s.ls.map(leaf);
      for (const x of leaves) if (x.bind) binds.push(x.bind);
      return { kind: "parallel", leaves };
    }
    if (s.kind === "terminal") return { kind: "terminal" };
    return { kind: "replan", context: [...binds, "never_bound"] };
  });
});

function toStep(l: Leaf): LeafStep {
  const bind = l.bind ?? "";
  switch (l.kind) {
    case "tool":
      return l.bind === undefined ? { kind: "tool", tool: "t", args: { q: l.marker } } : { kind: "tool", tool: "t", args: { q: l.marker }, bind };
    case "llm_compose":
      return { kind: "llm_compose", preset: "base", prompt: l.marker, input: {}, bind };
    case "llm_agent":
      return { kind: "llm_agent", preset: "base", skill: "s", prompt: l.marker, tools: ["t"], maxIterations: 2, bind };
    case "code_agent":
      return { kind: "code_agent", task: l.marker, bind };
    case "decide":
      return { kind: "decide", preset: "filter", items: "${list}", prompt: l.marker, bind };
  }
}

function toWorkflow(items: Item[]): Workflow {
  return {
    version: 1,
    steps: items.map((i): Step => {
      switch (i.kind) {
        case "leaf":
          return toStep(i.leaf);
        case "parallel":
          return { kind: "parallel", steps: i.leaves.map(toStep) };
        case "terminal":
          return { kind: "terminal" };
        case "replan":
          return { kind: "replan", context: i.context };
      }
    }),
  };
}

const REASON: Record<Kind, ExecFailureReason> = {
  tool: "tool_error",
  llm_compose: "llm_error",
  llm_agent: "step_failed",
  code_agent: "step_failed",
  decide: "decision_error",
};

// ── the scripted engine ──────────────────────────────────────────────────────

interface Event {
  at: number;
  marker: string;
  what: "start" | "ok" | "fail" | "aborted" | "loop-start" | "loop-end";
}

function scriptedEngine(scripts: ReadonlyMap<string, Script>) {
  const events: Event[] = [];
  let clock = 0;
  const log = (marker: string, what: Event["what"]) => events.push({ at: clock++, marker, what });
  const find = (text: string): string => {
    const m = /\bm\d+\b/.exec(text);
    if (!m) throw new Error(`no marker in ${JSON.stringify(text).slice(0, 200)}`);
    return m[0];
  };
  // Runs one scripted call: waits its delay (cut short by the signal when the
  // call honours it), then succeeds or fails.
  async function perform<T>(marker: string, signal: AbortSignal | undefined, ok: () => T, fail: () => T | never): Promise<T> {
    const s = scripts.get(marker);
    if (!s) throw new Error(`no script for ${marker}`);
    log(marker, "start");
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(resolve, s.delayMs);
      if (s.honoursAbort && signal) {
        const onAbort = () => {
          clearTimeout(timer);
          log(marker, "aborted");
          reject(signal.reason);
        };
        if (signal.aborted) onAbort();
        else signal.addEventListener("abort", onAbort, { once: true });
      }
    });
    if (s.fail) {
      log(marker, "fail");
      return fail();
    }
    log(marker, "ok");
    return ok();
  }

  const provider: ChatProvider = {
    kind: "openai",
    complete: (params) =>
      perform(
        find(params.messages.map((m) => (typeof m.content === "string" ? m.content : "")).join(" ")),
        params.signal,
        () => ({ message: { role: "assistant" as const, content: "composed", refusal: null }, finishReason: "stop" }),
        () => {
          throw new Error("provider: 500");
        },
      ),
  };
  const FilterState = z.object({ rules: z.string(), items: z.array(z.object({ n: z.number(), item: z.unknown() })) });
  const decisions: DecisionClient = {
    model: "fake",
    async decide<A extends Record<string, unknown>>(req: DecideRequest<A>): Promise<Decision<A>> {
      const state = FilterState.parse(req.state);
      const decision: Decision<A> = { model: "fake", answer: (key) => req.questions[key].parse({ type: "noul", noul: 1 }) };
      return perform(find(state.rules), req.signal, () => decision, () => {
        throw new Error("decisions: 503");
      });
    },
  };
  const engine: EngineSurface = {
    presets: DEFAULT_PRESETS,
    resolveProvider: () => provider,
    mcp: {
      callTool: (_name, args, options) =>
        perform(find(JSON.stringify(args)), options?.signal, () => '{"ok":true}', () => "[tool error] boom"),
    },
    startAgentLoop: async (opts) => {
      const messages: ChatCompletionMessageParam[] = [];
      const id = opts.id;
      log(id, "loop-start");
      return {
        messages,
        run: () => perform(find(messages.map((m) => (typeof m.content === "string" ? m.content : "")).join(" ")), opts.signal, () => "answer", () => {
          throw new Error("agent crashed");
        }),
      };
    },
    endAgentLoop: (id) => void log(id, "loop-end"),
    decisions,
  };
  const codex = {
    run: (req: { prompt: string }, options?: { signal?: AbortSignal }) =>
      perform(find(req.prompt), options?.signal, () => ({ ok: true as const, content: " result ", stderr: "" }), () => {
        throw new Error("codex: exit 1");
      }),
  };
  return { engine, codex, events };
}

// ── the property ─────────────────────────────────────────────────────────────

describe("workflow executor under generated plans and faults", () => {
  it("runs what the plan allows, stops where it must, and leaves nothing running", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(plan, async (items) => {
        const leaves = items.flatMap((i) => (i.kind === "leaf" ? [i.leaf] : i.kind === "parallel" ? i.leaves : []));
        const { engine, codex, events } = scriptedEngine(new Map(leaves.map((l) => [l.marker, l.script])));
        const executor = createExecutor({ engine, readSkill: async () => "skill", setMemory: () => {}, codex });
        const store = createStore({ list: [1, 2] });
        const result = await executor.execute(toWorkflow(items), {
          store,
          sessionContext: createSessionContext({ id: "t", env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } }),
          parentTrace: nullTracer.trace({ id: "t", name: "t", kind: "agent" }),
          signalLabel: "scheduler:1",
        });
        const atReturn = events.length;
        const keysAtReturn = Object.keys(store.snapshot()).sort();
        // Let anything that ignored its abort finish.
        await new Promise((resolve) => setTimeout(resolve, 15));
        const where = JSON.stringify(items.map((i) => (i.kind === "leaf" ? i.leaf : i)));

        // Expected, from the plan and the script alone.
        const stopAt = items.findIndex((i) => i.kind === "terminal" || i.kind === "replan");
        const reachable = stopAt < 0 ? items : items.slice(0, stopAt + 1);
        const failAt = reachable.findIndex((i) => (i.kind === "leaf" ? i.leaf.script.fail : i.kind === "parallel" && i.leaves.some((l) => l.script.fail)));
        const ran = (i: Item) => (i.kind === "leaf" ? [i.leaf] : i.kind === "parallel" ? i.leaves : []);
        const mustRun = (failAt < 0 ? reachable : reachable.slice(0, failAt)).flatMap(ran);
        const mayRun = new Set((failAt < 0 ? reachable : reachable.slice(0, failAt + 1)).flatMap(ran).map((l) => l.marker));
        const started = events.filter((e) => e.what === "start").map((e) => e.marker);

        expect(new Set(started).size, `${where}: no call runs twice`).toBe(started.length);
        for (const m of started) expect(mayRun.has(m), `${where}: ${m} ran after the plan stopped`).toBe(true);
        for (const l of mustRun) expect(started, `${where}: ${l.marker} had to run`).toContain(l.marker);

        // Top-level steps are sequential: every call of step k starts after
        // every call of step k-1 has ended.
        const endOf = (m: string) => events.find((e) => e.marker === m && (e.what === "ok" || e.what === "fail" || e.what === "aborted"))?.at ?? Infinity;
        const startOf = (m: string) => events.find((e) => e.marker === m && e.what === "start")?.at ?? Infinity;
        for (let k = 1; k < reachable.length; k++) {
          for (const now of ran(reachable[k]!)) {
            if (!started.includes(now.marker)) continue;
            for (const before of ran(reachable[k - 1]!)) {
              expect(startOf(now.marker), `${where}: ${now.marker} started before ${before.marker} ended`).toBeGreaterThan(endOf(before.marker));
            }
          }
        }

        // Up to four children of a parallel run at once: all of them start
        // before the first one ends.
        for (const item of reachable.slice(0, failAt < 0 ? undefined : failAt + 1)) {
          if (item.kind !== "parallel" || item.leaves.length > 4) continue;
          const firstEnd = Math.min(...item.leaves.map((l) => endOf(l.marker)));
          for (const l of item.leaves) expect(startOf(l.marker), `${where}: ${l.marker} waited for a sibling`).toBeLessThan(firstEnd);
        }

        if (failAt < 0) {
          expect(result.ok, `${where}: nothing failed, so the run succeeds`).toBe(true);
          const replan = reachable.at(-1);
          if (result.ok && replan?.kind === "replan") {
            witnessed.add("replan");
            const bound = mustRun.flatMap((l) => (l.bind ? [l.bind] : []));
            expect(Object.keys(result.replan?.context ?? {}).sort(), `${where}: replan carries exactly the bound names`).toEqual(bound.sort());
          } else if (result.ok) {
            expect(result.replan, `${where}: no replan without a replan step`).toBeUndefined();
          }
          const binds = mustRun.flatMap((l) => (l.bind ? [l.bind] : []));
          expect(keysAtReturn, `${where}: the store holds every bind and nothing else`).toEqual(["list", ...binds].sort());
        } else {
          witnessed.add(reachable[failAt]!.kind === "parallel" ? "a parallel fails" : "a step fails");
          expect(result.ok, `${where}: a failing call fails the run`).toBe(false);
          if (!result.ok) {
            expect(result.stepIndex, `${where}: the first failing step is reported`).toBe(failAt);
            const failing = ran(reachable[failAt]!).filter((l) => l.script.fail);
            expect(failing.map((l) => REASON[l.kind]), `${where}: reason ${result.reason}`).toContain(result.reason);
          }
          const before = reachable.slice(0, failAt).flatMap(ran).flatMap((l) => (l.bind ? [l.bind] : []));
          const sameStep = ran(reachable[failAt]!).flatMap((l) => (l.bind ? [l.bind] : []));
          for (const b of before) expect(keysAtReturn, `${where}: earlier binds stay`).toContain(b);
          for (const k of keysAtReturn) expect(["list", ...before, ...sameStep], `${where}: ${k} bound by a step that never ran`).toContain(k);
        }

        // Nothing outlives execute: no store write after it returned, no
        // abort-honouring call still running, every agent loop ended.
        expect(Object.keys(store.snapshot()).sort(), `${where}: the store changed after execute returned`).toEqual(keysAtReturn);
        const honouring = new Set(leaves.filter((l) => l.script.honoursAbort).map((l) => l.marker));
        const stillRunning = started.filter((m) => honouring.has(m) && endOf(m) >= atReturn);
        expect(stillRunning, `${where}: calls still running after execute returned`).toEqual([]);
        const loopStarts = events.filter((e) => e.what === "loop-start").map((e) => e.marker);
        const loopEnds = events.filter((e) => e.what === "loop-end" && e.at < atReturn).map((e) => e.marker);
        expect(loopEnds.sort(), `${where}: every agent loop ended before execute returned`).toEqual(loopStarts.sort());
        if (events.some((e) => e.what === "aborted")) witnessed.add("a sibling is interrupted");
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(["a parallel fails", "a sibling is interrupted", "a step fails", "replan"]);
  }, 300_000);
});
