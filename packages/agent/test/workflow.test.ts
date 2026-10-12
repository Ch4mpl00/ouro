// Workflow runtime properties: the variable store against plain JSON, `${}`
// substitution against its documented contract, and the plan validator
// against the executor — a plan the validator accepts never dies of a
// missing or duplicate binding at run time, where an earlier step may already
// have sent something and the supervisor won't retry.

import fc from "fast-check";
import { describe, expect, it } from "vitest";
import type { DecisionClient } from "../src/decisions";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { nullTracer } from "../src/tracing";
import { createWorkflowSchema, parseWorkflow, type LeafStep, type Step, type Workflow } from "../src/workflow/dsl";
import { createExecutor, type EngineSurface } from "../src/workflow/execute";
import { createStore, MissingBindingError, substitute } from "../src/workflow/variables";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 300);

// ── the variable store against plain JSON ────────────────────────────────────

// A path exists when every segment names an OWN property of the value
// reached so far — the data, never what objects inherit.
function walk(root: Record<string, unknown>, path: string): { found: boolean; value?: unknown } {
  let current: unknown = root;
  for (const seg of path.split(".")) {
    if (current === null || typeof current !== "object" || !Object.hasOwn(current, seg)) return { found: false };
    current = Reflect.get(current, seg);
  }
  return { found: true, value: current };
}

const INHERITED = ["constructor", "toString", "__proto__", "hasOwnProperty", "valueOf", "length", "map"];

// Paths into the bindings: real keys (own segments), plus inherited names
// and plain misses at any depth.
function pathsInto(bindings: Record<string, unknown>): fc.Arbitrary<string> {
  const real: string[] = [];
  const visit = (value: unknown, prefix: string, depth: number) => {
    if (depth > 3 || value === null || typeof value !== "object") return;
    for (const key of Object.keys(value)) {
      // Not addressable in `${a.b}` syntax: a dot splits, a brace ends it.
      if (key.includes(".") || key.includes("}") || key === "") continue;
      const path = `${prefix}.${key}`;
      real.push(path);
      visit(Reflect.get(value, key), path, depth + 1);
    }
  };
  for (const [name, value] of Object.entries(bindings)) {
    real.push(name);
    visit(value, name, 0);
  }
  const base = fc.constantFrom(...real);
  return fc.oneof(
    { weight: 3, arbitrary: base },
    { weight: 2, arbitrary: fc.tuple(base, fc.constantFrom(...INHERITED, "missing", "0")).map(([p, s]) => `${p}.${s}`) },
    { weight: 1, arbitrary: fc.constantFrom(...INHERITED, "nobody") },
  );
}

const bindings = fc.dictionary(
  fc.constantFrom("posts", "user", "result", "items"),
  fc.jsonValue({ maxDepth: 3 }),
  { minKeys: 1 },
);

