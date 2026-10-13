// The agent's sqlite state on a real database file: the memory KV (digest
// watermarks) and the trace mirror with its judgements, under generated
// operation sequences — including closing and reopening the file, as a
// restart does. Expectations are the plain store semantics: last write wins,
// reads see exactly what was written (as JSON), filters and orderings hold.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import fc from "fast-check";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createAgentDb, type AgentDatabase } from "../src/db/client";
import { createMemoryStore } from "../src/db/memory";
import { createTraceStore, type JudgementInput, type StoredTraceInput } from "../src/db/trace-store";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 60);

let dir: string;
beforeAll(() => {
  dir = mkdtempSync(path.join(tmpdir(), "agent-db-"));
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));
const fresh = () => path.join(dir, `agent-${Math.random().toString(36).slice(2)}.db`);

// What a value looks like after a trip through a JSON column.
const asJson = (v: unknown): unknown => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

describe("memory KV", () => {
  it("returns the last value written for each key, across reopening the file", async () => {
    const key = fc.constantFrom("news_digest.last_read_at", "tech_digest.last_read_at", "a", "ключ", "x".repeat(300));
    const op = fc.oneof(
      fc.record({ kind: fc.constant("set" as const), key, value: fc.string({ unit: "grapheme", maxLength: 50 }) }),
      fc.record({ kind: fc.constant("get" as const), key }),
      fc.constant({ kind: "reopen" as const }),
    );
    fc.assert(
      fc.property(fc.array(op, { minLength: 1, maxLength: 30, size: "max" }), (ops) => {
        const file = fresh();
        let db: AgentDatabase = createAgentDb(file);
        let store = createMemoryStore(db);
        const model = new Map<string, string>();
        try {
          for (const o of ops) {
            if (o.kind === "set") {
              store.set(o.key, o.value);
              model.set(o.key, o.value);
            } else if (o.kind === "get") {
              expect(store.get(o.key), `get(${o.key})`).toBe(model.get(o.key) ?? null);
            } else {
              db.$client.close();
              db = createAgentDb(file);
              store = createMemoryStore(db);
            }
          }
          for (const [k, v] of model) expect(store.get(k), `final ${k}`).toBe(v);
        } finally {
          db.$client.close();
        }
      }),
      { numRuns: RUNS },
    );
  });
});

