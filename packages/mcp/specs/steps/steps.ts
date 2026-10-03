// The executable specification's machinery. The specification itself is
// specs/*.feature — plain-English scenarios a person reads and edits. This
// file only teaches cucumber what each sentence means, by driving the real
// server over MCP:
//
//   1. environment — one Postgres + the real server, in-process (support/)
//   2. world       — per-scenario state: the MCP session, the last result
//   3. calls       — generic steps: call a tool, check the result
//   4. memory      — project / document sentences
//   5. signals     — the queue
//   6. scheduler   — time zone, tasks, ticks
//   7. surface     — the tool list
//
//   pnpm spec                                (Docker needed for Postgres)
//   TEST_DATABASE_URL=postgres://… pnpm spec
//
// Sentences are matched whatever their keyword. One sentence means two things
// by position — `"<doc>" in "<project>" reads:` writes the document under
// Given and checks it under Then — so the step looks at its pickle type.

import assert from "node:assert/strict";
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import {
  After,
  AfterAll,
  Before,
  BeforeAll,
  BeforeStep,
  type DataTable,
  defineStep,
  setDefaultTimeout,
  setWorldConstructor,
  World,
} from "@cucumber/cucumber";
import type { StartedPostgreSqlContainer } from "@testcontainers/postgresql";
import { sql } from "drizzle-orm";
import { tick } from "../../src/services/scheduler";
import { call, connect, database, type Outcome, type Server, startServer } from "../support/environment";

setDefaultTimeout(60_000);

// ── 1. environment ───────────────────────────────────────────────────────────

// Built once; every scenario talks to the same server and wipes the data
// before it starts.
let env: Server;
let container: StartedPostgreSqlContainer | null = null;

BeforeAll({ timeout: 180_000 }, async function () {
  const db = await database();
  container = db.container;
  env = await startServer(db.url);
});

AfterAll(async function () {
  await env?.stop();
  await container?.stop();
});

// Every table a scenario can touch, emptied: each scenario starts from a
// freshly migrated database.
Before(async function () {
  await env.pg.db.execute(
    sql`TRUNCATE news_items, knowledge_base_notes, memory_projects, memory_project_docs,
                 memory_doc_patches, memory_facts, memory_index RESTART IDENTITY CASCADE`,
  );
  await env.state.pool.query(
    `TRUNCATE signals, scheduled_tasks, settings, telegram_messages, telegram_kv, gmail_kv,
              integration_account RESTART IDENTITY`,
  );
});

After(async function (this: Spec) {
  await this.client?.close();
});

// ── 2. world ─────────────────────────────────────────────────────────────────

type StepType = "Context" | "Action" | "Outcome" | "Unknown";

class Spec extends World {
  client: Client | null = null;
  last: Outcome | null = null;
  remembered = new Map<string, unknown>();
  delivered: number[] = [];
  queued: number[] = [];
  lastTask: number | null = null;
  stepType: StepType = "Unknown";

  async session(): Promise<Client> {
    this.client ??= await connect(env.url);
    return this.client;
  }

  // `{{name}}` → a value remembered earlier in the scenario.
  substitute(text: string): string {
    let out = text;
    for (const [name, value] of this.remembered) {
      out = out.replaceAll(`{{${name}}}`, typeof value === "string" ? value : JSON.stringify(value));
    }
    return out;
  }

  async call(tool: string, args: Record<string, unknown>): Promise<Outcome> {
    this.last = await call(await this.session(), tool, args);
    return this.last;
  }

  result(): unknown {
    if (this.last?.kind !== "ok") assert.fail(`expected a successful call, got ${JSON.stringify(this.last)}`);
    return this.last.value;
  }
}

setWorldConstructor(Spec);

BeforeStep(function (this: Spec, { pickleStep }) {
  this.stepType = pickleStep.type ?? "Unknown";
});

// ── 3. calls ─────────────────────────────────────────────────────────────────

function jsonArg(spec: Spec, docstring: string): Record<string, unknown> {
  const raw = spec.substitute(docstring);
  try {
    return JSON.parse(raw);
  } catch (err) {
    return assert.fail(`arguments are not JSON (${(err as Error).message}):\n${raw}`);
  }
}

// "doc.body", "failures.0.reason"
function at(value: unknown, path: string): unknown {
  let current: unknown = value;
  for (const key of path.split(".")) {
    if (current === null || typeof current !== "object") return undefined;
    current = Array.isArray(current) && /^\d+$/.test(key) ? current[Number(key)] : (current as Record<string, unknown>)[key];
  }
  return current;
}

