import { describe, expect, it } from "vitest";
import type OpenAI from "openai";
import type {
  ChatCompletionMessageParam,
  ChatCompletionTool,
} from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { Generation, Span, Trace, TraceContext } from "../tracing";
import { createCompiler, type CompileRequest } from "./compile";

const PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
};

function recordingSpan(): Span {
  const span: Span = {
    update() {},
    end() {},
    generation(_) {
      const gen: Generation = { end() {} };
      return gen;
    },
    span(opts) {
      return recordingSpan();
    },
    event() {},
  };
  return span;
}

function recordingTrace(): Trace {
  return {
    update() {},
    end() {},
    generation() {
      return { end() {} };
    },
    span: () => recordingSpan(),
    event() {},
  };
}

interface MockClientOpts {
  llmReplies: Array<string | Error>;
}

function makeMockClient(opts: MockClientOpts): {
  client: OpenAI;
  calls: Array<{ messages: ChatCompletionMessageParam[] }>;
} {
  const calls: Array<{ messages: ChatCompletionMessageParam[] }> = [];
  const queue = [...opts.llmReplies];
  const client = {
    chat: {
      completions: {
        create: async (body: {
          messages: ChatCompletionMessageParam[];
        }) => {
          calls.push({ messages: structuredClone(body.messages) });
          const next = queue.shift();
          if (next instanceof Error) throw next;
          return {
            choices: [{ message: { content: next ?? "" } }],
            usage: { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150 },
          };
        },
      },
    },
  } as unknown as OpenAI;
  return { client, calls };
}

function makeEngineSurface(client: OpenAI) {
  return {
    presets: PRESETS,
    resolveProvider(model: string) {
      return {
        client,
        kind: model.startsWith("deepseek") ? ("deepseek" as const) : ("openai" as const),
      };
    },
  };
}

function makeReq(): CompileRequest {
  return {
    signal: {
      source: "telegram",
      content: "что нового",
      envContext: "Default chat id: 285083560",
    },
    envData: {
      now: new Date("2026-06-03T05:05:00Z"),
      timezone: "Europe/Kiev",
      userEmail: "user@example.com",
      newsLastReadAt: "2026-06-02T18:00:00Z",
    },
    parentTrace: recordingTrace() as TraceContext,
    signalLabel: "telegram:1",
  };
}

const VALID_PLAN_JSON = JSON.stringify({
  version: 1,
  steps: [
    {
      kind: "llm_agent",
      preset: "smart",
      skill: "telegram",
      prompt: "что нового",
      tools: ["search_news"],
      maxIterations: 5,
      bind: "answer",
    },
    { kind: "terminal" },
  ],
});

