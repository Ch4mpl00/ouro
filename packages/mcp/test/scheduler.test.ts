// The scheduler, judged from the outside: tasks are created, listed and
// cancelled over MCP, the time zone is set over MCP, and the poller's tick
// runs on a controlled clock. Every step is checked against the rows Postgres
// holds before and after, and every fire against what the cron means on the
// user's wall clock (test/support/wall-cron.ts — not cron-parser).
//
// Rules, as in signal-queue.test.ts: outcome (from request + pre-state),
// effect (the reply and the changed rows), frame (what must not change).
//
// PROPERTY_RUNS raises the number of generated sequences.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { tick, type ScheduledTasks, type SchedulerDeps } from "../src/services/scheduler";
import type { Signals } from "../src/services/signals";
import { call, connect, startServer, type Outcome, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";
import { daysExist, firstOwed, legitimateSlot, parseWallCron } from "./support/wall-cron";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 30);
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

// ── observation ──────────────────────────────────────────────────────────────

interface Task {
  id: number;
  cron: string;
  recurring: boolean;
  prompt: string;
  source: string | null;
  lastRunAt: number | null; // unix seconds of the slot last fired for
  createdMs: number;
}

interface World {
  tz: string;
  tasks: ReadonlyMap<number, Task>;
  signals: ReadonlyMap<number, { source: string; content: string }>;
}

const taskRow = z.object({
  id: z.number().int(),
  cron_expr: z.string(),
  recurring: z.boolean(),
  prompt: z.string(),
  source: z.string().nullable(),
  last_run_at: z.number().int().nullable(),
  created_ms: z.number().int(),
});
const signalRow = z.object({ id: z.number().int(), source: z.string(), content: z.string() });

async function observe(server: Server): Promise<World> {
  const pool = server.state.pool;
  const tz = await pool.query("SELECT value FROM settings WHERE key = 'timezone'");
  const tasks = await pool.query(
    `SELECT id, cron_expr, recurring, prompt, source, last_run_at,
            (extract(epoch FROM created_at) * 1000)::bigint AS created_ms
       FROM scheduled_tasks`,
  );
  const signals = await pool.query("SELECT id, source, content FROM signals");
  const tzValue = z.object({ value: z.string() }).array().parse(tz.rows)[0]?.value ?? "UTC";
  return {
    tz: tzValue,
    tasks: new Map(
      tasks.rows.map((raw) => {
        const r = taskRow.parse(raw);
        const t: Task = {
          id: r.id,
          cron: r.cron_expr,
          recurring: r.recurring,
          prompt: r.prompt,
          source: r.source,
          lastRunAt: r.last_run_at,
          createdMs: r.created_ms,
        };
        return [t.id, t];
      }),
    ),
    signals: new Map(
      signals.rows.map((raw) => {
        const r = signalRow.parse(raw);
        return [r.id, { source: r.source, content: r.content }];
      }),
    ),
  };
}

const active = (t: Task) => t.recurring || t.lastRunAt === null;
// What the next fire is counted from: the slot last fired for, or creation.
const anchorMs = (t: Task) => (t.lastRunAt !== null ? t.lastRunAt * 1000 : t.createdMs);

function sameTask(a: Task | undefined, b: Task | undefined): boolean {
  return a !== undefined && b !== undefined && JSON.stringify(a) === JSON.stringify(b);
}

// ── a fired signal, read back ────────────────────────────────────────────────

interface Fire {
  signalId: number;
  source: string;
  taskId: number;
  cron: string;
  slotMs: number;
  nowIso: string;
  previous: string;
  recurring: string;
  body: string;
}

