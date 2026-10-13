import { randomBytes } from "node:crypto";
import { afterEach, describe, expect, it } from "vitest";
import { createScheduledTasks } from "../services/scheduler";
import { createSettings } from "../services/settings";
import { createSignals } from "../services/signals";
import { createTelegramLog } from "../services/telegram/storage";
import { connectStateDb, stateDatabaseUrl, type StateDb } from "./state";
import { testServerUrl } from "../../test/support/database";

// Runs against a throwaway server named by TEST_DATABASE_URL (never prod),
// or a container; skipped without either. Each test gets a fresh state database on it.

// TEST_DATABASE_URL, or a container of our own when Docker is there.
const TEST_URL = await testServerUrl();

const open: StateDb[] = [];
afterEach(async () => {
  await Promise.all(open.splice(0).map((db) => db.close()));
});

async function freshState(): Promise<StateDb> {
  const url = new URL(TEST_URL!);
  url.pathname = `/t_${randomBytes(8).toString("hex")}`;
  const db = await connectStateDb(TEST_URL!, url.toString());
  open.push(db);
  return db;
}

describe("stateDatabaseUrl", () => {
  it("swaps the database name of DATABASE_URL for mcp_state", () => {
    expect(stateDatabaseUrl("postgres://u:p@postgres:5432/mcp")).toBe("postgres://u:p@postgres:5432/mcp_state");
  });
});

describe.skipIf(!TEST_URL)("mcp_state", () => {
  it("creates the database, and a second boot finds it migrated", async () => {
    const db = await freshState();
    const url = new URL(TEST_URL!);
    url.pathname = new URL(db.pool.options.connectionString!).pathname;
    // mcp and mcp-tunnel boot at once against the same database.
    const [a, b] = await Promise.all([
      connectStateDb(TEST_URL!, url.toString()),
      connectStateDb(TEST_URL!, url.toString()),
    ]);
    open.push(a, b);
    const { rows } = await a.pool.query("SELECT version FROM state_migrations");
    expect(rows).toEqual([{ version: 1 }]);
  });

  it("seeds the system tasks once and respects a cancellation", async () => {
    const db = await freshState();
    const count = async () => (await db.pool.query("SELECT count(*) AS n FROM scheduled_tasks")).rows[0].n;
    expect(await count()).toBe(3);
    await db.pool.query("DELETE FROM scheduled_tasks WHERE source = 'dreaming'");

    const again = await connectStateDb(TEST_URL!, db.pool.options.connectionString!);
    open.push(again);
    expect(await count()).toBe(2);
  });

  it("pops oldest first, once, with the timestamps the agent has always seen", async () => {
    const signals = createSignals(await freshState());
    const first = await signals.record({ source: "telegram", content: "a" });
    await signals.record({ source: "gmail", content: "b" });

    const popped = await signals.popNext();
    expect(popped).toMatchObject({ id: first, source: "telegram", content: "a" });
    expect(popped!.created_at).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
    expect(await signals.countPending()).toBe(1);

    const all = await signals.list({});
    expect(all.map((s) => [s.content, s.consumed_at === null])).toEqual([
      ["a", false],
      ["b", true],
    ]);
  });

  // The sqlite era compared `since` as text, so a same-day ISO string lost to
  // the "YYYY-MM-DD HH:MM:SS" format. Now it is a real time comparison.
  it("lists signals after a real instant", async () => {
    const db = await freshState();
    const signals = createSignals(db);
    await db.pool.query(
      `INSERT INTO signals (source, content, created_at) VALUES
         ('telegram', 'old', '2026-10-01T09:00:00Z'), ('telegram', 'new', '2026-10-01T10:00:01Z')`,
    );

    const rows = await signals.list({ since: new Date("2026-10-01T10:00:00.000Z") });

    expect(rows.map((r) => [r.content, r.created_at])).toEqual([["new", "2026-10-01 10:00:01"]]);
  });

  it("keeps settings, tasks and the chat log in the shapes the tools return", async () => {
    const db = await freshState();
    const settings = createSettings(db);
    expect(await settings.timezone()).toBe("UTC");
    await settings.setTimezone("Europe/Kiev");
    expect(await settings.timezone()).toBe("Europe/Kiev");
    await expect(settings.setTimezone("Mars/Olympus")).rejects.toThrow();

    const tasks = createScheduledTasks(db);
    const task = await tasks.insert({ cron_expr: "30 14 2 10 *", recurring: false, prompt: "take pills" });
    expect(task).toMatchObject({ cron_expr: "30 14 2 10 *", recurring: 0, source: null, last_run_at: null });
    expect(typeof task.id).toBe("number");
    expect(await tasks.swapLastRun(task.id, null, 1790951400)).toBe(true);
    expect(await tasks.swapLastRun(task.id, null, 1790951400)).toBe(false);
    expect((await tasks.listActive()).map((t) => t.id)).not.toContain(task.id);
    expect(await tasks.delete(task.id)).toBe(true);
    expect(await tasks.delete(task.id)).toBe(false);

    const log = createTelegramLog(db);
    await log.record({ chatId: 7, tgMessageId: 1, threadId: null, role: "user", text: "general" });
    await log.record({ chatId: 7, tgMessageId: 2, threadId: 5, role: "assistant", text: "topic" });
    expect((await log.history(7)).map((m) => m.text)).toEqual(["general", "topic"]);
    expect((await log.history(7, 50, null)).map((m) => m.text)).toEqual(["general"]);
    expect((await log.history(7, 50, 5)).map((m) => m.text)).toEqual(["topic"]);
    expect(await log.lastUpdateId()).toBeNull();
    await log.setLastUpdateId(555);
    expect(await log.lastUpdateId()).toBe(555);
  });
});
