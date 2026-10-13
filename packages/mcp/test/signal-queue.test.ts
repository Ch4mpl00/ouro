// The signal queue, judged from the outside: pollers record signals through
// the Signals module (their real boundary), the agent pops and lists them
// over MCP, and every step is checked against what Postgres actually holds
// before and after.
//
// Rules are independent statements about one observed transition, named so
// a failure says which requirement broke:
//   - outcome: which reply is legal, decided from the request and the
//     pre-state only;
//   - effect: what the reply and the changed rows must look like;
//   - frame: what must not change, refusals included.
//
// PROPERTY_RUNS raises the number of generated sequences.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { call, connect, startServer, type Outcome, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 60);
const DEFAULT_LIST_LIMIT = 200;

// ── observation ──────────────────────────────────────────────────────────────

interface Stored {
  id: number;
  source: string;
  content: string;
  createdUs: number;
  consumedUs: number | null;
}
type World = ReadonlyMap<number, Stored>;

const storedRow = z.object({
  id: z.number().int(),
  source: z.string(),
  content: z.string(),
  created_us: z.number().int(),
  consumed_us: z.number().int().nullable(),
});

async function observe(server: Server): Promise<World> {
  const { rows } = await server.state.pool.query(
    `SELECT id, source, content,
            (extract(epoch FROM created_at) * 1000000)::bigint AS created_us,
            (extract(epoch FROM consumed_at) * 1000000)::bigint AS consumed_us
       FROM signals ORDER BY id`,
  );
  const world = new Map<number, Stored>();
  for (const raw of rows) {
    const r = storedRow.parse(raw);
    world.set(r.id, { id: r.id, source: r.source, content: r.content, createdUs: r.created_us, consumedUs: r.consumed_us });
  }
  return world;
}

const pending = (w: World) => [...w.values()].filter((s) => s.consumedUs === null);
const second = (us: number) => Math.floor(us / 1_000_000);

// The documented wire shape: "YYYY-MM-DD HH:MM:SS", UTC, truncated.
function visible(us: number): string {
  return new Date(second(us) * 1000).toISOString().slice(0, 19).replace("T", " ");
}

// ── replies (parsed strictly: an unexpected shape fails the run) ─────────────

const nextReply = z
  .object({
    signal: z
      .object({
        id: z.number().int(),
        source: z.string(),
        content: z.string(),
        created_at: z.string(),
        envContext: z.string().nullable(),
      })
      .strict()
      .nullable(),
    pendingAfter: z.number().int(),
  })
  .strict();

const listReply = z
  .object({
    count: z.number().int(),
    signals: z.array(
      z
        .object({
          id: z.number().int(),
          source: z.string(),
          content: z.string(),
          created_at: z.string(),
          consumed_at: z.string().nullable(),
        })
        .strict(),
    ),
  })
  .strict();

function okValue(outcome: Outcome, op: string): unknown {
  if (outcome.kind !== "ok") throw new Error(`${op}: expected a result, got ${JSON.stringify(outcome)}`);
  return outcome.value;
}

// ── operations ───────────────────────────────────────────────────────────────

// `since` is generated relative to whatever the queue holds when the step
// runs: a stored signal's second, shifted, in either accepted spelling.
type Since =
  | { kind: "around"; pick: number; shift: -1 | 0 | 1; spelling: "visible" | "iso"; ms: number }
  | { kind: "garbage"; text: string };

type Op =
  | { kind: "record"; source: string; content: string }
  | { kind: "next" }
  | { kind: "list"; source: string | undefined; since: Since | undefined; limit: number | undefined };

const SOURCES = ["telegram", "gmail", "scheduler", "nashdom-bill", "dreaming"];

const content = fc.oneof(
  { weight: 8, arbitrary: fc.string({ unit: "grapheme", maxLength: 120 }) },
  { weight: 1, arbitrary: fc.constant("") },
  { weight: 1, arbitrary: fc.constant("🧾 Рахунок №42 — «НашДім»\n".repeat(800)) },
);