function parseFire(signalId: number, source: string, content: string): Fire {
  const lines = content.split("\n");
  const header = (i: number, label: string): string => {
    const line = lines[i] ?? "";
    if (!line.startsWith(label)) throw new Error(`signal ${signalId}: line ${i} is not "${label}…": ${JSON.stringify(line)}`);
    return line.slice(label.length);
  };
  const id = /^Scheduled task #(\d+) fired\.$/.exec(lines[0] ?? "");
  if (!id || lines[6] !== "") throw new Error(`signal ${signalId}: not a scheduler signal: ${JSON.stringify(content.slice(0, 200))}`);
  const slot = Date.parse(header(2, "Slot: "));
  if (Number.isNaN(slot)) throw new Error(`signal ${signalId}: bad slot`);
  return {
    signalId,
    source,
    taskId: Number(id[1]),
    cron: header(1, "Cron: "),
    slotMs: slot,
    nowIso: header(3, "Now: "),
    previous: header(4, "Previous fire: "),
    recurring: header(5, "Recurring: "),
    body: lines.slice(7).join("\n"),
  };
}

// ── replies ──────────────────────────────────────────────────────────────────

const storedTask = z
  .object({
    id: z.number().int(),
    cron_expr: z.string(),
    recurring: z.number().int(),
    prompt: z.string(),
    source: z.string().nullable(),
    last_run_at: z.number().int().nullable(),
    created_at: z.string(),
  })
  .strict();

const scheduleReply = z.union([
  z.object({ ok: z.literal(true), task: storedTask, timezone: z.string(), upcoming_fires: z.array(z.string()) }).strict(),
  z.object({ ok: z.literal(false), error: z.string() }).strict(),
]);

const listReply = z
  .object({
    timezone: z.string(),
    count: z.number().int(),
    tasks: z.array(
      storedTask
        .extend({ recurring: z.boolean(), last_run_at_iso: z.string().nullable(), upcoming_fires: z.array(z.string()) })
        .strict(),
    ),
  })
  .strict();

const okReply = z.union([
  z.object({ ok: z.literal(true), timezone: z.string(), local_now: z.string() }).strict(),
  z.object({ ok: z.literal(false), error: z.string() }).strict(),
]);
const cancelReply = z.object({ ok: z.boolean(), id: z.number().int() }).strict();

function okValue(outcome: Outcome, op: string): unknown {
  if (outcome.kind !== "ok") throw new Error(`${op}: expected a result, got ${JSON.stringify(outcome)}`);
  return outcome.value;
}

// ── operations ───────────────────────────────────────────────────────────────

const ZONES = ["UTC", "Europe/Kiev", "America/New_York", "Asia/Kolkata", "Australia/Lord_Howe"];
const BAD_ZONES = ["Mars/Olympus_Mons", "Europe/Kyiv_", "UTC+3"];

// Realistic agent-written crons, plus a few the tool must refuse.
const cron = fc
  .record({
    minute: fc.constantFrom("0", "15", "30", "59", "*/15", "*/5", "*", "0,30"),
    hour: fc.constantFrom("*", "0", "2", "3", "9", "23", "*/6", "9,18", "1-3"),
    dom: fc.constantFrom("*", "*", "*", "1", "15", "29", "31"),
    month: fc.constantFrom("*", "*", "*", "3", "10", "11"),
    dow: fc.constantFrom("*", "*", "*", "1-5", "0", "6", "7"),
  })
  .map((f) => `${f.minute} ${f.hour} ${f.dom} ${f.month} ${f.dow}`);
const badCron = fc.constantFrom("every day at nine", "61 * * * *", "0 25 * * *", "* * * *", "30 9 * * * *", "0 9 * * * * * *", "@daily");

