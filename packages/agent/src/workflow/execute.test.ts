import { beforeEach, describe, expect, it } from "vitest";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { ChatProvider } from "../providers";
import type { AgentLoopOpts } from "../agent-loop";
import type { Generation, Span, Trace, TraceContext } from "../tracing";
import { createWorkflowSchema, type Workflow } from "./dsl";
import {
  createExecutor,
  type EngineSurface,
  type AgentLoopHandle,
  __testing,
} from "./execute";
import { createStore } from "./variables";

// ─── shared mocks ────────────────────────────────────────────────────

const PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
  compiler: { model: "gemini-3-flash-preview", reasoningEffort: "low" },
};

function recordingSpan(): Span & { events: unknown[] } {
  const events: unknown[] = [];
  const span: Span & { events: unknown[] } = {
    events,
    update(data) {
      events.push({ kind: "update", data });
    },
    end(opts) {
      events.push({ kind: "end", opts });
    },
    generation(opts) {
      events.push({ kind: "generation:start", opts });
      const gen: Generation = {
        end(eo) {
          events.push({ kind: "generation:end", opts: eo });
        },
      };
      return gen;
    },
    span(opts) {
      events.push({ kind: "span:start", opts });
      return recordingSpan();
    },
    event(opts) {
      events.push({ kind: "event", opts });
    },
  };
  return span;
}

function recordingTrace(): Trace {
  const root = recordingSpan();
  return {
    update: root.update,
    generation: root.generation,
    span: root.span,
    event: root.event,
    end: () => {},
  };
}

// Like recordingSpan, but every span created under it (recursively) is
// pushed into a shared `all` list so a test can inspect the output a
// nested step span was closed with. recordingTrace/recordingSpan discard
// child spans, which is fine for store/tool-call assertions but hides the
// per-step `end({ output })` we want to verify.
function collectingTrace(): { trace: Trace; all: Array<{ name: string; events: unknown[] }> } {
  const all: Array<{ name: string; events: unknown[] }> = [];
  function make(name: string): Span & { events: unknown[] } {
    const events: unknown[] = [];
    const self: Span & { events: unknown[] } = {
      events,
      update(data) {
        events.push({ kind: "update", data });
      },
      end(opts) {
        events.push({ kind: "end", opts });
      },
      generation(opts) {
        events.push({ kind: "generation:start", opts });
        return {
          end(eo) {
            events.push({ kind: "generation:end", opts: eo });
          },
        };
      },
      span(opts) {
        return make(opts.name);
      },
      event(opts) {
        events.push({ kind: "event", opts });
      },
    };
    all.push({ name, events });
    return self;
  }
  const root = make("__root__");
  const trace: Trace = {
    update: root.update,
    generation: root.generation,
    span: root.span,
    event: root.event,
    end: () => {},
  };
  return { trace, all };
}

// Pull the `output` a span was closed with, by name prefix (step spans are
// named `step[<i>]:<kind>`).
function endOutput(
  all: Array<{ name: string; events: unknown[] }>,
  namePrefix: string,
): unknown {
  const span = all.find((s) => s.name.startsWith(namePrefix));
  if (!span) throw new Error(`no span named ${namePrefix}; have ${all.map((s) => s.name).join(", ")}`);
  const end = span.events.find(
    (e): e is { kind: "end"; opts?: { output?: unknown } } =>
      typeof e === "object" && e !== null && (e as { kind?: string }).kind === "end",
  );
  return end?.opts?.output;
}

interface MockCall {
  tool: string;
  args: Record<string, unknown>;
}

interface MockEngineOpts {
  toolResponses?: Record<string, string | ((args: Record<string, unknown>) => string)>;
  llmResponses?: string[];
  agentLoopResults?: string[];
  startAgentLoopThrows?: Error;
}