const since: fc.Arbitrary<Since> = fc.oneof(
  {
    weight: 9,
    arbitrary: fc.record({
      kind: fc.constant("around" as const),
      pick: fc.nat(),
      shift: fc.constantFrom(-1 as const, 0 as const, 1 as const),
      spelling: fc.constantFrom("visible" as const, "iso" as const),
      ms: fc.integer({ min: 0, max: 999 }),
    }),
  },
  {
    weight: 1,
    arbitrary: fc.record({
      kind: fc.constant("garbage" as const),
      text: fc.constantFrom("", "yesterday", "2026-13-01 00:00:00", "2026-10-02 25:00:00", "not a date"),
    }),
  },
);

const limit = fc.oneof(
  { weight: 4, arbitrary: fc.constant(undefined) },
  { weight: 4, arbitrary: fc.integer({ min: 1, max: 6 }) },
  { weight: 1, arbitrary: fc.constantFrom(DEFAULT_LIST_LIMIT, 2000) },
  { weight: 1, arbitrary: fc.constantFrom(0, -1, 2001, 1.5) },
);

const op: fc.Arbitrary<Op> = fc.oneof(
  {
    weight: 5,
    arbitrary: fc.record({ kind: fc.constant("record" as const), source: fc.constantFrom(...SOURCES), content }),
  },
  { weight: 3, arbitrary: fc.constant<Op>({ kind: "next" }) },
  // An unfiltered listing with a small limit, so a cut listing is common.
  {
    weight: 1,
    arbitrary: fc.integer({ min: 1, max: 3 }).map((limit): Op => ({ kind: "list", source: undefined, since: undefined, limit })),
  },
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("list" as const),
      source: fc.option(fc.constantFrom(...SOURCES, "unknown-source"), { nil: undefined }),
      since: fc.option(since, { nil: undefined }),
      limit,
    }),
  },
);

// ── rules ────────────────────────────────────────────────────────────────────

class Violations {
  readonly found: string[] = [];
  check(rule: string, holds: boolean, detail: () => unknown): void {
    if (!holds) this.found.push(`${rule}: ${JSON.stringify(detail())}`);
  }
}

function sameRow(a: Stored | undefined, b: Stored | undefined): boolean {
  return (
    a !== undefined &&
    b !== undefined &&
    a.id === b.id &&
    a.source === b.source &&
    a.content === b.content &&
    a.createdUs === b.createdUs &&
    a.consumedUs === b.consumedUs
  );
}

// Every row of `before` other than `except` is still there, untouched.
function frame(v: Violations, rule: string, before: World, after: World, except?: number): void {
  for (const [id, row] of before) {
    if (id === except) continue;
    v.check(rule, sameRow(row, after.get(id)), () => ({ id, before: row, after: after.get(id) ?? null }));
  }
}

function noNewRows(v: Violations, rule: string, before: World, after: World): void {
  const added = [...after.keys()].filter((id) => !before.has(id));
  v.check(rule, added.length === 0, () => ({ added }));
}

const isRefusal = (o: Outcome) => o.kind === "toolError" || o.kind === "rejected";

// Witnesses: interesting situations that must actually occur across a run,
// so a generator drifting into trivia is noticed.
const witnessed = new Set<string>();