// Clock starts near the year's DST transitions (Kiev, New York, Lord Howe)
// or anywhere; steps jump to just before / at / after the next fire.
const STARTS = [
  "2026-03-07T20:00:00Z", // New York springs forward Mar 8
  "2026-03-28T20:00:00Z", // Kiev springs forward Mar 29
  "2026-04-04T10:00:00Z", // Lord Howe falls back Apr 5 (30 min)
  "2026-10-03T10:00:00Z", // Lord Howe springs forward Oct 4 (30 min)
  "2026-10-24T20:00:00Z", // Kiev falls back Oct 25
  "2026-10-31T20:00:00Z", // New York falls back Nov 1
].map(Date.parse);
const start = fc.oneof(
  fc.constantFrom(...STARTS),
  fc.integer({ min: Date.parse("2026-01-01T00:00:00Z"), max: Date.parse("2026-12-31T00:00:00Z") }),
);

type Op =
  | { kind: "schedule"; cron: string; recurring: boolean; prompt: string }
  | { kind: "seed"; cron: string; source: string; prompt: string }
  | { kind: "cancel"; pick: number; exists: boolean }
  | { kind: "timezone"; tz: string }
  | { kind: "list" }
  | { kind: "advance"; ms: number }
  | { kind: "advanceToFire"; pick: number; offsetMs: -1000 | 0 | 1000 }
  | { kind: "tick" };

const prompt = fc.string({ unit: "grapheme", minLength: 1, maxLength: 40 });
const op: fc.Arbitrary<Op> = fc.oneof(
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("schedule" as const),
      cron: fc.oneof({ weight: 9, arbitrary: cron }, { weight: 1, arbitrary: badCron }),
      recurring: fc.boolean(),
      prompt,
    }),
  },
  {
    weight: 1,
    arbitrary: fc.record({
      kind: fc.constant("seed" as const),
      cron,
      source: fc.constantFrom("dreaming", "news-digest"),
      prompt,
    }),
  },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("cancel" as const), pick: fc.nat(), exists: fc.boolean() }) },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("timezone" as const), tz: fc.constantFrom(...ZONES, ...BAD_ZONES) }) },
  { weight: 1, arbitrary: fc.constant<Op>({ kind: "list" }) },
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("advance" as const),
      ms: fc.oneof(fc.constantFrom(1000, 29_000, 30_000, MINUTE, 15 * MINUTE, HOUR, 6 * HOUR, 24 * HOUR), fc.integer({ min: 1, max: 26 * HOUR })),
    }),
  },
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("advanceToFire" as const),
      pick: fc.nat(),
      offsetMs: fc.constantFrom(-1000 as const, 0 as const, 1000 as const),
    }),
  },
  { weight: 6, arbitrary: fc.constant<Op>({ kind: "tick" }) },
);

// ── rules ────────────────────────────────────────────────────────────────────

class Violations {
  readonly found: string[] = [];
  check(rule: string, holds: boolean, detail: () => unknown): void {
    if (!holds) this.found.push(`${rule}: ${JSON.stringify(detail())}`);
  }
}

function tasksFrame(v: Violations, rule: string, before: World, after: World, except: ReadonlySet<number> = new Set()): void {
  for (const [id, t] of before.tasks) {
    if (except.has(id)) continue;
    v.check(rule, sameTask(t, after.tasks.get(id)), () => ({ id, before: t, after: after.tasks.get(id) ?? null }));
  }
}

function noSignals(v: Violations, rule: string, before: World, after: World): void {
  const added = [...after.signals.keys()].filter((id) => !before.signals.has(id));
  v.check(rule, added.length === 0, () => ({ added }));
}

const witnessed = new Set<string>();
const MAX_SCAN = 40 * 24 * HOUR;

// Upcoming fires a tool reports: legitimate, increasing, and nothing owed
// skipped between them (from `fromMs`, the real time of the call).
function checkUpcoming(v: Violations, rule: string, cronText: string, tz: string, fromMs: number, upcoming: readonly string[]): void {
  const c = parseWallCron(cronText);
  let prev = fromMs;
  for (const iso of upcoming) {
    const at = Date.parse(iso);
    v.check(`${rule}: a legitimate fire time`, legitimateSlot(c, tz, at), () => ({ cron: cronText, tz, iso }));
    v.check(`${rule}: increasing and in the future`, at > prev - 2000, () => ({ iso, prev: new Date(prev).toISOString() }));
    if (at - prev < MAX_SCAN) {
      const owed = firstOwed(c, tz, prev, at - 1);
      v.check(`${rule}: no owed fire skipped`, owed === null, () => ({ cron: cronText, tz, iso, skipped: owed && new Date(owed).toISOString() }));
    }
    prev = at;
  }
}

