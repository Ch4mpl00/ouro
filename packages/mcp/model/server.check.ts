// Differential test: the real memory server against the reference model.
// The model is the specification — read model.ts, not this file. This file
// only generates calls, sends each one to both, and compares the answers:
//
//   1. environment — Postgres + the real server over MCP (specs/support),
//                    a restricted multi-session instance like mcp-tunnel
//   2. sequential  — random call sequences, answer for answer
//   3. concurrent  — overlapping calls from several sessions, then a
//                    linearizability check against the model
//
//   pnpm spec:model                          (needs Docker)
//   TEST_DATABASE_URL=postgres://… pnpm spec:model
//   MODEL_CASES=500 pnpm spec:model          (more cases; default 200)
//
// A failure prints the smallest input that still fails (fast-check shrinks
// it) and the transcript: every call, the server's answer, and what the
// model expected.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import type { StartedPostgreSqlContainer } from "@testcontainers/postgresql";
import { sql } from "drizzle-orm";
import fc from "fast-check";
import { afterAll, beforeAll, describe, it } from "vitest";
import { call, connect, database, type Server, startServer } from "../specs/support/environment";
import { canonical, modelAnswer, normalise, sameAnswer, type Answer } from "./answers";
import { type Call, concretise, describeCall, DOCS, PROJECT, step, type Step, toTool } from "./calls";
import { linearizable, type Event } from "./linearizable";
import { emptyWorld, type World } from "./model";

const CASES = Number(process.env.MODEL_CASES ?? 200);

// ── 1. environment ───────────────────────────────────────────────────────────

let env: Server;
let container: StartedPostgreSqlContainer | null = null;
let client: Client;

beforeAll(async () => {
  const db = await database();
  container = db.container;
  env = await startServer(db.url, ["memory"]);
  client = await connect(env.url);
}, 180_000);

afterAll(async () => {
  await client?.close();
  await env?.stop();
  await container?.stop();
});

// Patch ids on the server, oldest first — index i is the model's patch i.
async function patchIds(c: Client, doc: string): Promise<string[]> {
  const outcome = await call(c, "doc_history", { project: PROJECT, doc, limit: 100 });
  if (outcome.kind !== "ok") return [];
  const history = (outcome.value as { history?: { patchId: string }[] }).history ?? [];
  return history.map((p) => p.patchId).reverse();
}

async function onServer(c: Client, request: Call): Promise<Answer> {
  const { tool, args } = toTool(request);
  return normalise(request, await call(c, tool, args));
}

// An empty memory with the one project, on both sides.
async function freshStart(): Promise<World> {
  await env.pg.db.execute(
    sql`TRUNCATE memory_projects, memory_project_docs, memory_doc_patches, memory_index RESTART IDENTITY CASCADE`,
  );
  const created = await call(client, "create_project", { slug: PROJECT, title: "Model" });
  if (created.kind !== "ok") throw new Error(`creating the project: ${JSON.stringify(created)}`);
  return emptyWorld;
}

// ── 2. sequential ────────────────────────────────────────────────────────────

// Runs the steps on both sides; the transcript of every call so far is the
// failure report. Returns the model's world afterwards.
async function sequential(steps: Step[], start: World, transcript: string[]): Promise<World> {
  let world = start;
  for (const s of steps) {
    const request = await concretise(s, world, (doc) => patchIds(client, doc));
    const [next, expected] = modelAnswer(world, request);
    const actual = await onServer(client, request);
    transcript.push(`  ${describeCall(request)}\n      → ${canonical(actual)}`);
    if (!sameAnswer(expected, actual)) {
      throw new Error(`the server and the model disagree:\n${transcript.join("\n")}\n  model expected: ${canonical(expected)}`);
    }
    world = next;
  }
  return world;
}

// ── 3. concurrent ────────────────────────────────────────────────────────────

// The sessions' steps are made concrete against the same snapshot, so they
// collide: two patches of the same version, a revert racing a patch.
async function concurrent(prefix: Step[], sessions: Step[][]): Promise<void> {
  const transcript: string[] = [];
  const world = await sequential(prefix, await freshStart(), transcript);
  const planned: Call[][] = [];
  for (const steps of sessions) {
    const calls: Call[] = [];
    for (const s of steps) calls.push(await concretise(s, world, (doc) => patchIds(client, doc)));
    planned.push(calls);
  }

  const clients = await Promise.all(planned.map(() => connect(env.url)));
  const t0 = performance.now();
  const runs = planned.map(async (calls, session) => {
    const events: (Event & { who: string })[] = [];
    for (const request of calls) {
      const start = performance.now() - t0;
      const answer = await onServer(clients[session]!, request);
      events.push({ who: `s${session}`, call: request, answer, start, end: performance.now() - t0 });
    }
    return events;
  });
  const events = (await Promise.all(runs)).flat();
  await Promise.all(clients.map((c) => c.close()));

  // Afterwards, one look at each document: the final state must be one the
  // linearization reaches.
  for (const doc of DOCS) {
    for (const request of [{ cmd: { op: "read", doc } }, { cmd: { op: "history", doc } }] as const) {
      const start = performance.now() - t0;
      const answer = await onServer(client, request);
      events.push({ who: "after", call: request, answer, start, end: performance.now() - t0 });
    }
  }

  if (linearizable(events, world)) return;
  const lines = events
    .sort((a, b) => a.start - b.start)
    .map(
      (e) =>
        `  ${e.who.padStart(5)} [${e.start.toFixed(1)}–${e.end.toFixed(1)}ms] ${describeCall(e.call)}\n          → ${canonical(e.answer)}`,
    );
  throw new Error(
    `not linearizable — no order of these calls makes the model give these answers.\nprefix:\n${transcript.join("\n")}\nconcurrent:\n${lines.join("\n")}`,
  );
}

describe("memory server vs the reference model", () => {
  it("answers every call sequence the way the model does", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(step(DOCS.length), { minLength: 1, maxLength: 30 }), async (steps) => {
        await sequential(steps, await freshStart(), []);
      }),
      { numRuns: CASES },
    );
  });

  it("behaves as if overlapping calls ran one at a time", async () => {
    const session = fc.array(step(1), { minLength: 1, maxLength: 3 });
    await fc.assert(
      fc.asyncProperty(
        fc.array(step(1), { maxLength: 6 }),
        fc.array(session, { minLength: 2, maxLength: 4 }),
        concurrent,
      ),
      { numRuns: CASES },
    );
  });
});