function makeMockEngine(opts: MockEngineOpts = {}): EngineSurface & {
  toolCalls: MockCall[];
  llmCalls: unknown[];
  agentLoopStarts: AgentLoopOpts[];
  endedAgentLoopIds: string[];
} {
  const toolCalls: MockCall[] = [];
  const llmCalls: unknown[] = [];
  const agentLoopStarts: AgentLoopOpts[] = [];
  const endedAgentLoopIds: string[] = [];
  const llmQueue = [...(opts.llmResponses ?? [])];
  const agentLoopQueue = [...(opts.agentLoopResults ?? [])];

  // ChatProvider mock: captures the normalized completion params (which
  // carry `.messages`, the assertion target) and returns a canned answer.
  const provider: ChatProvider = {
    kind: "openai",
    complete: async (params) => {
      llmCalls.push(params);
      const text = llmQueue.shift() ?? "";
      return {
        message: { role: "assistant", content: text, refusal: null },
        finishReason: "stop",
        usage: { input: 100, output: 50, total: 150 },
      };
    },
  };

  return {
    presets: PRESETS,
    resolveProvider(_model: string) {
      return provider;
    },
    mcp: {
      callTool: async (name, args) => {
        toolCalls.push({ tool: name, args });
        const r = opts.toolResponses?.[name];
        if (r === undefined) return `[tool error] unknown tool ${name}`;
        return typeof r === "function" ? r(args) : r;
      },
    },
    startAgentLoop: async (loopOpts: AgentLoopOpts): Promise<AgentLoopHandle> => {
      if (opts.startAgentLoopThrows) throw opts.startAgentLoopThrows;
      agentLoopStarts.push(loopOpts);
      const result = agentLoopQueue.shift() ?? "";
      const messages: ChatCompletionMessageParam[] = [];
      return {
        messages,
        run: async () => result,
      };
    },
    endAgentLoop: (id: string) => {
      endedAgentLoopIds.push(id);
    },
    toolCalls,
    llmCalls,
    agentLoopStarts,
    endedAgentLoopIds,
  };
}

function nullReadSkill(): (name: string) => Promise<string | null> {
  return async () => null;
}

function fixedReadSkill(map: Record<string, string>): (
  name: string,
) => Promise<string | null> {
  return async (name: string) => map[name] ?? null;
}

const baseCtx = () => ({
  store: createStore({ env: { chatId: 42 } }),
  parentTrace: recordingTrace() as TraceContext,
  signalLabel: "test:1",
});

// ─── tests ───────────────────────────────────────────────────────────

