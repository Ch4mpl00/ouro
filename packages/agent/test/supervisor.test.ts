// The supervisor end to end: the real poll loop, supervisor module, engine,
// workflow runner (compile + execute), skills from skills.default and the
// sqlite trace mirror — driven over the real MCP client protocol against an
// in-process MCP server that plays the queue and Telegram, with a scripted
// LLM at the provider boundary.
//
// Each generated signal carries a plan for what "the model" does with it
// (send messages, some of which the tool rejects; answer; crash; for the
// scheduler: compile a workflow, or fail to). The rules then say, from the
// plan alone, what must have reached the chat, which paths ran and what was
// traced — per signal, in queue order.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import fc from "fast-check";
import type { ChatCompletionMessage, ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { z } from "zod";
import { createAgentDb } from "../src/db/client";
import { createMemoryStore } from "../src/db/memory";
import { createTraceStore, type TraceStore } from "../src/db/trace-store";
import { createEngine } from "../src/engine";
import { connectMcp } from "../src/mcp-client";
import type { ModelPreset, PresetName } from "../src/models";
import type { ChatProvider, CompletionParams } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { createSkillStore } from "../src/skills";
import { createLocalRecorderTracer } from "../src/tracing/local-recorder";
import { createWorkflowRunner } from "../src/workflow";
import { runSupervisorLoop } from "../src/supervisor/loop";
import { createSupervisorModule } from "../src/supervisor/module";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 25);
const REPO = path.resolve(import.meta.dirname, "../../..");
const CHAT = 42;

const PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "m-base", reasoningEffort: "disabled" },
  smart: { model: "m-smart", reasoningEffort: "medium" },
  smartest: { model: "m-smartest", reasoningEffort: "max" },
  compiler: { model: "m-compiler", reasoningEffort: "medium" },
};

// ── plans ────────────────────────────────────────────────────────────────────

// What the model does on each turn of a loop. A send the tool rejects comes
// back to the model as a tool error and the loop goes on.
type Act = { kind: "send"; ok: boolean } | { kind: "answer" } | { kind: "crash" };

// `disguised`: a non-Telegram signal whose content starts like a Telegram
// one — it must still be treated as what its source says.
type Plan =
  | { source: "telegram"; loop: Act[] }
  | { source: "nashdom-bill" | "dreaming"; loop: Act[]; disguised: boolean }
  | { source: "scheduler"; compile: { kind: "valid"; sends: boolean[] } | { kind: "invalid"; loop: Act[] } };

// A loop always ends: in an answer or a crash.
const loopActs = fc
  .tuple(fc.array(fc.record({ kind: fc.constant("send" as const), ok: fc.boolean() }), { maxLength: 3 }), fc.constantFrom("answer" as const, "crash" as const))
  .map(([sends, end]): Act[] => [...sends, { kind: end }]);

const plan: fc.Arbitrary<Plan> = fc.oneof(
  fc.record({ source: fc.constant("telegram" as const), loop: loopActs }),
  fc.record({ source: fc.constantFrom("nashdom-bill" as const, "dreaming" as const), loop: loopActs, disguised: fc.boolean() }),
  fc.record({
    source: fc.constant("scheduler" as const),
    compile: fc.oneof(
      fc.record({ kind: fc.constant("valid" as const), sends: fc.array(fc.boolean(), { minLength: 1, maxLength: 3 }) }),
      fc.record({ kind: fc.constant("invalid" as const), loop: loopActs }),
    ),
  }),
);

const tag = (id: number) => `⟦sig ${id}⟧`;
const sendText = (id: number, i: number, ok: boolean) => `${tag(id)} part ${i}${ok ? "" : " ⚠reject"}`;
const recoveryText = (id: number) => `${tag(id)} recovery report`;

// What must reach the chat for a signal, from its plan alone.
function expectedDeliveries(id: number, p: Plan): { texts: string[]; failed: boolean; loop: boolean; compiled: boolean } {
  const runLoop = (acts: Act[]) => {
    const texts: string[] = [];
    for (const [i, act] of acts.entries()) {
      if (act.kind === "send" && act.ok) texts.push(sendText(id, i, true));
      if (act.kind === "crash") return { texts: [...texts, recoveryText(id)], failed: true };
      if (act.kind === "answer") break;
    }
    return { texts, failed: false };
  };
  if (p.source !== "scheduler") return { ...runLoop(p.loop), loop: true, compiled: false };
  if (p.compile.kind === "invalid") return { ...runLoop(p.compile.loop), loop: true, compiled: false };
  const texts: string[] = [];
  for (const [i, ok] of p.compile.sends.entries()) {
    if (!ok) return { texts: [...texts, recoveryText(id)], failed: true, loop: false, compiled: true };
    texts.push(sendText(id, i, true));
  }
  return { texts, failed: false, loop: false, compiled: true };
}