describe("variable store", () => {
  it("resolves exactly the paths that exist in the bound data", () => {
    fc.assert(
      fc.property(bindings.chain((b) => fc.tuple(fc.constant(b), fc.array(pathsInto(b), { minLength: 1, maxLength: 10 }))), ([b, paths]) => {
        const store = createStore(b);
        for (const path of paths) {
          const want = walk(b, path);
          expect(store.has(path), `has(${path})`).toBe(want.found);
          if (want.found) expect(store.get(path), `get(${path})`).toEqual(want.value);
          else expect(() => store.get(path), `get(${path}) must throw`).toThrow(MissingBindingError);
        }
      }),
      { numRuns: RUNS },
    );
  });

  // Interpolation: literal text stays, each placeholder becomes its value —
  // strings as-is, null / undefined spelled out, anything else as JSON. A
  // value that itself looks like a placeholder is never expanded again. A
  // whole-string placeholder is the value itself, type and all.
  it("substitutes placeholders by their values and nothing else", () => {
    const literal = fc.string({ maxLength: 12 }).filter((s) => !s.includes("${"));
    fc.assert(
      fc.property(
        bindings.chain((b) => fc.tuple(fc.constant(b), fc.array(fc.tuple(literal, pathsInto(b)), { minLength: 1, maxLength: 4 }), literal)),
        fc.constantFrom("${posts}", "${user.name}"),
        ([b, parts, tail], sneaky) => {
          const store = createStore({ ...b, sneaky });
          const template = parts.map(([lit, path]) => `${lit}\${${path}}`).join("") + tail;
          const resolvable = parts.every(([, path]) => walk(b, path).found);
          if (!resolvable) {
            expect(() => substitute(template, store)).toThrow(MissingBindingError);
            return;
          }
          const render = (v: unknown) => (typeof v === "string" ? v : v === null || v === undefined ? String(v) : JSON.stringify(v));
          const want = parts.map(([lit, path]) => lit + render(walk(b, path).value)).join("") + tail;
          if (parts.length === 1 && parts[0]![0] === "" && tail === "") {
            expect(substitute(template, store)).toEqual(walk(b, parts[0]![1]).value);
          } else {
            expect(substitute(template, store)).toBe(want);
          }
          expect(substitute("x ${sneaky} y", store)).toBe(`x ${sneaky} y`);
        },
      ),
      { numRuns: RUNS },
    );
  });
});

// ── the plan validator against the executor ──────────────────────────────────

const TOOLS = ["t_read", "t_send"];
const SKILLS = ["writer"];
const NAMES = ["a", "b", "c", "env", "signal", "context"];
const { WorkflowSchema } = createWorkflowSchema({ knownTools: TOOLS, knownSkills: SKILLS });

// Text that may reference names: plain words, root placeholders, paths into
// the seeded env / signal, and placeholder look-alikes that aren't ones.
const textRef = fc
  .array(
    fc.oneof(
      fc.constantFrom("hi ", "", "price $5 ", "${} ", "$ {a} ", "${ a } "),
      fc.constantFrom(...NAMES).map((n) => `\${${n}} `),
      fc.constantFrom("${env.timezone} ", "${signal.content} "),
    ),
    { maxLength: 3 },
  )
  .map((xs) => xs.join("") || "go");
const bind = fc.constantFrom(...NAMES.slice(0, 3), "env");

const leaf: fc.Arbitrary<LeafStep> = fc.oneof(
  fc.record({ kind: fc.constant("tool" as const), tool: fc.constantFrom(...TOOLS), args: fc.record({ q: textRef }), bind: fc.option(bind, { nil: undefined }) }).map(
    ({ bind: b, ...s }): LeafStep => (b === undefined ? s : { ...s, bind: b }),
  ),
  fc.record({ kind: fc.constant("llm_compose" as const), preset: fc.constant("base" as const), prompt: textRef, input: fc.record({ data: textRef }), bind }),
  fc.record({
    kind: fc.constant("llm_agent" as const),
    preset: fc.constant("base" as const),
    skill: fc.constant("writer"),
    prompt: textRef,
    tools: fc.constant<string[]>(["t_read"]),
    maxIterations: fc.constant(3),
    bind,
  }),
  fc.record({ kind: fc.constant("code_agent" as const), task: textRef, data: textRef, bind }),
  fc.record({
    kind: fc.constant("decide" as const),
    preset: fc.constant("filter" as const),
    items: fc.constantFrom(...NAMES.slice(0, 3)).map((n) => `\${${n}}`),
    prompt: textRef,
    bind,
  }),
);

const step: fc.Arbitrary<Step> = fc.oneof(
  { weight: 6, arbitrary: leaf },
  { weight: 2, arbitrary: fc.array(leaf, { minLength: 2, maxLength: 3 }).map((steps): Step => ({ kind: "parallel", steps })) },
  { weight: 1, arbitrary: fc.constant<Step>({ kind: "terminal" }) },
);