describe("executor.execute — tool step", () => {
  it("calls mcp.callTool with substituted args and binds parsed JSON result", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        list_news: JSON.stringify({ count: 2, items: [{ id: 1 }, { id: 2 }] }),
      },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });

    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "list_news",
          args: { chatId: "${env.chatId}", limit: 5 },
          bind: "posts",
        },
        { kind: "terminal" },
      ],
    };

    const ctx = baseCtx();
    const r = await executor.execute(plan, ctx);

    expect(r.ok).toBe(true);
    expect(engine.toolCalls).toEqual([
      { tool: "list_news", args: { chatId: 42, limit: 5 } },
    ]);
    expect(ctx.store.get("posts")).toEqual({
      count: 2,
      items: [{ id: 1 }, { id: 2 }],
    });
  });

  it("records the real tool output on the step span (not a generic {ok:true})", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        search_news: JSON.stringify({ hits: [{ id: 7, title: "x" }] }),
      },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const { trace, all } = collectingTrace();

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "search_news", args: { queries: ["ai"] }, bind: "res" },
          { kind: "terminal" },
        ],
      },
      { store: createStore({ env: {} }), parentTrace: trace, signalLabel: "test:1" },
    );

    expect(r.ok).toBe(true);
    // The step span must carry the parsed tool result, not `{ ok: true }`.
    expect(endOutput(all, "step[0]:tool")).toEqual({ hits: [{ id: 7, title: "x" }] });
  });

  it("works without bind (fire-and-forget)", async () => {
    const engine = makeMockEngine({
      toolResponses: { send_telegram_message: JSON.stringify({ delivered: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "send_telegram_message", args: { chatId: 1, text: "hi" } },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(true);
  });

  it("preserves raw string when MCP returns non-JSON", async () => {
    const engine = makeMockEngine({
      toolResponses: { ping: "pong" },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "ping", args: {}, bind: "out" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(ctx.store.get("out")).toBe("pong");
  });

  it("propagates [tool error] as failure with tool_error reason", async () => {
    const engine = makeMockEngine({
      toolResponses: { broken_tool: "[tool error] something broke" },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "broken_tool", args: {}, bind: "x" },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("tool_error");
      expect(r.stepIndex).toBe(0);
      expect(r.step.kind).toBe("tool");
    }
  });

  it("missing binding in args surfaces as missing_binding reason", async () => {
    const engine = makeMockEngine();
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "tool",
            tool: "ping",
            args: { ref: "${posts}" },
            bind: "x",
          },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("missing_binding");
    // Tool was never called — short-circuit on substitution failure.
    expect(engine.toolCalls).toEqual([]);
  });
});

describe("executor.execute — set_memory step (agent-side builtin)", () => {
  it("dispatches to the injected setMemory writer, not MCP, and binds an ack", async () => {
    const engine = makeMockEngine();
    const memWrites: Array<[string, string]> = [];
    const executor = createExecutor({
      engine,
      readSkill: nullReadSkill(),
      setMemory: (k, v) => memWrites.push([k, v]),
    });
    const ctx = baseCtx();
    ctx.store.set("now", "2026-06-03T05:05:00Z");

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "tool",
            tool: "set_memory",
            args: { key: "news_digest.last_read_at", value: "${now}" },
            bind: "wm",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );

    expect(r.ok).toBe(true);
    expect(memWrites).toEqual([["news_digest.last_read_at", "2026-06-03T05:05:00Z"]]);
    // Never forwarded to MCP — set_memory has no MCP counterpart.
    expect(engine.toolCalls).toEqual([]);
    expect(ctx.store.get("wm")).toEqual({ ok: true, key: "news_digest.last_read_at" });
  });

  it("rejects a non-string value as tool_error without writing", async () => {
    const engine = makeMockEngine();
    const memWrites: Array<[string, string]> = [];
    const executor = createExecutor({
      engine,
      readSkill: nullReadSkill(),
      setMemory: (k, v) => memWrites.push([k, v]),
    });

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "set_memory", args: { key: "k", value: 123 }, bind: "x" },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );

    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("tool_error");
    expect(memWrites).toEqual([]);
  });
});