function contentOf(id: number, p: Plan): string {
  const telegram = `Telegram message in chat ${CHAT}.\nText: ${JSON.stringify(`${tag(id)} hello`)}`;
  if (p.source === "telegram" || (p.source !== "scheduler" && p.disguised)) return telegram;
  return `${tag(id)} ${p.source} event`;
}

// ── the world: queue, chat, and a log of everything that happened ────────────

type Event =
  | { kind: "pop"; id: number | null }
  | { kind: "delivered"; text: string }
  | { kind: "rejected"; text: string }
  | { kind: "history" }
  | { kind: "llm"; role: "compile" | "primary" | "recovery"; signal: number; firstUser: string };

interface World {
  queue: { id: number; source: string; content: string }[];
  events: Event[];
  drained: boolean;
}

function mcpServer(world: World): McpServer {
  const server = new McpServer({ name: "fake-mcp", version: "0.0.0" });
  const json = (value: unknown) => ({ content: [{ type: "text" as const, text: JSON.stringify(value) }] });
  server.registerTool("get_next_signal", { description: "Pop the oldest pending signal.", inputSchema: {} }, async () => {
    const next = world.queue.shift() ?? null;
    world.events.push({ kind: "pop", id: next?.id ?? null });
    if (next === null) world.drained = true;
    return json({
      signal: next && { ...next, envContext: `Default Telegram chat id: ${CHAT}.`, created_at: "2026-10-12 10:00:00" },
      pendingAfter: world.queue.length,
    });
  });
  server.registerTool(
    "send_telegram_message",
    { description: "Send a Telegram message.", inputSchema: { text: z.string(), chatId: z.union([z.string(), z.number()]).optional() } },
    async ({ text }) => {
      if (text.includes("⚠reject")) {
        world.events.push({ kind: "rejected", text });
        return { content: [{ type: "text" as const, text: "Telegram sendMessage failed (400): Bad Request" }], isError: true };
      }
      world.events.push({ kind: "delivered", text });
      return json({ delivered: true, messageId: world.events.length });
    },
  );
  server.registerTool(
    "get_telegram_chat_history",
    { description: "Chat history.", inputSchema: { chatId: z.union([z.string(), z.number()]), limit: z.number().optional(), threadId: z.number().optional() } },
    async () => {
      world.events.push({ kind: "history" });
      return json({ messages: [] });
    },
  );
  server.registerTool("get_timezone", { description: "Timezone.", inputSchema: {} }, async () => json({ timezone: "UTC", local_now: "2026-10-12 10:00" }));
  return server;
}

// ── the scripted model ───────────────────────────────────────────────────────

const text = (m: ChatCompletionMessageParam): string =>
  typeof m.content === "string" ? m.content : Array.isArray(m.content) ? m.content.map((p) => ("text" in p ? p.text : "")).join("") : "";