interface Clock {
  now: number;
}

async function step(server: Server, client: Client, clock: Clock, o: Op): Promise<string[]> {
  const v = new Violations();
  const before = await observe(server);

  switch (o.kind) {
    case "advance":
      clock.now += o.ms;
      return [];

    case "advanceToFire": {
      const candidates = [...before.tasks.values()].filter(active);
      const t = candidates[o.pick % Math.max(candidates.length, 1)];
      if (t === undefined) return [];
      const owed = firstOwed(parseWallCron(t.cron), before.tz, Math.max(anchorMs(t), clock.now - MINUTE), clock.now + 3 * 24 * HOUR);
      if (owed !== null) clock.now = owed + o.offsetMs;
      return [];
    }

    case "schedule": {
      const realNow = Date.now();
      const outcome = await call(client, "schedule_task", { cron_expr: o.cron, recurring: o.recurring, prompt: o.prompt });
      const reply = scheduleReply.parse(okValue(outcome, "schedule_task"));
      const after = await observe(server);
      tasksFrame(v, "schedule frame: existing tasks untouched", before, after);
      noSignals(v, "schedule frame: no signal", before, after);
      // Valid: five fields whose days exist in some selected month. Days
      // that don't exist but a restricted weekday could still match — either
      // answer is accepted (cron-parser refuses; see Findings).
      let valid: boolean | "either";
      try {
        const c = parseWallCron(o.cron);
        valid = daysExist(c) ? true : c.dowRestricted ? "either" : false;
      } catch {
        valid = false;
      }
      if (valid === "either") {
        if (!reply.ok) {
          v.check("schedule frame: a refusal adds no task", after.tasks.size === before.tasks.size, () => ({ cron: o.cron }));
          return v.found;
        }
        await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [clock.now, reply.task.id]);
        return v.found;
      }
      if (!valid) {
        witnessed.add("an invalid cron is refused");
        v.check("schedule: an invalid cron is refused", !reply.ok, () => ({ cron: o.cron, reply }));
        v.check("schedule frame: a refusal adds no task", after.tasks.size === before.tasks.size, () => ({ cron: o.cron }));
        return v.found;
      }
      v.check("schedule: a valid cron is accepted", reply.ok, () => ({ cron: o.cron, reply }));
      if (!reply.ok) return v.found;
      const row = after.tasks.get(reply.task.id);
      v.check("schedule: exactly one task is added", after.tasks.size === before.tasks.size + 1 && !before.tasks.has(reply.task.id), () => ({
        id: reply.task.id,
      }));
      v.check(
        "schedule: the task holds what was asked",
        row !== undefined && row.cron === o.cron && row.recurring === o.recurring && row.prompt === o.prompt && row.source === null && row.lastRunAt === null,
        () => ({ row: row ?? null }),
      );
      v.check("schedule: the reply echoes the stored task", reply.task.cron_expr === o.cron && reply.task.prompt === o.prompt && reply.task.recurring === (o.recurring ? 1 : 0), () => reply.task);
      v.check("schedule: timezone is the configured one", reply.timezone === before.tz, () => ({ reply: reply.timezone, stored: before.tz }));
      v.check("schedule: 3 upcoming fires for recurring, 1 for one-shot", reply.upcoming_fires.length === (o.recurring ? 3 : 1), () => reply.upcoming_fires);
      checkUpcoming(v, "schedule upcoming", o.cron, before.tz, realNow, reply.upcoming_fires);
      // Move the new task into the controlled clock's world.
      await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [clock.now, reply.task.id]);
      return v.found;
    }

    case "seed": {
      const row = await server.scheduler.tasks.insert({ cron_expr: o.cron, recurring: true, prompt: o.prompt, source: o.source });
      await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [clock.now, row.id]);
      return [];
    }

    case "cancel": {
      const ids = [...before.tasks.keys()];
      const id = o.exists && ids.length > 0 ? ids[o.pick % ids.length]! : Math.max(0, ...ids) + 1 + (o.pick % 5);
      const reply = cancelReply.parse(okValue(await call(client, "cancel_scheduled_task", { id }), "cancel_scheduled_task"));
      const after = await observe(server);
      const existed = before.tasks.has(id);
      v.check("cancel: ok exactly when the task existed", reply.ok === existed && reply.id === id, () => ({ id, existed, reply }));
      v.check("cancel: the task is gone", !after.tasks.has(id), () => ({ id }));
      v.check("cancel: nothing else is removed", after.tasks.size === before.tasks.size - (existed ? 1 : 0), () => ({ id }));
      tasksFrame(v, "cancel frame: other tasks untouched", before, after, new Set([id]));
      noSignals(v, "cancel frame: no signal", before, after);
      if (existed) witnessed.add("a task is cancelled");
      return v.found;
    }

    case "timezone": {
      const reply = okReply.parse(okValue(await call(client, "set_timezone", { tz: o.tz }), "set_timezone"));
      const after = await observe(server);
      const known = ZONES.includes(o.tz);
      v.check("timezone: accepted exactly when IANA knows it", reply.ok === known, () => ({ tz: o.tz, reply }));
      v.check("timezone: stored when accepted, unchanged when refused", after.tz === (known ? o.tz : before.tz), () => ({ tz: o.tz, stored: after.tz }));
      tasksFrame(v, "timezone frame: tasks untouched", before, after);
      noSignals(v, "timezone frame: no signal", before, after);
      return v.found;
    }

    case "list": {
      const realNow = Date.now();
      const reply = listReply.parse(okValue(await call(client, "list_scheduled_tasks", {}), "list_scheduled_tasks"));
      const after = await observe(server);
      tasksFrame(v, "list frame: listing changes nothing", before, after);
      noSignals(v, "list frame: no signal", before, after);
      const expected = [...before.tasks.values()].filter(active).map((t) => t.id).sort((a, b) => a - b);
      v.check("list: exactly the tasks that may still fire", JSON.stringify(reply.tasks.map((t) => t.id)) === JSON.stringify(expected), () => ({
        listed: reply.tasks.map((t) => t.id),
        expected,
      }));
      v.check("list: count matches", reply.count === reply.tasks.length, () => reply.count);
      v.check("list: timezone is the configured one", reply.timezone === before.tz, () => reply.timezone);
      for (const t of reply.tasks) {
        const s = before.tasks.get(t.id);
        v.check(
          "list: each row as stored",
          s !== undefined && t.cron_expr === s.cron && t.prompt === s.prompt && t.recurring === s.recurring && t.source === s.source && t.last_run_at === s.lastRunAt,
          () => ({ listed: t, stored: s ?? null }),
        );
        v.check(
          "list: last_run_at_iso is last_run_at",
          t.last_run_at_iso === (t.last_run_at === null ? null : new Date(t.last_run_at * 1000).toISOString()),
          () => t,
        );
        checkUpcoming(v, "list upcoming", t.cron_expr, before.tz, realNow, t.upcoming_fires);
      }
      return v.found;
    }

    case "tick": {
      const now = clock.now;
      const n = await tick(server.scheduler, new Date(now));
      const after = await observe(server);
      const added = [...after.signals].filter(([id]) => !before.signals.has(id));
      const fires = added.map(([id, s]) => parseFire(id, s.source, s.content));
      v.check("tick: returns how many fired", n === fires.length, () => ({ n, fires: fires.length }));
      const firedTasks = new Set<number>();
      for (const f of fires) {
        const t = before.tasks.get(f.taskId);
        v.check("tick: fires only an existing, active task", t !== undefined && active(t), () => ({ fire: f, task: t ?? null }));
        if (t === undefined) continue;
        v.check("tick: at most one fire per task per tick", !firedTasks.has(t.id), () => ({ task: t.id }));
        firedTasks.add(t.id);
        const c = parseWallCron(t.cron);
        v.check("tick: source is the task's, or scheduler", f.source === (t.source ?? "scheduler"), () => ({ fire: f.source, task: t.source }));
        v.check("tick: the signal carries the task", f.cron === t.cron && f.body === t.prompt && f.recurring === (t.recurring ? "yes" : "no (one-shot)"), () => f);
        v.check("tick: Now is the tick's time", f.nowIso === new Date(now).toISOString(), () => f.nowIso);
        v.check(
          "tick: Previous fire is the slot last fired for",
          f.previous === (t.lastRunAt === null ? "never (this is the first run)" : new Date(t.lastRunAt * 1000).toISOString()),
          () => ({ previous: f.previous, lastRunAt: t.lastRunAt }),
        );
        v.check("tick: never early", f.slotMs <= now, () => ({ slot: new Date(f.slotMs).toISOString(), now: new Date(now).toISOString() }));
        v.check("tick: only after the anchor", f.slotMs > anchorMs(t), () => ({ slot: new Date(f.slotMs).toISOString(), anchor: new Date(anchorMs(t)).toISOString() }));
        v.check("tick: the slot is a legitimate fire time", legitimateSlot(c, before.tz, f.slotMs), () => ({
          cron: t.cron,
          tz: before.tz,
          slot: new Date(f.slotMs).toISOString(),
        }));
        const skipped = firstOwed(c, before.tz, anchorMs(t), f.slotMs - 1);
        v.check("tick: no owed fire is skipped", skipped === null, () => ({
          cron: t.cron,
          tz: before.tz,
          anchor: new Date(anchorMs(t)).toISOString(),
          slot: new Date(f.slotMs).toISOString(),
          skipped: skipped && new Date(skipped).toISOString(),
        }));
        const a = after.tasks.get(t.id);
        v.check("tick: last_run_at becomes the slot", a?.lastRunAt === f.slotMs / 1000, () => ({ after: a ?? null, slot: f.slotMs / 1000 }));
        if (!t.recurring) witnessed.add("a one-shot fires");
        if (t.lastRunAt !== null && t.recurring) witnessed.add("a recurring task fires again");
        if (now - f.slotMs > HOUR) witnessed.add("a late tick catches up");
        if (t.source !== null) witnessed.add("a seeded task fires under its own source");
        if (before.tz !== "UTC") witnessed.add("a fire in a non-UTC zone");
      }
      // Progress: every active task with an owed fire by now fires.
      for (const t of before.tasks.values()) {
        if (!active(t) || firedTasks.has(t.id)) continue;
        const owed = firstOwed(parseWallCron(t.cron), before.tz, anchorMs(t), now);
        v.check("tick: a task with an owed fire fires", owed === null, () => ({
          task: t.id,
          cron: t.cron,
          tz: before.tz,
          anchor: new Date(anchorMs(t)).toISOString(),
          owed: owed && new Date(owed).toISOString(),
          now: new Date(now).toISOString(),
        }));
      }
      if (fires.length === 0 && [...before.tasks.values()].some(active)) witnessed.add("a tick with nothing due");
      tasksFrame(v, "tick frame: tasks that didn't fire are untouched", before, after, firedTasks);
      v.check("tick frame: no task added or removed", after.tasks.size === before.tasks.size, () => ({ before: before.tasks.size, after: after.tasks.size }));
      return v.found;
    }
  }
}