describe("executor.execute — llm_compose step", () => {
  it("loads skill as system, builds user from prompt + XML input, calls LLM without tools", async () => {
    const engine = makeMockEngine({ llmResponses: ["composed digest"] });
    const executor = createExecutor({
      engine,
      readSkill: fixedReadSkill({ "news-digest": "RULES go here" }),
      setMemory: () => {},
    });

    const ctx = baseCtx();
    ctx.store.set("posts", [{ id: 1 }]);
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "smartest",
            skill: "news-digest",
            input: { posts: "${posts}" },
            bind: "digest",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("digest")).toBe("composed digest");

    expect(engine.llmCalls.length).toBe(1);
    const body = engine.llmCalls[0] as {
      model: string;
      messages: ChatCompletionMessageParam[];
      tools?: unknown;
    };
    expect(body.model).toBe("gpt-5.4");
    expect(body.tools).toBeUndefined(); // No tools key — model can't emit tool_calls.
    expect(body.messages[0]).toEqual({ role: "system", content: "RULES go here" });
    expect(typeof body.messages[1]!.content).toBe("string");
    expect(body.messages[1]!.content).toContain("<posts>");
    expect(body.messages[1]!.content).toContain('"id": 1');
  });

  it("parses JSON output and binds the object, so later steps can dot into it", async () => {
    const engine = makeMockEngine({
      llmResponses: [JSON.stringify({ cancelId: 10, cron_expr: "0 9 L * *" })],
      toolResponses: { cancel_scheduled_task: JSON.stringify({ cancelled: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            prompt: "Emit the reschedule plan as JSON",
            input: {},
            bind: "target",
          },
          {
            kind: "tool",
            tool: "cancel_scheduled_task",
            args: { id: "${target.cancelId}" },
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    // Bound as a parsed object, not a raw JSON string.
    expect(ctx.store.get("target")).toEqual({ cancelId: 10, cron_expr: "0 9 L * *" });
    // The dot-access ${target.cancelId} resolved to the literal 10.
    const toolCall = engine.toolCalls.find((c) => c.tool === "cancel_scheduled_task");
    expect(toolCall?.args).toEqual({ id: 10 });
  });

  it("resolves deep dot-access into a nested parsed object", async () => {
    const engine = makeMockEngine({
      llmResponses: [JSON.stringify({ schedule: { cron: "0 9 L * *", recurring: true } })],
      toolResponses: { schedule_task: JSON.stringify({ id: 11 }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "plan", input: {}, bind: "plan" },
          {
            kind: "tool",
            tool: "schedule_task",
            args: { cron_expr: "${plan.schedule.cron}", recurring: "${plan.schedule.recurring}" },
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const call = engine.toolCalls.find((c) => c.tool === "schedule_task");
    // Whole-string placeholders preserve type: cron stays a string, recurring stays a boolean.
    expect(call?.args).toEqual({ cron_expr: "0 9 L * *", recurring: true });
  });

  it("parses a JSON array output and binds it as an array (whole-string preserves type)", async () => {
    const engine = makeMockEngine({
      llmResponses: [JSON.stringify([{ id: 1 }, { id: 2 }])],
      toolResponses: { send_batch: JSON.stringify({ ok: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "list", input: {}, bind: "rows" },
          { kind: "tool", tool: "send_batch", args: { rows: "${rows}" } },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("rows")).toEqual([{ id: 1 }, { id: 2 }]);
    const call = engine.toolCalls.find((c) => c.tool === "send_batch");
    expect(Array.isArray(call?.args.rows)).toBe(true);
    expect(call?.args.rows).toEqual([{ id: 1 }, { id: 2 }]);
  });

  it("interpolates a parsed object's field into a mixed string (JSON-stringifies the number)", async () => {
    const engine = makeMockEngine({
      llmResponses: [JSON.stringify({ cancelId: 10 })],
      toolResponses: { send_telegram_message: JSON.stringify({ delivered: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "plan", input: {}, bind: "target" },
          {
            kind: "tool",
            tool: "send_telegram_message",
            args: { chatId: 1, text: "Cancelled task ${target.cancelId}" },
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const call = engine.toolCalls.find((c) => c.tool === "send_telegram_message");
    expect(call?.args.text).toBe("Cancelled task 10");
  });

  it("keeps plain prose as a string (no JSON prefix → not parsed)", async () => {
    const engine = makeMockEngine({ llmResponses: ["Привет! Вот твой дайджест за сегодня."] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "greet", input: {}, bind: "reply" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const reply = ctx.store.get("reply");
    expect(typeof reply).toBe("string");
    expect(reply).toBe("Привет! Вот твой дайджест за сегодня.");
  });

  it("keeps a bare primitive output as a string (only {/[ prefixes are parsed)", async () => {
    const engine = makeMockEngine({ llmResponses: ["42"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "count", input: {}, bind: "n" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    // "42" doesn't start with { or [, so it stays the raw string, not the number 42.
    expect(ctx.store.get("n")).toBe("42");
  });

  it("keeps JSON-looking-but-invalid output as the raw string", async () => {
    const engine = makeMockEngine({ llmResponses: ['{ "cancelId": 10  // oops, not JSON'] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "plan", input: {}, bind: "target" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    // JSON.parse throws → tryParseJson falls back to the raw string.
    expect(ctx.store.get("target")).toBe('{ "cancelId": 10  // oops, not JSON');
  });

  it("fails with missing_binding when dotting into a field the parsed object lacks", async () => {
    const engine = makeMockEngine({
      llmResponses: [JSON.stringify({ cancelId: 10 })],
      toolResponses: { cancel_scheduled_task: JSON.stringify({ cancelled: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "plan", input: {}, bind: "target" },
          {
            kind: "tool",
            tool: "cancel_scheduled_task",
            args: { id: "${target.missingField}" },
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("missing_binding");
      expect(r.stepIndex).toBe(1);
    }
    // The tool never ran — substitution failed before dispatch.
    expect(engine.toolCalls.find((c) => c.tool === "cancel_scheduled_task")).toBeUndefined();
  });

  it("fails with missing_binding when dotting into a string-valued compose result", async () => {
    // Regression guard for the original bug: a prose compose bound as a
    // string, then a step that dots into it — must surface MissingBindingError,
    // not silently expand to "undefined".
    const engine = makeMockEngine({
      llmResponses: ["just some prose, no fields here"],
      toolResponses: { cancel_scheduled_task: JSON.stringify({ cancelled: true }) },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", prompt: "plan", input: {}, bind: "target" },
          {
            kind: "tool",
            tool: "cancel_scheduled_task",
            args: { id: "${target.cancelId}" },
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("missing_binding");
  });

  it("works with prompt-only (no skill)", async () => {
    const engine = makeMockEngine({ llmResponses: ["A: 5"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    ctx.store.set("question", "what is 2+3");
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            prompt: "Answer: ${question}",
            input: {},
            bind: "answer",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("answer")).toBe("A: 5");
    const body = engine.llmCalls[0] as {
      messages: ChatCompletionMessageParam[];
    };
    expect(body.messages.length).toBe(1); // no system, just user
    expect(body.messages[0]!.content).toBe("Answer: what is 2+3");
  });

  it("appends XML blocks after prompt when both are present", async () => {
    const engine = makeMockEngine({ llmResponses: ["ok"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    ctx.store.set("items", ["a", "b"]);
    await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            prompt: "Inspect this:",
            input: { items: "${items}" },
            bind: "out",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    const body = engine.llmCalls[0] as { messages: ChatCompletionMessageParam[] };
    const userText = body.messages[0]!.content as string;
    expect(userText).toMatch(/^Inspect this:/);
    expect(userText).toContain("<items>");
  });

  it("skill_not_found when skill is named but readSkill returns null", async () => {
    const engine = makeMockEngine();
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            skill: "ghost",
            input: {},
            bind: "out",
          },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("skill_not_found");
  });
});

describe("executor.execute — llm_agent step", () => {
  it("spawns child session with toolWhitelist and binds result", async () => {
    const engine = makeMockEngine({ agentLoopResults: ["agent answer"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    ctx.store.set("query", "что в Одессе");
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_agent",
            preset: "smart",
            skill: "news-query",
            prompt: "${query}",
            tools: ["search_news", "list_news"],
            maxIterations: 5,
            bind: "answer",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("answer")).toBe("agent answer");

    expect(engine.agentLoopStarts.length).toBe(1);
    const opts = engine.agentLoopStarts[0]!;
    expect(opts.skills).toEqual(["news-query"]);
    expect(opts.includeEngineSkills).toBe(false);
    expect(opts.preset).toBe("smart");
    expect(opts.maxIterations).toBe(5);
    expect(opts.parentId).toBe("test:1");
    expect(opts.toolWhitelist).toEqual(new Set(["search_news", "list_news"]));
  });

  it("ends the spawned session on success", async () => {
    const engine = makeMockEngine({ agentLoopResults: ["x"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_agent",
            preset: "smart",
            skill: "news-query",
            prompt: "q",
            tools: ["search_news"],
            maxIterations: 3,
            bind: "a",
          },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(engine.endedAgentLoopIds.length).toBe(1);
    expect(engine.endedAgentLoopIds[0]).toMatch(/__agent:a$/);
  });

  it("ends the spawned session even when child.run() throws", async () => {
    const engine = makeMockEngine();
    // Override startAgentLoop to return a handle whose run throws.
    engine.startAgentLoop = async (loopOpts: AgentLoopOpts) => {
      engine.agentLoopStarts.push(loopOpts);
      return {
        messages: [],
        run: async () => {
          throw new Error("child crashed");
        },
      };
    };
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_agent",
            preset: "smart",
            skill: "news-query",
            prompt: "q",
            tools: ["search_news"],
            maxIterations: 3,
            bind: "a",
          },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    expect(engine.endedAgentLoopIds.length).toBe(1);
  });
});

describe("executor.execute — parallel step", () => {
  it("runs children concurrently and binds each", async () => {
    const order: string[] = [];
    const engine = makeMockEngine({
      toolResponses: {
        a: () => {
          order.push("a-start");
          return "A";
        },
        b: () => {
          order.push("b-start");
          return "B";
        },
      },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "parallel",
            steps: [
              { kind: "tool", tool: "a", args: {}, bind: "x" },
              { kind: "tool", tool: "b", args: {}, bind: "y" },
            ],
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("x")).toBe("A");
    expect(ctx.store.get("y")).toBe("B");
    // Both started before either is bound (Promise.all semantics) —
    // we can at least assert both happened.
    expect(order).toContain("a-start");
    expect(order).toContain("b-start");
  });

  it("fails fast when any child step fails", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        good: "ok",
        bad: "[tool error] nope",
      },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "parallel",
            steps: [
              { kind: "tool", tool: "good", args: {}, bind: "g" },
              { kind: "tool", tool: "bad", args: {}, bind: "b" },
            ],
          },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.step.kind).toBe("parallel");
      expect(r.reason).toBe("tool_error");
    }
  });
});

describe("executor.execute — terminal and end-of-list", () => {
  it("stops at explicit terminal mid-plan", async () => {
    const engine = makeMockEngine({
      toolResponses: { a: "A", b: "B" },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "a", args: {}, bind: "x" },
          { kind: "terminal" },
          { kind: "tool", tool: "b", args: {}, bind: "y" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.has("x")).toBe(true);
    expect(ctx.store.has("y")).toBe(false);
    expect(engine.toolCalls.length).toBe(1);
  });

  it("succeeds when plan ends without explicit terminal", async () => {
    const engine = makeMockEngine({
      toolResponses: { a: "A" },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [{ kind: "tool", tool: "a", args: {}, bind: "x" }],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    expect(ctx.store.get("x")).toBe("A");
  });
});

describe("executor.execute — replan step", () => {
  it("stops the pass and returns the named bindings as replan context", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        get_history: JSON.stringify([{ id: 1 }, { id: 2 }]),
        later: "L",
      },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "get_history", args: {}, bind: "history" },
          { kind: "replan", context: ["history"], note: "decide" },
          { kind: "tool", tool: "later", args: {}, bind: "after" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    if (!r.ok) throw new Error("expected ok");
    expect(r.replan).toBeDefined();
    expect(r.replan?.note).toBe("decide");
    expect(r.replan?.context).toEqual({ history: [{ id: 1 }, { id: 2 }] });
    // Steps after replan don't run — the pass terminates there.
    expect(ctx.store.has("after")).toBe(false);
    expect(engine.toolCalls.map((c) => c.tool)).toEqual(["get_history"]);
  });

  it("drops context names that were never bound, keeping the rest", async () => {
    const engine = makeMockEngine({ toolResponses: { a: "A" } });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "a", args: {}, bind: "x" },
          { kind: "replan", context: ["x", "missing"] },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    if (!r.ok) throw new Error("expected ok");
    expect(r.replan?.context).toEqual({ x: "A" });
  });
});

describe("executor.execute — duplicate binding", () => {
  it("surfaces duplicate_binding reason", async () => {
    const engine = makeMockEngine({
      toolResponses: { a: "A", b: "B" },
    });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "a", args: {}, bind: "x" },
          { kind: "tool", tool: "b", args: {}, bind: "x" },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("duplicate_binding");
      expect(r.stepIndex).toBe(1);
    }
  });
});

describe("executor.execute — end-to-end fixture", () => {
  it("runs a news-digest-shaped plan to completion", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        list_news: JSON.stringify({ count: 1, items: [{ id: 1, body: "post" }] }),
        get_telegram_chat_history: JSON.stringify({ messages: [] }),
        send_telegram_message: JSON.stringify({ delivered: true }),
        // No set_memory here on purpose: it is NOT an MCP tool. It is
        // dispatched to the injected setMemory writer below, never to
        // engine.mcp.callTool.
      },
      llmResponses: ["📰 Новости · 3 июня\n• fake digest"],
    });
    const memWrites: Array<[string, string]> = [];
    const executor = createExecutor({
      engine,
      readSkill: fixedReadSkill({ "news-digest": "DIGEST RULES" }),
      setMemory: (k, v) => memWrites.push([k, v]),
    });

    const ctx = baseCtx();
    ctx.store.set("watermark", "2026-06-02T18:00:00Z");
    ctx.store.set("now", "2026-06-03T05:05:00Z");

    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [
            {
              kind: "tool",
              tool: "list_news",
              args: { source: "channel", sinceISO: "${watermark}" },
              bind: "posts",
            },
            {
              kind: "tool",
              tool: "get_telegram_chat_history",
              args: { chatId: "${env.chatId}", limit: 5 },
              bind: "history",
            },
          ],
        },
        {
          kind: "llm_compose",
          preset: "smart",
          skill: "news-digest",
          input: { posts: "${posts}", history: "${history}" },
          bind: "digest",
        },
        {
          kind: "parallel",
          steps: [
            {
              kind: "tool",
              tool: "send_telegram_message",
              args: { chatId: "${env.chatId}", text: "${digest}" },
            },
            {
              kind: "tool",
              tool: "set_memory",
              args: { key: "news_digest.last_read_at", value: "${now}" },
            },
          ],
        },
        { kind: "terminal" },
      ],
    };

    // Validate plan against the schema first, like the real compiler would.
    const { WorkflowSchema } = createWorkflowSchema({
      knownTools: [
        "list_news",
        "get_telegram_chat_history",
        "send_telegram_message",
        "set_memory",
      ],
      knownSkills: ["news-digest"],
    });
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);

    const r = await executor.execute(plan, ctx);
    expect(r.ok).toBe(true);
    expect(ctx.store.get("digest")).toContain("📰 Новости");

    // Telegram fired via MCP; the watermark went to the injected writer,
    // NOT through engine.mcp.callTool (set_memory has no MCP counterpart).
    const toolNames = engine.toolCalls.map((c) => c.tool);
    expect(toolNames).toContain("send_telegram_message");
    expect(toolNames).not.toContain("set_memory");
    expect(memWrites).toEqual([
      ["news_digest.last_read_at", "2026-06-03T05:05:00Z"],
    ]);
    const send = engine.toolCalls.find((c) => c.tool === "send_telegram_message")!;
    expect(send.args).toEqual({
      chatId: 42,
      text: "📰 Новости · 3 июня\n• fake digest",
    });
  });
});

// ─── internal helpers via __testing ──────────────────────────────────

describe("renderInputAsXml (internal)", () => {
  beforeEach(() => {});

  it("returns empty string for empty input", () => {
    expect(__testing.renderInputAsXml({})).toBe("");
  });

  it("emits a tag per entry", () => {
    const out = __testing.renderInputAsXml({ a: "x", b: "y" });
    expect(out).toBe("<a>\nx\n</a>\n\n<b>\ny\n</b>");
  });

  it("JSON-stringifies non-string values", () => {
    const out = __testing.renderInputAsXml({ x: { v: 1 } });
    expect(out).toContain('"v": 1');
  });
});

describe("classifyError (internal)", () => {
  it("recognises ToolCallError", () => {
    const e = new __testing.ToolCallError("x", "msg");
    expect(__testing.classifyError(e)).toBe("tool_error");
  });
  it("recognises SkillNotFoundError", () => {
    const e = new __testing.SkillNotFoundError("ghost");
    expect(__testing.classifyError(e)).toBe("skill_not_found");
  });
  it("recognises LlmCallError", () => {
    const e = new __testing.LlmCallError("oops");
    expect(__testing.classifyError(e)).toBe("llm_error");
  });
  it("falls back to step_failed for unknown errors", () => {
    expect(__testing.classifyError(new Error("?"))).toBe("step_failed");
  });
});