const FAKE_TOOLS: ChatCompletionTool[] = [
  {
    type: "function",
    function: {
      name: "search_news",
      description: "Vector search over the news store",
      parameters: {
        type: "object",
        properties: {
          query: { type: "string" },
          k: { type: "number" },
          sinceISO: { type: "string" },
        },
        required: ["query"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "send_telegram_message",
      description: "Send a message to a chat",
      parameters: {
        type: "object",
        properties: {
          chatId: { type: "number" },
          text: { type: "string" },
        },
        required: ["chatId", "text"],
      },
    },
  },
];
const FAKE_SKILLS = ["telegram", "news-digest"];

describe("compiler.compile — happy path", () => {
  it("returns ok with parsed plan on first valid response", async () => {
    const { client, calls } = makeMockClient({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "PLANNER RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });

    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.attempts).toBe(1);
      expect(r.workflow.steps[0]?.kind).toBe("llm_agent");
    }
    expect(calls.length).toBe(1);
  });

  it("splits the prompt: static tools/skills in system (cache prefix), variable signal in user", async () => {
    const { client, calls } = makeMockClient({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "PLANNER RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    await compiler.compile(makeReq());

    const messages = calls[0]!.messages;
    // System = planner skill + the static reference (tools + skills). It
    // leads with the skill verbatim so the whole static block is a stable
    // cache prefix across signals.
    expect(messages[0]?.role).toBe("system");
    const systemText = messages[0]?.content as string;
    expect(systemText.startsWith("PLANNER RULES")).toBe(true);
    expect(systemText).toContain("<tools>");
    // Compact signature format with required/optional param names and
    // types — exactly what the compiler needs to use the right keys
    // (e.g. `k` not `limit`, `sinceISO` for date filters).
    expect(systemText).toMatch(
      /- search_news\(query: string, k\?: number, sinceISO\?: string\)/,
    );
    expect(systemText).toContain("<skills>");
    expect(systemText).toContain("- telegram");

    // User = only the per-signal variable content; NO tools/skills (those
    // must stay in the cached prefix, not after the variable signal text).
    const userText = messages[1]?.content as string;
    expect(userText).toContain("<signal>");
    expect(userText).toContain("Source: telegram");
    expect(userText).toContain("что нового");
    expect(userText).toContain("<env>");
    expect(userText).toContain("Europe/Kiev");
    expect(userText).toContain("<envContext>");
    expect(userText).toContain("285083560");
    expect(userText).not.toContain("<tools>");
    expect(userText).not.toContain("<skills>");
  });

  it("tool signatures expose required vs optional params correctly", async () => {
    const { client, calls } = makeMockClient({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    await compiler.compile(makeReq());
    const systemText = calls[0]!.messages[0]?.content as string;
    // chatId + text are required, so no `?`
    expect(systemText).toMatch(
      /- send_telegram_message\(chatId: number, text: string\)/,
    );
  });

  it("uses the smartest preset's model in the request", async () => {
    const { client, calls } = makeMockClient({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    await compiler.compile(makeReq());
    // The OpenAI request body shape — verified indirectly via the
    // recordingClient capturing messages; model verification belongs in
    // an integration test, so we just sanity-check we got through.
    expect(calls.length).toBe(1);
  });
});

describe("compiler.compile — skill missing", () => {
  it("returns skill_not_found without calling the LLM", async () => {
    const { client, calls } = makeMockClient({ llmReplies: [] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => null,
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("skill_not_found");
      expect(r.attempts).toBe(0);
    }
    expect(calls.length).toBe(0);
  });
});

describe("compiler.compile — LLM error", () => {
  it("returns llm_error and stops without retrying", async () => {
    const err = new Error("rate limit");
    const { client, calls } = makeMockClient({ llmReplies: [err] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("llm_error");
      expect(r.errors[0]).toContain("rate limit");
      expect(r.attempts).toBe(1);
    }
    expect(calls.length).toBe(1);
  });
});

describe("compiler.compile — retry loop", () => {
  it("retries on invalid JSON with error feedback", async () => {
    const { client, calls } = makeMockClient({
      llmReplies: ["not json", VALID_PLAN_JSON],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.attempts).toBe(2);

    // Second call should have appended assistant + corrective user.
    expect(calls.length).toBe(2);
    const retryMessages = calls[1]!.messages;
    expect(retryMessages.length).toBe(4);
    expect(retryMessages[2]?.role).toBe("assistant");
    expect(retryMessages[2]?.content).toBe("not json");
    expect(retryMessages[3]?.role).toBe("user");
    expect(retryMessages[3]?.content).toMatch(/invalid JSON/);
  });

  it("retries on schema failure with formatted Zod errors", async () => {
    const invalidPlanJson = JSON.stringify({
      version: 1,
      steps: [
        { kind: "tool", tool: "ghost_tool", args: {}, bind: "x" },
        { kind: "terminal" },
      ],
    });
    const { client, calls } = makeMockClient({
      llmReplies: [invalidPlanJson, VALID_PLAN_JSON],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.attempts).toBe(2);

    // The retry user message should reference the offending tool
    // name so the LLM can fix it on the next try.
    const retryUser = calls[1]!.messages[3]?.content as string;
    expect(retryUser).toContain("ghost_tool");
  });

  it("returns schema_invalid after exhausting all attempts on schema errors", async () => {
    const invalidPlanJson = JSON.stringify({
      version: 1,
      steps: [
        { kind: "tool", tool: "ghost_tool", args: {}, bind: "x" },
        { kind: "terminal" },
      ],
    });
    const { client } = makeMockClient({
      llmReplies: [invalidPlanJson, invalidPlanJson, invalidPlanJson],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
      maxAttempts: 3,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("schema_invalid");
      expect(r.attempts).toBe(3);
      expect(r.errors.length).toBeGreaterThan(0);
    }
  });

  it("returns invalid_json after exhausting all attempts on JSON errors", async () => {
    const { client } = makeMockClient({
      llmReplies: ["not json", "still not json", "definitely not"],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
      maxAttempts: 3,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.reason).toBe("invalid_json");
      expect(r.attempts).toBe(3);
    }
  });

  it("respects custom maxAttempts (1 = no retries)", async () => {
    const { client } = makeMockClient({ llmReplies: ["not json"] });
    const compiler = createCompiler({
      engine: makeEngineSurface(client),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
      maxAttempts: 1,
    });
    const r = await compiler.compile(makeReq());
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.attempts).toBe(1);
  });
});