describe("trace mirror and judgements", () => {
  const traceId = fc.constantFrom("telegram:1", "scheduler:2", "gmail:3", "t4", "t5");
  const json = fc.oneof(fc.constant(undefined), fc.jsonValue({ maxDepth: 2 }));
  const provider = fc.constantFrom("codex", "openai");
  const version = fc.constantFrom("n1", "n2");
  const skill = fc.constantFrom("planner", "news-digest", "telegram");

  type Op =
    | { kind: "trace"; id: string; input: unknown; output: unknown; tags: string[]; name: string }
    | { kind: "judge"; pick: number; observationId: string; provider: string; version: string; skill: string; score: number | null }
    | { kind: "recent"; limit: number; filter: { provider: string; promptVersion: string } | undefined }
    | { kind: "judgements"; skill: string; provider: string; version: string };

  const op: fc.Arbitrary<Op> = fc.oneof(
    { weight: 3, arbitrary: fc.record({ kind: fc.constant("trace" as const), id: traceId, input: json, output: json, tags: fc.array(fc.constantFrom("telegram", "agent-loop", "planner-mode"), { maxLength: 3 }), name: fc.constantFrom("signal:telegram", "signal:scheduler") }) },
    {
      weight: 3,
      arbitrary: fc.record({
        kind: fc.constant("judge" as const),
        pick: fc.nat(),
        observationId: fc.constantFrom("o1", "o2"),
        provider,
        version,
        skill,
        score: fc.option(fc.integer({ min: 0, max: 5 }), { nil: null }),
      }),
    },
    { weight: 2, arbitrary: fc.record({ kind: fc.constant("recent" as const), limit: fc.integer({ min: 1, max: 6 }), filter: fc.option(fc.record({ provider, promptVersion: version }), { nil: undefined }) }) },
    { weight: 2, arbitrary: fc.record({ kind: fc.constant("judgements" as const), skill, provider, version }) },
  );

  it("reads back what was written, last write winning, with filters and orderings that hold", () => {
    fc.assert(
      fc.property(fc.array(op, { minLength: 1, maxLength: 30, size: "max" }), (ops) => {
        const db = createAgentDb(fresh());
        const store = createTraceStore(db);
        const written = new Map<string, StoredTraceInput>();
        const judged = new Map<string, JudgementInput>();
        let clock = Date.parse("2026-10-01T00:00:00Z");
        try {
          for (const o of ops) {
            if (o.kind === "trace") {
              const t: StoredTraceInput = {
                id: o.id,
                name: o.name,
                source: o.tags[0] ?? null,
                skill: null,
                sessionId: o.id,
                tags: o.tags,
                input: o.input,
                output: o.output,
                metadata: { signal_source: o.tags[0] ?? null },
                observations: [],
                startedAt: new Date((clock += 1000)).toISOString(),
              };
              store.writeTrace(t);
              written.set(o.id, t);
              const back = store.getTrace(o.id);
              expect(back?.trace, `getTrace(${o.id}) after a write`).toMatchObject({
                id: t.id,
                name: t.name,
                sessionId: t.sessionId,
                timestamp: t.startedAt,
                tags: t.tags,
                input: asJson(t.input),
                output: asJson(t.output),
              });
            } else if (o.kind === "judge") {
              const ids = [...written.keys()];
              if (ids.length === 0) continue;
              const j: JudgementInput = {
                traceId: ids[o.pick % ids.length]!,
                observationId: o.observationId,
                nodeKind: "compose",
                skill: o.skill,
                provider: o.provider,
                promptVersion: o.version,
                scores: { query_formulation: o.score, process: o.score, coverage: null, composition: null, faithfulness: null },
                detail: { note: `score ${o.score}` },
              };
              store.writeJudgement(j);
              judged.set([j.traceId, j.observationId, j.provider, j.promptVersion].join("|"), j);
            } else if (o.kind === "recent") {
              const f = o.filter;
              const got = store.listRecent(o.limit, f).map((s) => s.id);
              const eligible = [...written.values()]
                .filter((t) => !f || ![...judged.values()].some((j) => j.traceId === t.id && j.provider === f.provider && j.promptVersion === f.promptVersion))
                .sort((a, b) => b.startedAt.localeCompare(a.startedAt))
                .slice(0, o.limit)
                .map((t) => t.id);
              expect(got, `listRecent(${o.limit}, ${JSON.stringify(o.filter)})`).toEqual(eligible);
            } else {
              const got = store
                .listJudgements({ skill: o.skill, provider: o.provider, promptVersion: o.version })
                .map((r) => [r.traceId, r.observationId, r.scores.process, r.startedAt].join("|"))
                .sort();
              const want = [...judged.values()]
                .filter((j) => j.skill === o.skill && j.provider === o.provider && j.promptVersion === o.version)
                .map((j) => [j.traceId, j.observationId, j.scores.process, written.get(j.traceId)?.startedAt].join("|"))
                .sort();
              expect(got, `listJudgements(${o.skill}, ${o.provider}, ${o.version})`).toEqual(want);
              const skills = store.listJudgedSkills({ provider: o.provider, promptVersion: o.version }).sort();
              const wantSkills = [...new Set([...judged.values()].filter((j) => j.provider === o.provider && j.promptVersion === o.version).map((j) => j.skill))].sort();
              expect(skills, "listJudgedSkills").toEqual(wantSkills);
            }
          }
        } finally {
          db.$client.close();
        }
      }),
      { numRuns: RUNS },
    );
  });
});

// Several processes opening one fresh database at once, as the agent,
// judge-worker and improve-worker do on the shared volume after a deploy
// with a new migration. Known and tracked: .claude/tasks/agent-migration-race.md
// (losers crash with "table already exists" and self-heal on restart).
describe("migrations on boot", () => {
  it("migrating the same file twice is a no-op", () => {
    const file = fresh();
    createAgentDb(file).$client.close();
    const again = createAgentDb(file);
    expect(createMemoryStore(again).get("anything")).toBeNull();
    again.$client.close();
  });

  it("three processes booting at once all come up (agent-migration-race.md)", async () => {
    const file = fresh();
    const script = path.join(dir, "boot.mts");
    const client = path.resolve(import.meta.dirname, "../src/db/client.ts");
    // All three wait for one shared instant, so their migrations overlap.
    const startAt = Date.now() + 4_000;
    writeFileSync(
      script,
      `import { createAgentDb } from ${JSON.stringify(client)};\n` +
        `while (Date.now() < ${startAt}) {}\n` +
        `createAgentDb(${JSON.stringify(file)}).$client.close();\n`,
    );
    const boot = () =>
      new Promise<number>((resolve) => {
        const child = spawn("pnpm", ["exec", "tsx", script], { cwd: path.resolve(import.meta.dirname, "../../.."), stdio: "ignore" });
        child.on("exit", (code) => resolve(code ?? 1));
      });
    expect(await Promise.all([boot(), boot(), boot()])).toEqual([0, 0, 0]);
  }, 120_000);
});