// Every call succeeds and returns an empty JSON array, so the only way a run
// can fail is the plan itself.
function okEngine(): EngineSurface {
  const provider: ChatProvider = {
    kind: "openai",
    complete: async () => ({ message: { role: "assistant", content: "[]", refusal: null }, finishReason: "stop" }),
  };
  const decisions: DecisionClient = { model: "fake", decide: async () => { throw new Error("the fake decision model refuses"); } };
  return {
    presets: DEFAULT_PRESETS,
    resolveProvider: () => provider,
    mcp: { callTool: async () => "[]" },
    startAgentLoop: async () => ({ messages: [], run: async () => "[]" }),
    endAgentLoop: () => {},
    decisions,
  };
}

async function run(workflow: Workflow, withContext: boolean) {
  const executor = createExecutor({ engine: okEngine(), readSkill: async () => "skill", setMemory: () => {}, codex: { run: async () => ({ ok: true, content: "[]", stderr: "" }) } });
  const store = createStore({
    env: { timezone: "UTC", now: "2026-10-12T00:00:00.000Z" },
    signal: { source: "scheduler", content: "tick", id: 1 },
    ...(withContext ? { context: { prior: 1 } } : {}),
  });
  return executor.execute(workflow, {
    sessionContext: createSessionContext({ id: "t", env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } }),
    store,
    parentTrace: nullTracer.trace({ id: "t", name: "t", kind: "agent" }),
    signalLabel: "scheduler:1",
  });
}

const BINDING_FAILURE = new Set(["missing_binding", "duplicate_binding"]);

describe("plan validator", () => {
  it("accepts a plan only if it can't die of a missing or duplicate binding, and only rejects such plans", async () => {
    const seen = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(step, { minLength: 1, maxLength: 5 }), fc.boolean(), async (steps, withContext) => {
        const workflow: Workflow = { version: 1, steps };
        const seeds = ["env", "signal", ...(withContext ? ["context"] : [])];
        const parsed = parseWorkflow(workflow, WorkflowSchema, { seeds });
        const result = await run(workflow, withContext);
        const bindingFailure = !result.ok && BINDING_FAILURE.has(result.reason);
        if (parsed.ok) {
          seen.add("accepted");
          expect(bindingFailure, `accepted, yet ${!result.ok && result.error.message}: ${JSON.stringify(steps)}`).toBe(false);
        } else {
          seen.add("rejected");
          // A sibling reference inside `parallel` only fails under some
          // schedules, so it is rejected without having to fail this run.
          const onlyOrderDependent = parsed.errors.every((e) => e.includes("parallel"));
          // The fake decision model refuses, so a decide step ends the run
          // before a later binding error can show.
          const decides = JSON.stringify(steps).includes('"kind":"decide"');
          if (!onlyOrderDependent && !decides) {
            expect(bindingFailure, `rejected (${parsed.errors.join("; ")}), yet ran: ${JSON.stringify(steps)}`).toBe(true);
          }
        }
      }),
      { numRuns: RUNS },
    );
    expect([...seen].sort()).toEqual(["accepted", "rejected"]);
  });

  it("names the step and the binding in each error, so the compiler can fix it", () => {
    const parsed = parseWorkflow(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "t_send", args: { text: "${draft}" } },
          { kind: "llm_compose", preset: "base", prompt: "write", input: {}, bind: "draft" },
          { kind: "llm_compose", preset: "base", prompt: "again", input: {}, bind: "draft" },
        ],
      },
      WorkflowSchema,
      { seeds: ["env", "signal"] },
    );
    expect(parsed.ok).toBe(false);
    if (!parsed.ok) {
      expect(parsed.errors.some((e) => e.includes("steps[0]") && e.includes("draft"))).toBe(true);
      expect(parsed.errors.some((e) => e.includes("steps[2]") && e.includes("draft"))).toBe(true);
    }
  });
});