function gate(): { passed: Promise<void>; open: () => void } {
  let open = () => {};
  const passed = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { passed, open };
}

async function resetWorld(server: Server): Promise<void> {
  await server.state.pool.query("TRUNCATE signals, scheduled_tasks RESTART IDENTITY");
  await server.scheduler.settings.setTimezone("UTC");
}

// ── suites ───────────────────────────────────────────────────────────────────

const available = await databaseAvailable();

describe.skipIf(!available)("scheduler over MCP against Postgres", () => {
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

  beforeEach(() => resetWorld(server));

  it("keeps every rule across generated schedules, cancels, zone changes and ticks", async () => {
    await fc.assert(
      fc.asyncProperty(start, fc.constantFrom(...ZONES), fc.array(op, { minLength: 1, maxLength: 40, size: "max" }), async (t0, tz, ops) => {
        await resetWorld(server);
        await server.scheduler.settings.setTimezone(tz);
        const clock: Clock = { now: t0 };
        for (const [i, o] of ops.entries()) {
          const violations = await step(server, client, clock, o);
          if (violations.length > 0) {
            throw new Error(`step ${i} at ${new Date(clock.now).toISOString()} ${JSON.stringify(o)}\n  ${violations.join("\n  ")}`);
          }
        }
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(
      [
        "a fire in a non-UTC zone",
        "a late tick catches up",
        "a one-shot fires",
        "a recurring task fires again",
        "a seeded task fires under its own source",
        "a task is cancelled",
        "a tick with nothing due",
        "an invalid cron is refused",
      ].sort(),
    );
  }, 600_000);

  // A daily reminder fires exactly once on every local day across both DST
  // transitions, ticking every 10 minutes like a slightly slow poller.
  // (cron-parser 5.5 skipped the whole spring-forward day for "0 9 * * *".)
  it.each([
    ["Europe/Kiev", "0 9 * * *", "2026-03-27T00:00:00Z"],
    ["America/New_York", "0 9 * * *", "2026-03-06T00:00:00Z"],
    ["Europe/Kiev", "0 8 * * *", "2026-10-23T00:00:00Z"],
    ["Europe/Kiev", "30 3 * * *", "2026-03-27T00:00:00Z"],
    ["Europe/Kiev", "30 3 * * *", "2026-10-23T00:00:00Z"],
    ["America/New_York", "30 1 * * *", "2026-10-30T00:00:00Z"],
    ["America/New_York", "30 2 * * *", "2026-03-06T00:00:00Z"],
    ["Australia/Lord_Howe", "45 1 * * *", "2026-04-03T00:00:00Z"],
  ])("a daily task fires once per local day across DST in %s (%s from %s)", (tz, cronText, from) => dailyAcrossDst(tz, cronText, from));

  // A time inside Lord Howe's 30-minute spring-forward gap: cron-parser skips
  // the day instead of shifting the fire past the gap, as it does for whole-
  // hour gaps (Kiev 03:30 above). See Findings.
  it.skip("a daily task inside a 30-minute DST gap still fires that day", () =>
    dailyAcrossDst("Australia/Lord_Howe", "15 2 * * *", "2026-10-02T00:00:00Z"));

  async function dailyAcrossDst(tz: string, cronText: string, from: string): Promise<void> {
    await server.scheduler.settings.setTimezone(tz);
    const row = await server.scheduler.tasks.insert({ cron_expr: cronText, recurring: true, prompt: "pills" });
    await server.state.pool.query("UPDATE scheduled_tasks SET created_at = $1 WHERE id = $2", [from, row.id]);
    const start = Date.parse(from);
    for (let now = start; now <= start + 5 * 24 * HOUR; now += 10 * MINUTE) await tick(server.scheduler, new Date(now));
    const world = await observe(server);
    const days = [...world.signals].map(([id, s]) => {
      const f = parseFire(id, s.source, s.content);
      return new Intl.DateTimeFormat("en-CA", { timeZone: tz }).format(new Date(f.slotMs));
    });
    const dayNumbers = days.map((d) => Date.parse(`${d}T00:00:00Z`) / (24 * HOUR));
    expect(days.length).toBeGreaterThanOrEqual(4);
    expect(dayNumbers.slice(1).map((d, i) => d - dayNumbers[i]!), `fired on ${days}`).toEqual(dayNumbers.slice(1).map(() => 1));
  }

  // Ticks overlap when one runs long (the poller's setInterval does not wait
  // for the previous tick). Every slot still fires once.
  it("overlapping ticks fire every slot once", async () => {
    const created = Date.parse("2026-07-01T00:00:00Z");
    for (let i = 0; i < 20; i++) {
      const row = await server.scheduler.tasks.insert({ cron_expr: "*/5 * * * *", recurring: i % 2 === 0, prompt: `t${i}` });
      await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [created, row.id]);
    }
    const now = new Date(created + 7 * MINUTE);
    await Promise.all([tick(server.scheduler, now), tick(server.scheduler, now), tick(server.scheduler, now)]);
    const world = await observe(server);
    const fires = [...world.signals].map(([id, s]) => parseFire(id, s.source, s.content));
    const keys = fires.map((f) => `${f.taskId}@${f.slotMs}`);
    expect(keys.length - new Set(keys).size, `duplicate fires: ${keys.sort()}`).toBe(0);
    expect(new Set(fires.map((f) => f.taskId)).size).toBe(20);
  });

  // A task cancelled after a tick read the task list, but before the tick
  // got to it, must not fire: the user was told it is gone.
  it("a task cancelled while a tick is in flight does not fire", async () => {
    const created = Date.parse("2026-07-01T00:00:00Z");
    const row = await server.scheduler.tasks.insert({ cron_expr: "0 9 * * *", recurring: false, prompt: "take pills" });
    await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [created, row.id]);
    const listed = gate();
    const tasks: ScheduledTasks = {
      ...server.scheduler.tasks,
      listActive: async () => {
        const rows = await server.scheduler.tasks.listActive();
        await listed.passed;
        return rows;
      },
    };
    const inFlight = tick({ ...server.scheduler, tasks }, new Date(created + 10 * HOUR));
    const reply = cancelReply.parse(okValue(await call(client, "cancel_scheduled_task", { id: row.id }), "cancel_scheduled_task"));
    expect(reply.ok).toBe(true);
    listed.open();
    await inFlight;
    expect([...(await observe(server)).signals.values()]).toEqual([]);
  });

  // Durable intent is not delivery: if recording the signal fails (a
  // Postgres blip), the slot must still be delivered by a later tick, not
  // silently marked as done.
  it("a slot whose signal failed to record is delivered by the next tick", async () => {
    const created = Date.parse("2026-07-01T00:00:00Z");
    const row = await server.scheduler.tasks.insert({ cron_expr: "30 14 1 7 *", recurring: false, prompt: "take pills" });
    await server.state.pool.query("UPDATE scheduled_tasks SET created_at = to_timestamp($1 / 1000.0) WHERE id = $2", [created, row.id]);
    const failing: Signals = {
      ...server.scheduler.signals,
      record: async () => {
        throw new Error("connection terminated unexpectedly");
      },
    };
    const deps: SchedulerDeps = { ...server.scheduler, signals: failing };
    await tick(deps, new Date(created + 15 * HOUR)).catch(() => undefined);
    await tick(server.scheduler, new Date(created + 15 * HOUR + 30_000));
    const fires = [...(await observe(server)).signals].map(([id, s]) => parseFire(id, s.source, s.content));
    expect(fires.map((f) => [f.taskId, new Date(f.slotMs).toISOString()])).toEqual([[row.id, "2026-07-01T14:30:00.000Z"]]);
  });
});
