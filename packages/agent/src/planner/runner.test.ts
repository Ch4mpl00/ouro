import { beforeEach, describe, expect, it } from "vitest";
import type OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { SessionOpts } from "../session";
import type { Generation, Span, Trace, TraceContext } from "../tracing";
import { createPlanSchema, type Plan } from "./dsl";
import {
  createRunner,
  type EngineSurface,
  type SubSessionHandle,
  __testing,
} from "./runner";
import { createStore } from "./substitute";

// ─── shared mocks ────────────────────────────────────────────────────

const PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
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
  };
  return span;
}

function recordingTrace(): Trace {
  const root = recordingSpan();
  return {
    update: root.update,
    generation: root.generation,
    span: root.span,
    end: () => {},
  };
}

interface MockCall {
  tool: string;
  args: Record<string, unknown>;
}

interface MockEngineOpts {
  toolResponses?: Record<string, string | ((args: Record<string, unknown>) => string)>;
  llmResponses?: string[];
  sessionResults?: string[];
  startSessionThrows?: Error;
}

function makeMockEngine(opts: MockEngineOpts = {}): EngineSurface & {
  toolCalls: MockCall[];
  llmCalls: unknown[];
  sessionStarts: SessionOpts[];
  endedSessionIds: string[];
} {
  const toolCalls: MockCall[] = [];
  const llmCalls: unknown[] = [];
  const sessionStarts: SessionOpts[] = [];
  const endedSessionIds: string[] = [];
  const llmQueue = [...(opts.llmResponses ?? [])];
  const sessionQueue = [...(opts.sessionResults ?? [])];

  const openaiClient = {
    chat: {
      completions: {
        create: async (body: unknown) => {
          llmCalls.push(body);
          const text = llmQueue.shift() ?? "";
          return {
            choices: [{ message: { content: text } }],
            usage: { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150 },
          };
        },
      },
    },
  } as unknown as OpenAI;

  return {
    presets: PRESETS,
    resolveProvider(model: string) {
      return {
        client: openaiClient,
        kind: model.startsWith("deepseek") ? "deepseek" : "openai",
      };
    },
    mcp: {
      callTool: async (name, args) => {
        toolCalls.push({ tool: name, args });
        const r = opts.toolResponses?.[name];
        if (r === undefined) return `[tool error] unknown tool ${name}`;
        return typeof r === "function" ? r(args) : r;
      },
    },
    startSession: async (sessionOpts: SessionOpts): Promise<SubSessionHandle> => {
      if (opts.startSessionThrows) throw opts.startSessionThrows;
      sessionStarts.push(sessionOpts);
      const result = sessionQueue.shift() ?? "";
      const messages: ChatCompletionMessageParam[] = [];
      return {
        messages,
        run: async () => result,
      };
    },
    endSession: (id: string) => {
      endedSessionIds.push(id);
    },
    toolCalls,
    llmCalls,
    sessionStarts,
    endedSessionIds,
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

describe("runner.run — tool step", () => {
  it("calls mcp.callTool with substituted args and binds parsed JSON result", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        list_news: JSON.stringify({ count: 2, items: [{ id: 1 }, { id: 2 }] }),
      },
    });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });

    const plan: Plan = {
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
    const r = await runner.run(plan, ctx);

    expect(r.ok).toBe(true);
    expect(engine.toolCalls).toEqual([
      { tool: "list_news", args: { chatId: 42, limit: 5 } },
    ]);
    expect(ctx.store.get("posts")).toEqual({
      count: 2,
      items: [{ id: 1 }, { id: 2 }],
    });
  });

  it("works without bind (fire-and-forget)", async () => {
    const engine = makeMockEngine({
      toolResponses: { set_memory: "ok" },
    });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "set_memory", args: { key: "x", value: "y" } },
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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

describe("runner.run — llm_compose step", () => {
  it("loads skill as system, builds user from prompt + XML input, calls LLM without tools", async () => {
    const engine = makeMockEngine({ llmResponses: ["composed digest"] });
    const runner = createRunner({
      engine,
      readSkill: fixedReadSkill({ "news-digest": "RULES go here" }),
    });

    const ctx = baseCtx();
    ctx.store.set("posts", [{ id: 1 }]);
    const r = await runner.run(
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

  it("works with prompt-only (no skill)", async () => {
    const engine = makeMockEngine({ llmResponses: ["A: 5"] });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    ctx.store.set("question", "what is 2+3");
    const r = await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    ctx.store.set("items", ["a", "b"]);
    await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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

describe("runner.run — llm_agent step", () => {
  it("spawns child session with toolWhitelist and binds result", async () => {
    const engine = makeMockEngine({ sessionResults: ["agent answer"] });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    ctx.store.set("query", "что в Одессе");
    const r = await runner.run(
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

    expect(engine.sessionStarts.length).toBe(1);
    const opts = engine.sessionStarts[0]!;
    expect(opts.skills).toEqual(["news-query"]);
    expect(opts.includeEngineSkills).toBe(false);
    expect(opts.preset).toBe("smart");
    expect(opts.maxIterations).toBe(5);
    expect(opts.parentId).toBe("test:1");
    expect(opts.toolWhitelist).toEqual(new Set(["search_news", "list_news"]));
  });

  it("ends the spawned session on success", async () => {
    const engine = makeMockEngine({ sessionResults: ["x"] });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    await runner.run(
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
    expect(engine.endedSessionIds.length).toBe(1);
    expect(engine.endedSessionIds[0]).toMatch(/__agent:a$/);
  });

  it("ends the spawned session even when child.run() throws", async () => {
    const engine = makeMockEngine();
    // Override startSession to return a handle whose run throws.
    engine.startSession = async (sessionOpts: SessionOpts) => {
      engine.sessionStarts.push(sessionOpts);
      return {
        messages: [],
        run: async () => {
          throw new Error("child crashed");
        },
      };
    };
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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
    expect(engine.endedSessionIds.length).toBe(1);
  });
});

describe("runner.run — parallel step", () => {
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    const r = await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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

describe("runner.run — terminal and end-of-list", () => {
  it("stops at explicit terminal mid-plan", async () => {
    const engine = makeMockEngine({
      toolResponses: { a: "A", b: "B" },
    });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    const r = await runner.run(
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
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const ctx = baseCtx();
    const r = await runner.run(
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

describe("runner.run — duplicate binding", () => {
  it("surfaces duplicate_binding reason", async () => {
    const engine = makeMockEngine({
      toolResponses: { a: "A", b: "B" },
    });
    const runner = createRunner({ engine, readSkill: nullReadSkill() });
    const r = await runner.run(
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

describe("runner.run — end-to-end fixture", () => {
  it("runs a news-digest-shaped plan to completion", async () => {
    const engine = makeMockEngine({
      toolResponses: {
        list_news: JSON.stringify({ count: 1, items: [{ id: 1, body: "post" }] }),
        get_telegram_chat_history: JSON.stringify({ messages: [] }),
        send_telegram_message: JSON.stringify({ delivered: true }),
        set_memory: "ok",
      },
      llmResponses: ["📰 Новости · 3 июня\n• fake digest"],
    });
    const runner = createRunner({
      engine,
      readSkill: fixedReadSkill({ "news-digest": "DIGEST RULES" }),
    });

    const ctx = baseCtx();
    ctx.store.set("watermark", "2026-06-02T18:00:00Z");
    ctx.store.set("now", "2026-06-03T05:05:00Z");

    const plan: Plan = {
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

    // Validate plan against the schema first, like the real planner would.
    const { PlanSchema } = createPlanSchema({
      knownTools: [
        "list_news",
        "get_telegram_chat_history",
        "send_telegram_message",
        "set_memory",
      ],
      knownSkills: ["news-digest"],
    });
    expect(PlanSchema.safeParse(plan).success).toBe(true);

    const r = await runner.run(plan, ctx);
    expect(r.ok).toBe(true);
    expect(ctx.store.get("digest")).toContain("📰 Новости");

    // Telegram + memory both fired.
    const toolNames = engine.toolCalls.map((c) => c.tool);
    expect(toolNames).toContain("send_telegram_message");
    expect(toolNames).toContain("set_memory");
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

describe("buildLlmComposeBody (internal)", () => {
  it("OpenAI: no thinking / no reasoning_effort", () => {
    const body = __testing.buildLlmComposeBody(
      PRESETS.base,
      "openai",
      [{ role: "user", content: "x" }],
    );
    expect(body).toEqual({
      model: "gpt-5.4-mini",
      messages: [{ role: "user", content: "x" }],
    });
  });

  it("DeepSeek + disabled effort: adds thinking:disabled, no reasoning_effort", () => {
    const body = __testing.buildLlmComposeBody(
      { model: "deepseek-chat", reasoningEffort: "disabled" },
      "deepseek",
      [],
    );
    expect(body).toMatchObject({
      thinking: { type: "disabled" },
    });
    expect((body as { reasoning_effort?: string }).reasoning_effort).toBeUndefined();
  });

  it("DeepSeek + max effort: adds thinking:enabled + reasoning_effort:max", () => {
    const body = __testing.buildLlmComposeBody(
      PRESETS.smart,
      "deepseek",
      [],
    );
    expect(body).toMatchObject({
      thinking: { type: "enabled" },
      reasoning_effort: "max",
    });
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