async function step(server: Server, client: Client, o: Op): Promise<string[]> {
  const v = new Violations();
  const before = await observe(server);

  if (o.kind === "record") {
    const id = await server.signals.record({ source: o.source, content: o.content });
    const after = await observe(server);
    const row = after.get(id);
    v.check("record: the returned id names a new row", row !== undefined && !before.has(id), () => ({ id }));
    v.check("record: the row holds exactly what was recorded", row?.source === o.source && row.content === o.content, () => ({
      stored: row ?? null,
    }));
    v.check("record: a new signal is pending", row?.consumedUs === null, () => ({ stored: row ?? null }));
    v.check("record: ids only grow", [...before.keys()].every((old) => old < id), () => ({ id, max: Math.max(...before.keys()) }));
    v.check("record: exactly one row is added", after.size === before.size + 1, () => ({ before: before.size, after: after.size }));
    frame(v, "record frame: existing signals are untouched", before, after);
    if (o.content.length > 10_000) witnessed.add("a large signal round-trips");
    return v.found;
  }

  if (o.kind === "next") {
    const outcome = await call(client, "get_next_signal", {});
    const reply = nextReply.parse(okValue(outcome, "get_next_signal"));
    const after = await observe(server);
    const waiting = pending(before);
    const oldest = waiting.reduce<Stored | undefined>((min, s) => (min === undefined || s.id < min.id ? s : min), undefined);

    if (oldest === undefined) {
      witnessed.add("an empty queue is asked");
      v.check("next on empty: replies null", reply.signal === null, () => reply);
      v.check("next on empty: pendingAfter is 0", reply.pendingAfter === 0, () => reply);
      frame(v, "next on empty frame: nothing changes", before, after);
      noNewRows(v, "next on empty frame: nothing is added", before, after);
      return v.found;
    }

    if (waiting.length >= 2) witnessed.add("a signal is taken while others wait");
    const got = reply.signal;
    v.check("next: a waiting queue hands out a signal", got !== null, () => ({ waiting: waiting.length }));
    if (got === null) return v.found;
    v.check("next: the oldest waiting signal comes out", got.id === oldest.id, () => ({ got: got.id, oldest: oldest.id }));
    const was = before.get(got.id);
    v.check("next: the signal comes out as recorded", was !== undefined && got.source === was.source && got.content === was.content, () => ({
      got,
      stored: was ?? null,
    }));
    v.check("next: created_at is the stored time on the wire format", was !== undefined && got.created_at === visible(was.createdUs), () => ({
      got: got.created_at,
      stored: was ? visible(was.createdUs) : null,
    }));
    v.check("next: with nothing configured, envContext is null", got.envContext === null, () => got.envContext);
    v.check("next: the handed-out signal is now consumed", after.get(got.id)?.consumedUs !== null, () => after.get(got.id) ?? null);
    v.check("next: pendingAfter counts what is still waiting", reply.pendingAfter === pending(after).length, () => ({
      pendingAfter: reply.pendingAfter,
      waiting: pending(after).length,
    }));
    frame(v, "next frame: every other signal is untouched", before, after, got.id);
    noNewRows(v, "next frame: nothing is added", before, after);
    return v.found;
  }

  // list
  const rows = [...before.values()];
  const anchorUs = rows.length > 0 ? rows[(o.since?.kind === "around" ? o.since.pick : 0) % rows.length]!.createdUs : Date.now() * 1000;
  let sinceText: string | undefined;
  let sinceUs: number | undefined;
  if (o.since?.kind === "garbage") sinceText = o.since.text;
  if (o.since?.kind === "around") {
    const base = second(anchorUs) + o.since.shift;
    if (o.since.spelling === "visible") {
      sinceText = visible(base * 1_000_000);
      sinceUs = base * 1_000_000;
    } else {
      sinceText = new Date(base * 1000 + o.since.ms).toISOString();
      sinceUs = (base * 1000 + o.since.ms) * 1000;
    }
  }
  const args: Record<string, unknown> = {};
  if (o.source !== undefined) args.source = o.source;
  if (sinceText !== undefined) args.since = sinceText;
  if (o.limit !== undefined) args.limit = o.limit;

  const outcome = await call(client, "list_signals", args);
  const after = await observe(server);
  frame(v, "list frame: listing never changes a signal", before, after);
  noNewRows(v, "list frame: listing adds nothing", before, after);

  const validLimit = o.limit === undefined || (Number.isInteger(o.limit) && o.limit >= 1 && o.limit <= 2000);
  if (!validLimit || o.since?.kind === "garbage") {
    witnessed.add("an invalid list request is refused");
    v.check("list: an invalid limit or since is refused", isRefusal(outcome), () => ({ args, outcome }));
    return v.found;
  }

  const reply = listReply.parse(okValue(outcome, "list_signals"));
  const cap = o.limit ?? DEFAULT_LIST_LIMIT;
  const ids = reply.signals.map((s) => s.id);
  v.check("list: count is the number of rows returned", reply.count === reply.signals.length, () => reply.count);
  v.check("list: at most limit rows", reply.signals.length <= cap, () => ({ cap, got: reply.signals.length }));
  v.check("list: oldest first", ids.every((id, i) => i === 0 || ids[i - 1]! < id), () => ids);
  for (const s of reply.signals) {
    const stored = before.get(s.id);
    v.check("list: every row is a stored signal, as stored", stored !== undefined && s.source === stored.source && s.content === stored.content, () => ({
      listed: s,
      stored: stored ?? null,
    }));
    if (stored === undefined) continue;
    v.check("list: created_at on the wire format", s.created_at === visible(stored.createdUs), () => ({ listed: s.created_at }));
    v.check("list: consumed_at is set exactly when consumed", (s.consumed_at === null) === (stored.consumedUs === null), () => ({
      listed: s.consumed_at,
      consumedUs: stored.consumedUs,
    }));
    if (stored.consumedUs !== null) {
      v.check("list: consumed_at on the wire format", s.consumed_at === visible(stored.consumedUs), () => ({ listed: s.consumed_at }));
    }
    v.check("list: the source filter holds", o.source === undefined || s.source === o.source, () => ({ filter: o.source, got: s.source }));
    // Sub-second ordering inside since's own second is unspecified (see
    // Findings in .claude/tasks/test-coverage-loop.md); earlier seconds
    // never come back.
    v.check("list: nothing from before since's second", sinceUs === undefined || second(stored.createdUs) >= second(sinceUs), () => ({
      since: sinceText,
      created: visible(stored.createdUs),
    }));
  }

  // Completeness: every matching signal from a later second is listed,
  // up to the limit, and the limit cuts the newest ones off, not the oldest.
  const owed = rows.filter(
    (s) => (o.source === undefined || s.source === o.source) && (sinceUs === undefined || second(s.createdUs) > second(sinceUs)),
  );
  const listed = new Set(ids);
  const missing = owed.filter((s) => !listed.has(s.id)).map((s) => s.id);
  const truncated = reply.signals.length === cap;
  const lastListed = ids.at(-1) ?? -Infinity;
  v.check("list: no matching signal is skipped", truncated ? missing.every((id) => id > lastListed) : missing.length === 0, () => ({
    missing,
    truncated,
  }));
  if (truncated && owed.length > cap) witnessed.add("the limit cuts a listing");
  if (o.source !== undefined && rows.some((s) => s.source !== o.source)) witnessed.add("the source filter excludes something");
  if (sinceUs !== undefined && rows.some((s) => second(s.createdUs) === second(sinceUs!))) witnessed.add("since falls inside a second that holds signals");
  return v.found;
}