// A table cell: JSON when it parses as JSON (2, true, null, "…", […]), the
// bare text otherwise.
function cell(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

// Every key in `expected` must be present in `actual` with a matching value;
// extra keys in `actual` are fine. Arrays match element by element.
function subset(expected: unknown, actual: unknown): boolean {
  if (Array.isArray(expected)) {
    return Array.isArray(actual) && expected.length === actual.length && expected.every((e, i) => subset(e, actual[i]));
  }
  if (expected !== null && typeof expected === "object") {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual)) return false;
    return Object.entries(expected).every(
      ([k, v]) => Object.hasOwn(actual, k) && subset(v, (actual as Record<string, unknown>)[k]),
    );
  }
  return expected === actual;
}

const pretty = (value: unknown) => JSON.stringify(value, null, 2);

defineStep("I call {string}", async function (this: Spec, tool: string) {
  await this.call(tool, {});
});

defineStep("I call {string} with:", async function (this: Spec, tool: string, docstring: string) {
  await this.call(tool, jsonArg(this, docstring));
});

defineStep("the result is:", function (this: Spec, table: DataTable) {
  const result = this.result();
  for (const [path, value] of table.raw().slice(1)) {
    const actual = at(result, path!);
    assert.notEqual(actual, undefined, `no \`${path}\` in the result:\n${pretty(result)}`);
    assert.deepEqual(actual, cell(this.substitute(value!)), `\`${path}\` in the result:\n${pretty(result)}`);
  }
});

defineStep("the result includes:", function (this: Spec, docstring: string) {
  const expected = jsonArg(this, docstring);
  const actual = this.result();
  assert.ok(subset(expected, actual), `expected the result to include\n${pretty(expected)}\nbut it was\n${pretty(actual)}`);
});

defineStep("the call fails with {string}", function (this: Spec, text: string) {
  if (this.last?.kind !== "toolError") assert.fail(`expected a tool error containing "${text}", got ${pretty(this.last)}`);
  assert.ok(this.last.message.includes(text), `tool error was: ${this.last.message}`);
});

// Invalid parameters never reach the tool. The TS SDK answers them as an
// isError result carrying the JSON-RPC code ("MCP error -32602: Input
// validation error: …") rather than as a protocol error; both mean this.
defineStep("the call is rejected as invalid parameters", function (this: Spec) {
  const last = this.last;
  if (last?.kind === "rejected" && last.code === -32602 && last.message.length > 0) return;
  if (last?.kind === "toolError" && last.message.startsWith("MCP error -32602:")) return;
  assert.fail(`expected an invalid-params rejection, got ${pretty(last)}`);
});

defineStep("I remember {string} as {string}", function (this: Spec, path: string, name: string) {
  const value = at(this.result(), path);
  assert.notEqual(value, undefined, `no \`${path}\` to remember`);
  this.remembered.set(name, value);
});

// ── 4. memory ────────────────────────────────────────────────────────────────

defineStep("a project {string} titled {string}", async function (this: Spec, slug: string, title: string) {
  const outcome = await this.call("create_project", { slug, title });
  assert.ok(outcome.kind === "ok" && at(outcome.value, "ok") === true, `creating the project: ${pretty(outcome)}`);
});

// A document's text is a file's: it ends with a newline. Standard Gherkin
// drops the one before the closing quotes; gherkin-rs, which the Rust harness
// used, keeps it — this keeps the sentence meaning the same in both.
const fileText = (docstring: string): string => `${docstring}\n`;

// Given: the document exists with this text. Then: it has this text.
defineStep("{string} in {string} reads:", async function (this: Spec, doc: string, project: string, docstring: string) {
  const body = fileText(docstring);
  if (this.stepType === "Context") {
    const outcome = await this.call("write_doc", { project, doc, body });
    assert.ok(outcome.kind === "ok" && at(outcome.value, "ok") === true, `writing the document: ${pretty(outcome)}`);
    return;
  }
  const saved = this.last;
  await this.call("read_doc", { project, doc });
  const actual = at(this.result(), "doc.body");
  this.last = saved;
  assert.equal(actual, body, "the document's text");
});

defineStep(
  "{string} in {string} is at version {int}",
  async function (this: Spec, doc: string, project: string, version: number) {
    const saved = this.last;
    await this.call("read_doc", { project, doc });
    const actual = at(this.result(), "doc.version");
    this.last = saved;
    assert.equal(actual, version, "the document's version");
  },
);