function scriptedProvider(world: World, plans: ReadonlyMap<number, Plan>): ChatProvider {
  const reply = (message: ChatCompletionMessage) => ({ message, finishReason: "stop", usage: { input: 1, output: 1, total: 2, cached: 0 } });
  const answer = (content: string) => reply({ role: "assistant", content, refusal: null });
  const send = (body: string, turn: number) =>
    reply({
      role: "assistant",
      content: null,
      refusal: null,
      tool_calls: [{ id: `call_${turn}`, type: "function", function: { name: "send_telegram_message", arguments: JSON.stringify({ text: body, chatId: CHAT }) } }],
    });

  return {
    kind: "openai",
    async complete(params: CompletionParams) {
      const all = params.messages.map(text).join("\n");
      const m = /⟦sig (\d+)⟧/.exec(all);
      if (!m) throw new Error(`scripted model: no signal tag in the prompt: ${all.slice(0, 300)}`);
      const id = Number(m[1]);
      const p = plans.get(id);
      if (!p) throw new Error(`scripted model: no plan for signal ${id}`);
      const turn = params.messages.filter((x) => x.role === "assistant").length;
      const recovery = params.messages.some((x) => x.role === "user" && text(x).startsWith("Original signal ("));
      const role = params.model === PRESETS.compiler.model ? "compile" : recovery ? "recovery" : "primary";
      const firstUser = params.messages.find((x) => x.role === "user");
      world.events.push({ kind: "llm", role, signal: id, firstUser: firstUser ? text(firstUser) : "" });

      if (role === "compile") {
        if (p.source !== "scheduler" || p.compile.kind === "invalid") return answer("{ this is not a workflow");
        const steps = p.compile.sends.map((ok, i) => ({ kind: "tool", tool: "send_telegram_message", args: { text: sendText(id, i, ok), chatId: CHAT } }));
        return answer(JSON.stringify({ version: 1, steps: [...steps, { kind: "terminal" }] }));
      }
      if (role === "recovery") return turn === 0 ? send(recoveryText(id), turn) : answer("reported");
      const acts = p.source === "scheduler" ? (p.compile.kind === "invalid" ? p.compile.loop : []) : p.loop;
      const act = acts[turn];
      if (!act) throw new Error(`scripted model: signal ${id} has no turn ${turn}`);
      if (act.kind === "crash") throw new Error("400 invalid request: scripted crash");
      if (act.kind === "answer") return answer("done");
      return send(sendText(id, turn, act.ok), turn);
    },
  };
}

// ── suite ────────────────────────────────────────────────────────────────────

