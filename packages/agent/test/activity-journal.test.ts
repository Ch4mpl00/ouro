// The activity journal: completed production traces become bounded packets
// (projection), the packets become one validated note each, and the worker
// appends them to the shared journal through MCP — on a real agent.db file,
// with the LLM and the MCP call at the boundary.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { sql } from "drizzle-orm";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  ACTIVITY_STREAM,
  createActivityJournalWorker,
  createActivityProgressStore,
  DEFAULT_ACTIVITY_JOURNAL_OPTS,
  DEFAULT_PROJECTION_BUDGET,
  projectTrace,
  type ActivityPacket,
  type NoteGenerator,
} from "../src/activity-journal";
import { createAgentDb, type AgentDatabase } from "../src/db/client";
import { createMemoryStore } from "../src/db/memory";
import { createTraceStore, type StoredTraceInput } from "../src/db/trace-store";
import type { Observation, Trace } from "../src/trace-model";

let dir: string;
beforeAll(() => {
  dir = mkdtempSync(path.join(tmpdir(), "activity-journal-"));
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

// ─── trace builders ─────────────────────────────────────────────────

let seq = 0;
function obs(fields: Partial<Observation> & Pick<Observation, "type">): Observation {
  seq += 1;
  return {
    id: `o${seq}`,
    name: `obs-${seq}`,
    parentObservationId: "root",
    startTime: new Date(Date.UTC(2026, 9, 13, 10, 0, seq)).toISOString(),
    endTime: new Date(Date.UTC(2026, 9, 13, 10, 0, seq)).toISOString(),
    level: "DEFAULT",
    statusMessage: null,
    input: null,
    output: null,
    metadata: null,
    model: null,
    modelParameters: null,
    usage: null,
    usageDetails: null,
    calculatedTotalCost: null,
    latency: 0,
    ...fields,
  };
}

const root = (): Observation => obs({ id: "root", type: "AGENT", parentObservationId: null, name: "signal:telegram" });

function tool(name: string, input: unknown, output: unknown, extra: Partial<Observation> = {}): Observation {
  return obs({ type: "TOOL", name, input, output, ...extra, metadata: { tool: name, ...extra.metadata } });
}

function trace(id: string, fields: Partial<Trace> = {}): Trace {
  return {
    id,
    name: "signal:telegram",
    sessionId: id,
    timestamp: "2026-10-13T10:00:00.000Z",
    input: "hello",
    output: "done",
    metadata: { activity: "run", skills: ["telegram", "orchestrator", "routing"] },
    observations: [],
    latency: 0,
    totalCost: 0,
    tags: ["telegram", "agent-loop"],
    ...fields,
  };
}

const project = (t: Trace, observations: Observation[]) => projectTrace(t, observations, "2026-10-13 10:05:00");

// ─── projection ─────────────────────────────────────────────────────

describe("projection", () => {
  it("keeps a news run with hundreds of full articles within budget, without bodies or LLM histories", () => {
    const BODY = "ARTICLE-BODY-SENTINEL ".repeat(400);
    const articles = (n: number) =>
      JSON.stringify({ count: 100, items: Array.from({ length: 100 }, (_, i) => ({ id: n * 100 + i, title: `Title ${i}`, url: `https://x/${i}`, body: BODY })) });
    const worker = obs({ type: "AGENT", name: "invoke_sub_agent", input: { task: "Make the news digest", skill: "news-digest" }, output: "Digest: 5 stories…", metadata: { tool: "invoke_sub_agent" } });
    const observations = [
      root(),
      obs({ type: "GENERATION", input: [{ role: "user", content: "INTERMEDIATE-HISTORY-SENTINEL" }], output: { content: "thinking" } }),
      worker,
      ...[0, 1, 2, 3, 4].map((n) => tool("list_news", { source: "hackernews", since: `day-${n}` }, articles(n), { parentObservationId: worker.id })),
      ...[0, 1, 2].map((n) => tool("search_news", { query: `q${n}` }, articles(10 + n), { parentObservationId: worker.id })),
      obs({ type: "GENERATION", parentObservationId: worker.id, input: [{ role: "tool", content: "INTERMEDIATE-HISTORY-SENTINEL" }] }),
      tool("send_telegram_message", { text: "Digest: 5 stories…", chatId: "42" }, JSON.stringify({ delivered: true, messageId: 77, messageIds: [77], parts: 1 })),
    ];

    const packet = project(trace("telegram:1", { input: "дай дайджест новостей" }), observations);
    const json = JSON.stringify(packet);

    expect(json.length).toBeLessThanOrEqual(DEFAULT_PROJECTION_BUDGET.totalChars);
    expect(json).not.toContain("ARTICLE-BODY-SENTINEL");
    expect(json).not.toContain("INTERMEDIATE-HISTORY-SENTINEL");
    const [digest, listNews, searchNews, send] = packet.actions;
    expect(digest).toMatchObject({ ref: 1, parent: null, tool: "invoke_sub_agent", kind: "agent", outcome: "ok" });
    // Repeated reads inside the worker collapse into one action each, under it.
    expect(listNews).toMatchObject({ tool: "list_news", parent: 1, calls: 5 });
    expect(searchNews).toMatchObject({ tool: "search_news", parent: 1, calls: 3 });
    expect(send).toMatchObject({ tool: "send_telegram_message", parent: null, outcome: "ok", result: { delivered: true, messageId: 77 } });
    expect(packet.actions).toHaveLength(4);
  });

  it("stays within the total even when every field is huge, dropping reads before deliveries", () => {
    const huge = "x".repeat(50_000);
    const observations = [
      root(),
      ...Array.from({ length: 120 }, (_, i) => tool(i % 2 ? "read_doc" : "recall", { query: huge }, huge)),
      tool("send_telegram_message", { text: huge }, JSON.stringify({ delivered: true, messageId: 5 })),
    ];
    const packet = project(trace("telegram:2", { input: huge, output: huge }), observations);
    expect(JSON.stringify(packet).length).toBeLessThanOrEqual(DEFAULT_PROJECTION_BUDGET.totalChars);
    expect(packet.actions.some((a) => a.tool === "send_telegram_message")).toBe(true);
    expect(packet.omitted?.actions_over_budget).toBeGreaterThan(0);
  });

  it("reads a scheduler workflow from tool metadata, not from step names or the execution marker", () => {
    const observations = [
      root(),
      obs({ type: "CHAIN", id: "runner", name: "runner" }),
      tool("step[0]:tool", { tool: "search_news", args: { query: "rust" } }, { count: 2, items: [{ title: "A" }, { title: "B" }] }, {
        parentObservationId: "runner",
        metadata: { tool: "search_news" },
      }),
      tool("step[1]:tool", { tool: "send_telegram_message", args: { text: "Утренний дайджест" } }, { delivered: true, messageId: 9 }, {
        parentObservationId: "runner",
        metadata: { tool: "send_telegram_message" },
      }),
      tool("step[2]:tool", { tool: "set_memory", args: { key: "news_digest.last_read_at" } }, { reason: "tool_error" }, {
        parentObservationId: "runner",
        metadata: { tool: "set_memory" },
        level: "ERROR",
        statusMessage: "tool set_memory failed: disk full",
      }),
    ];
    const packet = project(
      trace("scheduler:3", {
        tags: ["scheduler", "planner-mode"],
        input: "Run the morning digest",
        output: { error: "Workflow execution failed at step 2 (tool_error): tool set_memory failed: disk full" },
        metadata: { activity: "run", error: true },
      }),
      observations,
    );

    expect(packet.actions.map((a) => [a.tool, a.outcome])).toEqual([
      ["search_news", "ok"],
      ["send_telegram_message", "ok"],
      ["set_memory", "error"],
    ]);
    expect(packet.actions[1]?.args).toEqual({ text: "Утренний дайджест" });
    // Delivered, then a later failure: the run failed, the delivery stands.
    expect(packet.status).toBe("error");
    expect(packet.error).toContain("set_memory");
    expect(packet.final).toBeUndefined();
  });

  it("keeps a partial send as a failed send with its delivery evidence", () => {
    const error = "[tool error] send_telegram_message: delivered 2 of 3 parts (message ids 11, 12) before Telegram refused part 3";
    const packet = project(trace("telegram:4"), [
      root(),
      tool("send_telegram_message", { text: "long answer" }, error, { level: "ERROR", statusMessage: error }),
    ]);
    expect(packet.actions[0]).toMatchObject({ outcome: "error", error: expect.stringContaining("2 of 3 parts") });
    expect(packet.actions[0]?.result).toBeUndefined();
  });

  it("gives a follow-up the conversation the primary loop was given, not the raw history fetch", () => {
    const signal = 'Telegram message in chat 42.\nText: "да"';
    const given = [
      "## Recent Telegram history",
      JSON.stringify([{ role: "assistant", text: "Поставить напоминание на 9:00?" }]),
      "",
      "## Current Telegram signal",
      signal,
    ].join("\n");
    const packet = project(trace("telegram:5", { input: signal, output: "Готово, напомню в 9:00" }), [
      root(),
      tool("get_telegram_chat_history", { chatId: "42" }, JSON.stringify({ messages: [{ text: "OTHER-TOPIC-SENTINEL" }] }), {
        metadata: { automatic: true },
      }),
      obs({ type: "CHAIN", name: "agent_loop", input: given, output: "Готово, напомню в 9:00", metadata: { activity: "primary_loop" } }),
    ]);

    expect(packet.context).toContain("Поставить напоминание на 9:00?");
    expect(packet.context).not.toContain("Text: \"да\"");
    expect(JSON.stringify(packet)).not.toContain("OTHER-TOPIC-SENTINEL");
    expect(packet.omitted).toMatchObject({ automatic_context_fetch: 1 });
    expect(packet.final).toBe("Готово, напомню в 9:00");
    expect(packet.input).toBe(signal);
  });

  it("does not depend on observation names", () => {
    const named = (name: string) => [root(), tool("send_telegram_message", { text: "hi" }, "{}", { name })];
    const a = project(trace("telegram:6"), named("send_telegram_message"));
    const b = project(trace("telegram:6"), named("renamed-span"));
    expect(b.actions).toEqual(a.actions);
  });
});

// ─── worker ─────────────────────────────────────────────────────────

interface Harness {
  db: AgentDatabase;
  write(id: string, fields?: Partial<StoredTraceInput>): void;
  clock: { now: Date };
  generated: ActivityPacket[];
  appended: Array<Record<string, unknown>>;
  notes: NoteGenerator & { next: Array<() => Promise<string>> };
  journalFails: { count: number };
  worker: ReturnType<typeof createActivityJournalWorker>;
  progress: ReturnType<typeof createActivityProgressStore>;
}

function harness(): Harness {
  const db = createAgentDb(path.join(dir, `agent-${Math.random().toString(36).slice(2)}.db`));
  const store = createTraceStore(db);
  const clock = { now: new Date("2026-10-13T10:00:00Z") };
  const progress = createActivityProgressStore(db, createMemoryStore(db), () => new Date(Date.now() - 60_000));
  const generated: ActivityPacket[] = [];
  const appended: Array<Record<string, unknown>> = [];
  const journalFails = { count: 0 };
  const notes: Harness["notes"] = {
    next: [],
    async generate(packet) {
      generated.push(packet);
      const scripted = notes.next.shift();
      return scripted ? scripted() : `Asked: ${packet.input}\nDone.`;
    },
  };
  const worker = createActivityJournalWorker(
    {
      traces: store,
      progress,
      notes,
      journal: {
        async callTool(name, args) {
          expect(name).toBe("journal_append");
          if (journalFails.count > 0) {
            journalFails.count -= 1;
            return "[tool error] journal_append: connection reset";
          }
          appended.push(args);
          return JSON.stringify({ ok: true, entry: { id: appended.length } });
        },
      },
      log: () => {},
      now: () => clock.now,
    },
    { ...DEFAULT_ACTIVITY_JOURNAL_OPTS, maxGenerationAttempts: 2 },
  );
  const write = (id: string, fields: Partial<StoredTraceInput> = {}) =>
    store.writeTrace({
      id,
      name: "signal:telegram",
      source: "telegram",
      skill: null,
      sessionId: id,
      tags: ["telegram", "agent-loop"],
      input: `input of ${id}`,
      output: "ok",
      metadata: { activity: "run" },
      observations: [root()],
      startedAt: "2026-10-13T10:00:00.000Z",
      ...fields,
    });
  return { db, write, clock, generated, appended, notes, journalFails, worker, progress };
}

const due = (h: Harness) => h.progress.listDue(100, h.clock.now).map((d) => d.traceId);
const later = (h: Harness, ms: number) => {
  h.clock.now = new Date(h.clock.now.getTime() + ms);
};

describe("worker", () => {
  it("journals only production runs recorded after the cutoff", () => {
    const h = harness();
    h.progress.cutoff();
    h.write("telegram:old");
    h.db.run(sql`UPDATE traces SET created_at = '2020-01-01 00:00:00' WHERE id = 'telegram:old'`);
    h.write("gaia:run:1", { metadata: { gaia_task_id: "x" }, tags: ["gaia", "bench"] });
    h.write("telegram:1");
    h.write("scheduler:2");
    expect(due(h)).toEqual(["telegram:1", "scheduler:2"]);
  });

  it("makes no LLM call on an empty poll", async () => {
    const h = harness();
    expect(await h.worker.tick()).toBe(0);
    expect(h.generated).toHaveLength(0);
  });

  it("appends one note per run with code-supplied provenance, then skips it", async () => {
    const h = harness();
    h.write("telegram:1");
    expect(await h.worker.tick()).toBe(1);
    expect(h.appended).toEqual([
      {
        stream: ACTIVITY_STREAM,
        capacity: 200,
        body: "2026-10-13 10:00 UTC · telegram\nAsked: input of telegram:1\nDone.",
        metadata: expect.objectContaining({ trace_id: "telegram:1", source: "telegram", started_at: "2026-10-13T10:00:00.000Z" }),
      },
    ]);
    expect(h.progress.get("telegram:1")).toMatchObject({ status: "done", entryId: 1, draft: null });
    expect(await h.worker.tick()).toBe(0);
    expect(h.generated).toHaveLength(1);
  });

  it("never processes one trace twice at once", async () => {
    const h = harness();
    h.write("telegram:1");
    let release: (note: string) => void = () => {};
    h.notes.next.push(() => new Promise((resolve) => (release = resolve)));
    const [item] = h.progress.listDue(10, h.clock.now);
    if (!item) throw new Error("expected a due trace");
    const first = h.worker.processTrace(item);
    // A poll while the first is still generating takes nothing.
    expect(await h.worker.tick()).toBe(0);
    expect(await h.worker.processTrace(item)).toBe("skipped");
    release("Note.");
    expect(await first).toBe("done");
    expect(h.generated).toHaveLength(1);
    expect(h.appended).toHaveLength(1);
  });

  it("retries a failed append with the note it already generated", async () => {
    const h = harness();
    h.write("telegram:1");
    h.journalFails.count = 1;
    await h.worker.tick();
    expect(h.progress.get("telegram:1")).toMatchObject({ status: "pending", draft: { body: expect.stringContaining("Done.") } });
    expect(due(h)).toEqual([]); // backing off
    later(h, DEFAULT_ACTIVITY_JOURNAL_OPTS.retryBaseMs + 1);
    await h.worker.tick();
    expect(h.generated).toHaveLength(1);
    expect(h.appended).toHaveLength(1);
    expect(h.progress.get("telegram:1")).toMatchObject({ status: "done", draft: null });
  });

  it("backs off a failing trace while later ones go through, and gives up on it", async () => {
    const h = harness();
    h.write("telegram:1");
    h.write("telegram:2");
    h.notes.next.push(async () => "line\n".repeat(30)); // invalid: too many lines
    await h.worker.tick();
    expect(h.progress.get("telegram:1")).toMatchObject({ status: "pending", attempts: 1, error: expect.stringContaining("invalid note") });
    expect(h.progress.get("telegram:2")).toMatchObject({ status: "done" });

    later(h, DEFAULT_ACTIVITY_JOURNAL_OPTS.retryBaseMs + 1);
    h.notes.next.push(async () => {
      throw new Error("model down");
    });
    await h.worker.tick();
    expect(h.progress.get("telegram:1")).toMatchObject({ status: "failed", attempts: 2 });
    later(h, DEFAULT_ACTIVITY_JOURNAL_OPTS.retryMaxMs);
    expect(due(h)).toEqual([]);
    expect(h.appended).toEqual([expect.objectContaining({ metadata: expect.objectContaining({ trace_id: "telegram:2" }) })]);
  });

  it("finds a long run that started first but finished last", () => {
    const h = harness();
    h.write("telegram:fast", { startedAt: "2026-10-13T10:05:00.000Z" });
    h.write("telegram:slow", { startedAt: "2026-10-13T09:00:00.000Z" });
    expect(due(h)).toEqual(["telegram:fast", "telegram:slow"]);
  });
});