// ── 5. signals ───────────────────────────────────────────────────────────────

defineStep("the signal queue holds:", async function (this: Spec, table: DataTable) {
  for (const [source, content] of table.raw().slice(1)) {
    this.queued.push(await env.signals.record({ source: source!, content: content! }));
  }
});

defineStep("the signal queue holds {int} signals", async function (this: Spec, count: number) {
  for (let i = 0; i < count; i++) {
    this.queued.push(await env.signals.record({ source: "telegram", content: `message ${i}` }));
  }
});

defineStep(
  "{int} agents take signals at the same time until the queue is empty",
  async function (this: Spec, agents: number) {
    const client = await this.session();
    const worker = async (): Promise<number[]> => {
      const got: number[] = [];
      for (;;) {
        const outcome = await call(client, "get_next_signal", {});
        if (outcome.kind !== "ok") assert.fail(`get_next_signal failed: ${pretty(outcome)}`);
        const id = at(outcome.value, "signal.id");
        if (typeof id !== "number") return got;
        got.push(id);
      }
    };
    this.delivered = (await Promise.all(Array.from({ length: agents }, worker))).flat();
  },
);

defineStep("every signal was delivered exactly once", function (this: Spec) {
  const counts = new Map<number, number>();
  for (const id of this.delivered) counts.set(id, (counts.get(id) ?? 0) + 1);
  const twice = [...counts].filter(([, n]) => n > 1).map(([id]) => id).sort((a, b) => a - b);
  const lost = this.queued.filter((id) => !counts.has(id)).sort((a, b) => a - b);
  assert.ok(
    twice.length === 0 && lost.length === 0,
    `${twice.length} of ${this.queued.length} signals were delivered more than once (${twice}); lost: ${lost}`,
  );
});

// ── 6. scheduler ─────────────────────────────────────────────────────────────

function instant(text: string): Date {
  const date = new Date(text);
  assert.ok(!Number.isNaN(date.getTime()), `not an ISO time: ${text}`);
  return date;
}

defineStep("the user's time zone is {string}", async function (this: Spec, tz: string) {
  const outcome = await this.call("set_timezone", { tz });
  assert.ok(outcome.kind === "ok" && at(outcome.value, "ok") === true, `setting the time zone: ${pretty(outcome)}`);
});

defineStep(
  "a {word} task {string} saying {string} created at {string}",
  async function (this: Spec, kind: string, cron: string, prompt: string, created: string) {
    assert.ok(kind === "recurring" || kind === "one-shot", `a task is recurring or one-shot, not ${kind}`);
    const row = await env.scheduler.tasks.insert({ cron_expr: cron, recurring: kind === "recurring", prompt });
    await env.state.pool.query("UPDATE scheduled_tasks SET created_at = $1 WHERE id = $2", [instant(created), row.id]);
    this.lastTask = row.id;
  },
);

defineStep("it last fired for the slot {string}", async function (this: Spec, slot: string) {
  assert.ok(this.lastTask !== null, "a task was created earlier in the scenario");
  await env.state.pool.query("UPDATE scheduled_tasks SET last_run_at = $1 WHERE id = $2", [
    Math.floor(instant(slot).getTime() / 1000),
    this.lastTask,
  ]);
});

defineStep("the scheduler ticks at {string}", async function (this: Spec, now: string) {
  await tick(env.scheduler, instant(now));
});

defineStep("{int} signal(s) is/are waiting", async function (this: Spec, count: number) {
  assert.equal(await env.signals.countPending(), count, "signals waiting in the queue");
});

defineStep(
  "the next signal is a {string} signal for the slot {string}",
  async function (this: Spec, source: string, slot: string) {
    await this.call("get_next_signal", {});
    const signal = at(this.result(), "signal");
    assert.equal(at(signal, "source"), source, `the signal's source: ${pretty(signal)}`);
    const content = String(at(signal, "content") ?? "");
    assert.ok(content.includes(`Slot: ${slot}`), `the signal's slot:\n${content}`);
  },
);

// ── 7. surface ───────────────────────────────────────────────────────────────

defineStep("the server offers exactly these tools:", async function (this: Spec, docstring: string) {
  const expected = docstring.split(/\s+/).filter(Boolean).sort();
  const { tools } = await (await this.session()).listTools();
  assert.deepEqual(tools.map((t) => t.name).sort(), expected, "the tool surface");
});