describe("supervisor end to end over MCP", () => {
  let dir: string;

  beforeAll(() => {
    dir = mkdtempSync(path.join(tmpdir(), "supervisor-e2e-"));
  });
  afterAll(() => rmSync(dir, { recursive: true, force: true }));

  async function run(plans: { id: number; plan: Plan }[]) {
    const world: World = {
      queue: plans.map(({ id, plan: p }) => ({
        id,
        source: p.source,
        content: contentOf(id, p),
      })),
      events: [],
      drained: false,
    };
    const byId = new Map(plans.map(({ id, plan: p }) => [id, p]));
    const db = createAgentDb(path.join(dir, `agent-${Math.random().toString(36).slice(2)}.db`));
    const traces = createTraceStore(db);
    const mcp = await connectMcp({
      createTransport: () => {
        const [client, server] = InMemoryTransport.createLinkedPair();
        void mcpServer(world).connect(server);
        return client;
      },
    });
    const provider = scriptedProvider(world, byId);
    const memory = createMemoryStore(db);
    const skillStore = createSkillStore({ liveDir: path.join(dir, "no-live-skills"), defaultsDir: path.join(REPO, "skills.default") });
    const engine = createEngine({
      providers: { openai: provider, gemini: provider, deepseek: provider },
      mcp,
      presets: PRESETS,
      skills: ["orchestrator", "routing"],
      skillStore,
      memory,
      tracer: createLocalRecorderTracer(traces),
      codex: { run: async () => { throw new Error("no code_agent in this test"); } },
    });
    const knownSkills = (await skillStore.listSkills()).map((s) => s.name).filter((n) => !["planner", "orchestrator", "routing", "recovery"].includes(n));
    const workflow = createWorkflowRunner({
      engine,
      mcpTools: mcp.tools,
      knownSkills,
      readSkill: async (name) => (await skillStore.readSkill(name))?.body ?? null,
      setMemory: (key, value) => memory.set(key, value),
    });
    const supervisor = createSupervisorModule({ engine, env: { mcp, memory, userEmail: null }, workflow });
    await runSupervisorLoop({ mcp, supervisor, stopping: () => world.drained, sleep: async () => {}, pollIntervalMs: 0 });

    // Read the mirror back first: the leak probe below opens loops under the
    // same ids, and a standalone loop records a trace of its own.
    const traced = new Map(plans.map(({ id, plan: p }) => [id, traces.getTrace(`${p.source}:${id}`)]));
    // No agent loop is left open: each signal's loop ids are free again.
    const leaked: string[] = [];
    for (const { id, plan: p } of plans) {
      for (const loopId of [`${p.source}:${id}`, `${p.source}:${id}__recovery`]) {
        const context = createSessionContext({ id: loopId, env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } });
        try {
          await engine.startAgentLoop({ id: loopId, sessionContext: context });
          await engine.endAgentLoop(loopId);
        } catch (err) {
          leaked.push(`${loopId}: ${err instanceof Error ? err.message : String(err)}`);
        }
      }
    }
    await supervisor.shutdown();
    db.$client.close();
    return { world, traced, leaked };
  }

  function checkSignal(id: number, p: Plan, segment: Event[], stored: ReturnType<TraceStore["getTrace"]> | undefined): void {
    const want = expectedDeliveries(id, p);
    const where = `signal ${id} ${JSON.stringify(p)}`;
    const delivered = segment.flatMap((e) => (e.kind === "delivered" ? [e.text] : []));
    expect(delivered, `${where}: exactly the planned messages reach the chat, in order`).toEqual(want.texts);
    expect(delivered.every((t) => t.startsWith(tag(id))), `${where}: nothing from another signal`).toBe(true);
    const llm = segment.flatMap((e) => (e.kind === "llm" ? [e] : []));
    expect(llm.every((e) => e.signal === id), `${where}: the model only ever sees this signal`).toBe(true);
    expect(llm.some((e) => e.role === "compile"), `${where}: compiled iff scheduler`).toBe(p.source === "scheduler");
    expect(llm.some((e) => e.role === "primary"), `${where}: the primary loop runs iff the plan needs it`).toBe(want.loop);
    expect(llm.some((e) => e.role === "recovery"), `${where}: recovery runs iff the signal failed`).toBe(want.failed);
    const history = segment.filter((e) => e.kind === "history").length;
    expect(history, `${where}: Telegram history is preloaded once, only for telegram signals`).toBe(p.source === "telegram" ? 1 : 0);
    if (history > 0) {
      const at = segment.findIndex((e) => e.kind === "history");
      const firstPrimary = segment.findIndex((e) => e.kind === "llm" && e.role === "primary");
      expect(at, `${where}: history before the first primary turn`).toBeLessThan(firstPrimary);
    }
    const firstPrimary = llm.find((e) => e.role === "primary");
    if (p.source !== "telegram" && firstPrimary) {
      expect(firstPrimary.firstUser, `${where}: a non-Telegram signal reaches the model verbatim`).toBe(contentOf(id, p));
    }
    if (want.compiled && want.failed) {
      expect(llm.some((e) => e.role === "primary"), `${where}: an execute failure is never retried agentically`).toBe(false);
    }
    expect(stored, `${where}: one trace per signal`).toBeTruthy();
    expect(stored?.trace.name, `${where}: trace name`).toBe(`signal:${p.source}`);
  }

  it("handles every signal once, in order, along the path its plan calls for", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.uniqueArray(fc.integer({ min: 1, max: 10_000 }), { minLength: 1, maxLength: 6 }), fc.array(plan, { minLength: 6, maxLength: 6 }), async (ids, ps) => {
        const plans = ids.map((id, i) => ({ id, plan: ps[i]! }));
        const { world, traced, leaked } = await run(plans);
        const pops = world.events.flatMap((e) => (e.kind === "pop" ? [e.id] : []));
        expect(pops, "each signal taken once, in queue order, then the queue is found empty").toEqual([...plans.map((p) => p.id), null]);
        // Sequential: everything between two pops belongs to the signal taken first.
        const starts = world.events.flatMap((e, i) => (e.kind === "pop" ? [i] : []));
        for (const [k, { id, plan: p }] of plans.entries()) {
          checkSignal(id, p, world.events.slice(starts[k]! + 1, starts[k + 1]), traced.get(id));
          const want = expectedDeliveries(id, p);
          witnessed.add(`${p.source === "scheduler" ? (want.compiled ? "compiled" : "compile failed") : "loop"} → ${want.failed ? "recovery" : "ok"}`);
          if (world.events.slice(starts[k]! + 1, starts[k + 1]).some((e) => e.kind === "rejected")) witnessed.add("a rejected send");
        }
        expect(leaked, "no agent loop left open").toEqual([]);
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(
      ["a rejected send", "compile failed → ok", "compile failed → recovery", "compiled → ok", "compiled → recovery", "loop → ok", "loop → recovery"].sort(),
    );
  }, 600_000);
});