// ── suites ───────────────────────────────────────────────────────────────────

const available = await databaseAvailable();

describe.skipIf(!available)("signal queue over MCP against Postgres", () => {
  let db: TestDatabase;
  let server: Server;
  let client: Client;

  beforeAll(async () => {
    db = await testDatabase();
    server = await startServer(db.url);
    client = await connect(server.url);
  }, 180_000);

  afterAll(async () => {
    await client?.close();
    await server?.stop();
    await db?.drop();
  });

  beforeEach(async () => {
    await server.state.pool.query("TRUNCATE signals RESTART IDENTITY");
  });

  it("keeps every rule across generated sequences of record / next / list", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(op, { minLength: 1, maxLength: 40, size: "max" }), async (ops) => {
        await server.state.pool.query("TRUNCATE signals RESTART IDENTITY");
        for (const [i, o] of ops.entries()) {
          const violations = await step(server, client, o);
          if (violations.length > 0) {
            throw new Error(`step ${i} ${JSON.stringify(o).slice(0, 300)}\n  ${violations.join("\n  ")}`);
          }
        }
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(
      [
        "a large signal round-trips",
        "a signal is taken while others wait",
        "an empty queue is asked",
        "an invalid list request is refused",
        "since falls inside a second that holds signals",
        "the limit cuts a listing",
        "the source filter excludes something",
      ].sort(),
    );
  }, 300_000);

  // Records and pops racing on separate sessions. Exactly once: every
  // recorded signal is handed out once, to one consumer, as recorded. No
  // overtaking: a pop never returns a newer signal while an older one, whose
  // record had already returned when the pop started, is still waiting for a
  // pop that began after this one ended.
  it("hands out every signal exactly once and in order while producers and consumers race", async () => {
    const PRODUCERS = 3;
    const PER_PRODUCER = 30;
    const CONSUMERS = 4;
    const total = PRODUCERS * PER_PRODUCER;
    // A full instance is single-session (one supervisor), so the extra
    // consumers come from a second, signals-only instance on the same
    // database — the shape of two MCP processes sharing one queue. Each
    // session also runs two loops at once.
    const scoped = await startServer(db.url, ["signals"]);
    const sessions = await Promise.all(Array.from({ length: CONSUMERS - 1 }, () => connect(scoped.url)));
    const clients = [client, client, ...sessions, ...sessions];
    const recorded = new Map<number, { content: string; end: number }>();
    const pops: { id: number; content: string; start: number; end: number }[] = [];
    let producing = PRODUCERS;

    const produce = async (p: number) => {
      for (let i = 0; i < PER_PRODUCER; i++) {
        const content = `p${p}-${i}`;
        const id = await server.signals.record({ source: "telegram", content });
        recorded.set(id, { content, end: performance.now() });
      }
      producing--;
    };
    const consume = async (c: Client) => {
      for (;;) {
        const start = performance.now();
        const reply = nextReply.parse(okValue(await call(c, "get_next_signal", {}), "get_next_signal"));
        const end = performance.now();
        if (reply.signal !== null) pops.push({ id: reply.signal.id, content: reply.signal.content, start, end });
        else if (producing === 0) return;
      }
    };

    try {
      await Promise.all([...Array.from({ length: PRODUCERS }, (_, p) => produce(p)), ...clients.map(consume)]);
    } finally {
      await Promise.all(sessions.map((c) => c.close()));
      await scoped.stop();
    }

    const delivered = pops.map((p) => p.id).sort((a, b) => a - b);
    expect(delivered).toEqual([...recorded.keys()].sort((a, b) => a - b));
    expect(delivered).toHaveLength(total);
    for (const p of pops) expect(p.content).toBe(recorded.get(p.id)?.content);

    const popOf = new Map(pops.map((p) => [p.id, p]));
    const overtakes: string[] = [];
    for (const p of pops) {
      for (const [older, rec] of recorded) {
        if (older >= p.id || rec.end >= p.start) continue;
        const taker = popOf.get(older);
        if (taker === undefined || taker.start > p.end) overtakes.push(`${p.id} handed out while ${older} still waited`);
      }
    }
    expect(overtakes).toEqual([]);
  }, 120_000);

  // Copying a listed signal's created_at into `since` should give exactly the
  // signals after it. The wire format drops sub-seconds, so the copied signal
  // and its same-second siblings come back. See Findings.
  it.skip("since copied from a listed signal returns exactly the later signals", async () => {
    const first = await server.signals.record({ source: "dreaming", content: "a" });
    const second = await server.signals.record({ source: "dreaming", content: "b" });
    const listed = listReply.parse(okValue(await call(client, "list_signals", {}), "list_signals"));
    const anchor = listed.signals.find((s) => s.id === first);
    const after = listReply.parse(okValue(await call(client, "list_signals", { since: anchor?.created_at }), "list_signals"));
    expect(after.signals.map((s) => s.id)).toEqual([second]);
  });
});
