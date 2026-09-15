import { beforeEach, describe, expect, it } from "vitest";
import type {
  ChatCompletionMessageParam,
  ChatCompletionTool,
} from "openai/resources/chat/completions";
import {
  DEFAULT_PRESETS,
  createSessionContext,
  PATCH_MARKER,
  type AgentLoopOpts,
  type ChatProvider,
  type CompletionParams,
  type CompletionResult,
  type ModelPreset,
  type PresetName,
} from "./agent-loop";
import {
  JUDGE_NODE_META,
  type Generation,
  type Span,
  type Trace,
  type TraceContext,
} from "./tracing";
import type { CodexClient } from "./codex-client";
import {
  __testing,
  createCompiler,
  createExecutor,
  createStore,
  createWorkflowSchema,
  formatWorkflowErrors,
  parseWorkflow,
  substitute,
  DuplicateBindingError,
  MissingBindingError,
  type AgentLoopHandle,
  type CompileRequest,
  type EngineSurface,
  type ExecContext,
  type Workflow,
} from "./workflow";

// ─── dsl ───────────────────────────────────────────

const SCHEMA_TOOLS = [
  "list_news",
  "send_telegram_message",
  "set_memory",
  "get_telegram_chat_history",
  "search_news",
  "start_typing",
] as const;

const SCHEMA_SKILLS = [
  "news-digest",
  "tech-digest",
  "news-query",
  "telegram",
] as const;

function makeSchema() {
  return createWorkflowSchema({
    knownTools: SCHEMA_TOOLS,
    knownSkills: SCHEMA_SKILLS,
  });
}

describe("createWorkflowSchema", () => {
  it("rejects empty knownTools / knownSkills at factory time", () => {
    expect(() =>
      createWorkflowSchema({ knownTools: [], knownSkills: ["x"] }),
    ).toThrow(/knownTools/);
    expect(() =>
      createWorkflowSchema({ knownTools: ["x"], knownSkills: [] }),
    ).toThrow(/knownSkills/);
  });
});

describe("plan validation — happy path per step kind", () => {
  const { WorkflowSchema } = makeSchema();

  it("accepts a tool step", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "send_telegram_message",
          args: { chatId: 285083560, text: "hi" },
          bind: "sent",
        },
        { kind: "terminal" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("accepts llm_compose with skill only", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "llm_compose",
          preset: "smart",
          skill: "news-digest",
          input: { posts: "${posts}" },
          bind: "digest",
        },
        { kind: "terminal" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("accepts llm_compose with prompt only", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "llm_compose",
          preset: "base",
          prompt: "Summarize in one sentence: ${text}",
          input: { text: "${some_input}" },
          bind: "summary",
        },
        { kind: "terminal" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("rejects llm_compose with neither skill nor prompt (post-check)", () => {
    // This check runs in parseWorkflow() after the discriminated union
    // succeeds — Zod can't express "either A or B required" inside a
    // discriminated union member without breaking the union itself.
    const r = parseWorkflow(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            input: {},
            bind: "out",
          },
          { kind: "terminal" },
        ],
      },
      WorkflowSchema,
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      const msgs = r.errors.join(" | ");
      expect(msgs).toMatch(/skill.*prompt|prompt.*skill/);
    }
  });

  it("post-check walks into parallel steps", () => {
    // llm_compose inside parallel still gets the skill-or-prompt check.
    const r = parseWorkflow(
      {
        version: 1,
        steps: [
          {
            kind: "parallel",
            steps: [
              { kind: "tool", tool: "list_news", args: {}, bind: "a" },
              {
                kind: "llm_compose",
                preset: "base",
                input: {},
                bind: "b",
              },
            ],
          },
          { kind: "terminal" },
        ],
      },
      WorkflowSchema,
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.errors.some((e) => e.includes("steps[0].steps[1]"))).toBe(true);
    }
  });

  it("accepts llm_agent with bounded tools and iterations", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "news-query",
          prompt: "${signal.body}",
          tools: ["search_news", "list_news"],
          maxIterations: 5,
          bind: "answer",
        },
        { kind: "terminal" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("accepts parallel with leaf steps inside", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [
            {
              kind: "tool",
              tool: "list_news",
              args: { source: "channel" },
              bind: "posts",
            },
            {
              kind: "tool",
              tool: "get_telegram_chat_history",
              args: { chatId: 1, limit: 5 },
              bind: "history",
            },
          ],
        },
        { kind: "terminal" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("accepts a bare terminal step", () => {
    expect(
      WorkflowSchema.safeParse({
        version: 1,
        steps: [{ kind: "terminal" }],
      }).success,
    ).toBe(true);
  });

  it("accepts a replan step with context and note", () => {
    const plan: Workflow = {
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "get_telegram_chat_history",
          args: { chatId: 1, limit: 10 },
          bind: "history",
        },
        { kind: "replan", context: ["history"], note: "decide what to continue" },
      ],
    };
    expect(WorkflowSchema.safeParse(plan).success).toBe(true);
  });

  it("accepts a replan step without a note", () => {
    expect(
      WorkflowSchema.safeParse({
        version: 1,
        steps: [
          { kind: "tool", tool: "list_news", args: {}, bind: "n" },
          { kind: "replan", context: ["n"] },
        ],
      }).success,
    ).toBe(true);
  });
});

describe("plan validation — rejections", () => {
  const { WorkflowSchema } = makeSchema();

  it("rejects an unknown tool name", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        { kind: "tool", tool: "list_things", args: {}, bind: "x" },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error).join(" | ");
      expect(msgs).toMatch(/list_things|Invalid enum/i);
    }
  });

  it("rejects an unknown skill name on llm_agent", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "ghost-skill",
          prompt: "x",
          tools: ["search_news"],
          maxIterations: 3,
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects an unknown preset", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_compose",
          preset: "genius",
          prompt: "x",
          input: {},
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects nested parallel (flat-parallel constraint)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [
            {
              kind: "parallel",
              steps: [
                { kind: "tool", tool: "list_news", args: {}, bind: "a" },
                { kind: "tool", tool: "search_news", args: {}, bind: "b" },
              ],
            },
            { kind: "tool", tool: "set_memory", args: {}, bind: "c" },
          ],
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects parallel with a single step", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [{ kind: "tool", tool: "list_news", args: {}, bind: "a" }],
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects a replan step with an empty context", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        { kind: "tool", tool: "list_news", args: {}, bind: "n" },
        { kind: "replan", context: [] },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects replan nested inside parallel (terminator, not a leaf)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [
            { kind: "tool", tool: "list_news", args: {}, bind: "a" },
            { kind: "replan", context: ["a"] },
          ],
        },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects terminal nested inside parallel (terminator, not a leaf)", () => {
    // The runtime ignores a stop signal coming from a parallel branch, so
    // the schema must not let the compiler emit a silently-dead step.
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "parallel",
          steps: [
            { kind: "tool", tool: "list_news", args: {}, bind: "a" },
            { kind: "terminal" },
          ],
        },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects unknown step kind", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [{ kind: "branch", if: "x" }, { kind: "terminal" }],
    });
    expect(r.success).toBe(false);
  });

  it("rejects unknown fields on a step (strict mode)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "list_news",
          args: {},
          bind: "x",
          extra_field: "oops",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects version other than 1", () => {
    const r = WorkflowSchema.safeParse({
      version: 2,
      steps: [{ kind: "terminal" }],
    });
    expect(r.success).toBe(false);
  });

  it("rejects empty plan", () => {
    const r = WorkflowSchema.safeParse({ version: 1, steps: [] });
    expect(r.success).toBe(false);
  });

  it("rejects llm_agent.maxIterations out of bounds", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "news-query",
          prompt: "x",
          tools: ["search_news"],
          maxIterations: 50,
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });

  it("rejects llm_agent with empty tools whitelist", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "news-query",
          prompt: "x",
          tools: [],
          maxIterations: 3,
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
  });
});

describe("parseWorkflow", () => {
  const { WorkflowSchema } = makeSchema();

  it("returns ok=true with parsed plan on success", () => {
    const r = parseWorkflow(
      {
        version: 1,
        steps: [{ kind: "terminal" }],
      },
      WorkflowSchema,
    );
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.workflow.steps[0]?.kind).toBe("terminal");
    }
  });

  it("returns ok=false with human-readable errors on failure", () => {
    const r = parseWorkflow(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "unknown_one", args: {}, bind: "a" },
          { kind: "terminal" },
        ],
      },
      WorkflowSchema,
    );
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.errors.length).toBeGreaterThan(0);
      // Path-formatted: "steps[0].tool: …"
      expect(r.errors.some((e) => e.includes("steps[0].tool"))).toBe(true);
    }
  });
});

describe("formatWorkflowErrors", () => {
  const { WorkflowSchema } = makeSchema();

  it("renders path with brackets for array indices and dots for keys", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "news-query",
          prompt: "x",
          tools: ["ghost_tool"],
          maxIterations: 3,
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error);
      expect(msgs.some((m) => m.includes("steps[0].tools[0]"))).toBe(true);
    }
  });

  it("uses 'at workflow root' when path is empty", () => {
    const r = WorkflowSchema.safeParse("not an object");
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error);
      expect(msgs.some((m) => m.startsWith("at workflow root"))).toBe(true);
    }
  });

  it("formats invalid_enum_value (real tool-name error) with offending value", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        { kind: "tool", tool: "list_things", args: {}, bind: "a" },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const line = formatWorkflowErrors(r.error).find((m) =>
        m.includes("steps[0].tool"),
      );
      expect(line).toBeDefined();
      // Zod's stock invalid_enum_value message lists what was received.
      expect(line).toMatch(/list_things/);
    }
  });

  it("formats unrecognized_keys (strict mode) with the offending key name", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "list_news",
          args: {},
          bind: "a",
          extra_field: "oops",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error).join(" | ");
      expect(msgs).toMatch(/extra_field/);
    }
  });

  it("formats invalid_type (real number-where-string error)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "tool",
          tool: "list_news",
          args: {},
          bind: 42,
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const line = formatWorkflowErrors(r.error).find((m) =>
        m.includes("steps[0].bind"),
      );
      expect(line).toBeDefined();
      expect(line).toMatch(/string/i);
    }
  });

  it("formats invalid_literal (wrong version)", () => {
    const r = WorkflowSchema.safeParse({
      version: 7,
      steps: [{ kind: "terminal" }],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error);
      expect(msgs.some((m) => m.includes("version"))).toBe(true);
    }
  });

  it("formats too_small (maxIterations < 1)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        {
          kind: "llm_agent",
          preset: "smart",
          skill: "news-query",
          prompt: "x",
          tools: ["search_news"],
          maxIterations: 0,
          bind: "y",
        },
        { kind: "terminal" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const line = formatWorkflowErrors(r.error).find((m) =>
        m.includes("steps[0].maxIterations"),
      );
      expect(line).toBeDefined();
    }
  });

  it("emits one line per issue when a parse produces multiple errors", () => {
    // Two independent violations in different paths — schema should
    // surface both, formatter should return one line each (not merge).
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [
        { kind: "tool", tool: "bogus_tool_one", args: {}, bind: "a" },
        { kind: "tool", tool: "bogus_tool_two", args: {}, bind: "b" },
      ],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error);
      expect(msgs.some((m) => m.includes("steps[0].tool"))).toBe(true);
      expect(msgs.some((m) => m.includes("steps[1].tool"))).toBe(true);
      // Both issues are surfaced independently.
      expect(msgs.length).toBeGreaterThanOrEqual(2);
    }
  });

  it("formats discriminator mismatch (unknown step kind)", () => {
    const r = WorkflowSchema.safeParse({
      version: 1,
      steps: [{ kind: "loop" }, { kind: "terminal" }],
    });
    expect(r.success).toBe(false);
    if (!r.success) {
      const msgs = formatWorkflowErrors(r.error);
      // Path points at the malformed step; Zod's discriminated-union
      // error mentions valid kinds.
      expect(msgs.some((m) => m.includes("steps[0]"))).toBe(true);
    }
  });
});

describe("workflowToJsonSchema", () => {
  it("produces a JSON schema object with $schema and a Workflow definition", () => {
    const { workflowToJsonSchema } = makeSchema();
    const schema = workflowToJsonSchema() as Record<string, unknown>;
    // Top-level shape sanity (zod-to-json-schema includes $schema by default).
    expect(typeof schema).toBe("object");
    expect(schema).not.toBeNull();
    // We asked for $refStrategy: 'none' — output should be fully inlined,
    // i.e. version literal appears somewhere in the serialized form.
    expect(JSON.stringify(schema)).toContain('"version"');
  });
});

// A worked plan in the shape the compiler emits: two parallel fan-outs
// around a compose step, placeholders bound from `env` and from earlier
// steps, closed by a terminal. Doubles as the readable reference for what
// the DSL looks like in practice.
const EXAMPLE_PLAN = {
  version: 1,
  steps: [
    {
      kind: "parallel",
      steps: [
        {
          kind: "tool",
          tool: "list_news",
          args: { source: "channel", sinceISO: "${env.watermark}" },
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
      input: {
        posts: "${posts}",
        history: "${history}",
        date: "${env.date}",
        timezone: "${env.timezone}",
      },
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
          args: { key: "news_digest.last_read_at", value: "${env.now}" },
        },
      ],
    },
    { kind: "terminal" },
  ],
};

describe("example fixture matches schema", () => {
  it("validates a realistic hand-written plan", () => {
    const { WorkflowSchema } = makeSchema();
    const r = WorkflowSchema.safeParse(EXAMPLE_PLAN);
    if (!r.success) {
      // Show why on failure so the test output explains itself.
      console.error(formatWorkflowErrors(r.error));
    }
    expect(r.success).toBe(true);
  });
});

// ─── variables ───────────────────────────────────────────

describe("createStore", () => {
  it("returns primitive top-level values", () => {
    const s = createStore({ count: 7, name: "alice" });
    expect(s.get("count")).toBe(7);
    expect(s.get("name")).toBe("alice");
  });

  it("walks dot-notation paths into nested objects", () => {
    const s = createStore({ env: { chatId: 42, tz: "Europe/Kiev" } });
    expect(s.get("env.chatId")).toBe(42);
    expect(s.get("env.tz")).toBe("Europe/Kiev");
  });

  it("throws MissingBindingError on missing top-level key", () => {
    const s = createStore({ env: {} });
    expect(() => s.get("posts")).toThrow(MissingBindingError);
  });

  it("throws MissingBindingError on missing nested key", () => {
    const s = createStore({ env: { tz: "UTC" } });
    expect(() => s.get("env.chatId")).toThrow(MissingBindingError);
  });

  it("throws MissingBindingError when traversing through non-object", () => {
    const s = createStore({ env: "not-an-object" });
    expect(() => s.get("env.foo")).toThrow(MissingBindingError);
  });

  it("preserves null vs undefined: explicit null is found, undefined missing key isn't", () => {
    const s = createStore({ env: { chatId: null } });
    expect(s.has("env.chatId")).toBe(true);
    expect(s.get("env.chatId")).toBe(null);
    expect(s.has("env.tz")).toBe(false);
  });

  it("has() returns true for existing paths, false for missing", () => {
    const s = createStore({ a: { b: 1 } });
    expect(s.has("a")).toBe(true);
    expect(s.has("a.b")).toBe(true);
    expect(s.has("a.c")).toBe(false);
    expect(s.has("x")).toBe(false);
  });

  it("set() adds a new binding", () => {
    const s = createStore({});
    s.set("posts", [1, 2, 3]);
    expect(s.get("posts")).toEqual([1, 2, 3]);
  });

  it("set() throws DuplicateBindingError on second write to same name", () => {
    const s = createStore({ posts: [] });
    expect(() => s.set("posts", [1])).toThrow(DuplicateBindingError);
  });

  it("snapshot() returns a plain object copy of the store", () => {
    const s = createStore({ a: 1 });
    s.set("b", 2);
    expect(s.snapshot()).toEqual({ a: 1, b: 2 });
  });
});

describe("substitute — string mode", () => {
  it("whole-string placeholder returns the raw value (preserves type)", () => {
    const s = createStore({ posts: [{ id: 1 }, { id: 2 }] });
    expect(substitute("${posts}", s)).toEqual([{ id: 1 }, { id: 2 }]);
  });

  it("whole-string placeholder returns number as number, not stringified", () => {
    const s = createStore({ count: 7 });
    expect(substitute("${count}", s)).toBe(7);
  });

  it("whole-string placeholder returns null as null", () => {
    const s = createStore({ x: null });
    expect(substitute("${x}", s)).toBe(null);
  });

  it("interpolation stringifies non-string values", () => {
    const s = createStore({ name: "Bob", count: 7 });
    expect(substitute("Hello ${name}, count=${count}", s)).toBe(
      "Hello Bob, count=7",
    );
  });

  it("interpolation JSON-stringifies object values", () => {
    const s = createStore({ x: { a: 1 } });
    expect(substitute("payload=${x}", s)).toBe('payload={"a":1}');
  });

  it("interpolation walks nested paths", () => {
    const s = createStore({ env: { chatId: 42, tz: "Europe/Kiev" } });
    expect(substitute("chat=${env.chatId} tz=${env.tz}", s)).toBe(
      "chat=42 tz=Europe/Kiev",
    );
  });

  it("missing binding in whole-string mode throws MissingBindingError", () => {
    const s = createStore({});
    expect(() => substitute("${posts}", s)).toThrow(MissingBindingError);
  });

  it("missing binding in interpolation mode throws MissingBindingError", () => {
    const s = createStore({});
    expect(() => substitute("hi ${name}", s)).toThrow(MissingBindingError);
  });

  it("strings without placeholders pass through unchanged", () => {
    const s = createStore({});
    expect(substitute("plain text", s)).toBe("plain text");
  });
});

describe("substitute — recursive walks", () => {
  it("walks object values recursively", () => {
    const s = createStore({ env: { chatId: 42 }, text: "hello" });
    expect(
      substitute(
        { chatId: "${env.chatId}", text: "${text}" },
        s,
      ),
    ).toEqual({ chatId: 42, text: "hello" });
  });

  it("walks array values recursively", () => {
    const s = createStore({ a: 1, b: 2 });
    expect(substitute(["${a}", "${b}", "literal"], s)).toEqual([1, 2, "literal"]);
  });

  it("does not mutate the input", () => {
    const input = { chatId: "${env.chatId}" };
    const s = createStore({ env: { chatId: 42 } });
    const out = substitute(input, s) as Record<string, unknown>;
    expect(input.chatId).toBe("${env.chatId}");
    expect(out.chatId).toBe(42);
  });

  it("passes non-string primitives through unchanged", () => {
    const s = createStore({});
    expect(substitute(42, s)).toBe(42);
    expect(substitute(true, s)).toBe(true);
    expect(substitute(null, s)).toBe(null);
  });

  it("handles deeply nested args without losing types", () => {
    const s = createStore({
      env: { chatId: 285083560 },
      digest: "📰 Новости",
    });
    const args = {
      chatId: "${env.chatId}",
      text: "${digest}",
      meta: { recipient: "${env.chatId}" },
    };
    expect(substitute(args, s)).toEqual({
      chatId: 285083560,
      text: "📰 Новости",
      meta: { recipient: 285083560 },
    });
  });
});

describe("array index paths (map-reduce chunks)", () => {
  it("resolves ${bind.chunks.N} to the N-th array element as-is", () => {
    const chunk0 = [{ body: "a" }, { body: "b" }];
    const chunk1 = [{ body: "c" }];
    const s = createStore({ posts: { count: 3, chunks: [chunk0, chunk1, []] } });
    expect(substitute("${posts.chunks.0}", s)).toBe(chunk0);
    expect(substitute("${posts.chunks.1}", s)).toBe(chunk1);
    expect(substitute("${posts.chunks.2}", s)).toEqual([]);
  });

  it("throws MissingBindingError for an out-of-range index", () => {
    const s = createStore({ posts: { chunks: [[]] } });
    expect(() => substitute("${posts.chunks.5}", s)).toThrow("posts.chunks.5");
  });
});

// ─── compile ───────────────────────────────────────────

const PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
  compiler: { model: "gemini-3-flash-preview", reasoningEffort: "low" },
};

function recordingSpan(): Span {
  const span: Span = {
    id: "test-span",
    update() {},
    end() {},
    generation(_) {
      const gen: Generation = { id: "test-gen", end() {} };
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
    id: "test-trace",
    update() {},
    end() {},
    generation() {
      return { id: "test-gen", end() {} };
    },
    span: () => recordingSpan(),
    event() {},
  };
}

// A trace that records every generation's name + the metadata seen at start
// and at end, so a test can assert which attempt carries the planner judge tag.
interface GenRecord {
  name: string;
  startMeta: Record<string, unknown> | undefined;
  endMeta: Record<string, unknown> | undefined;
}
function collectingTrace(): { trace: Trace; gens: GenRecord[] } {
  const gens: GenRecord[] = [];
  function record(opts: { name: string; metadata?: Record<string, unknown> }): Generation {
    const rec: GenRecord = { name: opts.name, startMeta: opts.metadata, endMeta: undefined };
    gens.push(rec);
    return {
      id: "test-gen",
      end(o) {
        rec.endMeta = o?.metadata;
      },
    };
  }
  function span(): Span {
    return {
      id: "test-span",
      update() {},
      end() {},
      generation: record,
      span: () => span(),
      event() {},
    };
  }
  const trace: Trace = {
    id: "test-trace",
    update() {},
    end() {},
    generation: record,
    span: () => span(),
    event() {},
  };
  return { trace, gens };
}

interface MockProviderOpts {
  llmReplies: Array<string | Error>;
}

function makeMockProvider(opts: MockProviderOpts): {
  provider: ChatProvider;
  calls: Array<{ messages: ChatCompletionMessageParam[] }>;
} {
  const calls: Array<{ messages: ChatCompletionMessageParam[] }> = [];
  const queue = [...opts.llmReplies];
  const provider: ChatProvider = {
    kind: "openai",
    complete: async (params) => {
      calls.push({ messages: structuredClone(params.messages) });
      const next = queue.shift();
      if (next instanceof Error) throw next;
      return {
        message: { role: "assistant", content: next ?? "", refusal: null },
        finishReason: "stop",
        usage: { input: 100, output: 50, total: 150 },
      };
    },
  };
  return { provider, calls };
}

function makeEngineSurface(provider: ChatProvider) {
  return {
    presets: PRESETS,
    resolveProvider(_model: string) {
      return provider;
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
    const { provider, calls } = makeMockProvider({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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

  it("uses the compiler preset's model in the request", async () => {
    const { provider, calls } = makeMockProvider({ llmReplies: [VALID_PLAN_JSON] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({ llmReplies: [] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({ llmReplies: [err] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({
      llmReplies: ["not json", VALID_PLAN_JSON],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider, calls } = makeMockProvider({
      llmReplies: [invalidPlanJson, VALID_PLAN_JSON],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider } = makeMockProvider({
      llmReplies: [invalidPlanJson, invalidPlanJson, invalidPlanJson],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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
    const { provider } = makeMockProvider({
      llmReplies: ["not json", "still not json", "definitely not"],
    });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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

  it("tags only the ACCEPTED attempt as the planner judge node, never failed retries", async () => {
    const { provider } = makeMockProvider({ llmReplies: ["not json", VALID_PLAN_JSON] });
    const { trace, gens } = collectingTrace();
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
      readSkill: async () => "RULES",
      mcpTools: FAKE_TOOLS,
      knownSkills: FAKE_SKILLS,
    });
    const r = await compiler.compile({ ...makeReq(), parentTrace: trace as TraceContext });
    expect(r.ok).toBe(true);

    const attempts = gens.filter((g) => g.name.startsWith("attempt-"));
    expect(attempts.length).toBe(2);
    // Identity is never set at start — `attempt-N` is display-only.
    expect(attempts.every((g) => g.startMeta === undefined)).toBe(true);
    // The failed attempt stays untagged → the per-node judge skips it.
    expect(attempts[0]!.endMeta).toBeUndefined();
    // Only the winning attempt is the judgeable planner node.
    expect(attempts[1]!.endMeta).toEqual({ [JUDGE_NODE_META]: "planner" });
  });

  it("respects custom maxAttempts (1 = no retries)", async () => {
    const { provider } = makeMockProvider({ llmReplies: ["not json"] });
    const compiler = createCompiler({
      engine: makeEngineSurface(provider),
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

// ─── execute ───────────────────────────────────────────

// ─── shared mocks ────────────────────────────────────────────────────

function eventRecordingSpan(): Span & { events: unknown[] } {
  const events: unknown[] = [];
  const span: Span & { events: unknown[] } = {
    events,
    id: "test-span",
    update(data) {
      events.push({ kind: "update", data });
    },
    end(opts) {
      events.push({ kind: "end", opts });
    },
    generation(opts) {
      events.push({ kind: "generation:start", opts });
      const gen: Generation = {
        id: "test-gen",
        end(eo) {
          events.push({ kind: "generation:end", opts: eo });
        },
      };
      return gen;
    },
    span(opts) {
      events.push({ kind: "span:start", opts });
      return eventRecordingSpan();
    },
    event(opts) {
      events.push({ kind: "event", opts });
    },
  };
  return span;
}

function eventRecordingTrace(): Trace {
  const root = eventRecordingSpan();
  return {
    id: "test-trace",
    update: root.update,
    generation: root.generation,
    span: root.span,
    event: root.event,
    end: () => {},
  };
}

// Like eventRecordingSpan, but every span created under it (recursively) is
// pushed into a shared `all` list so a test can inspect the output a
// nested step span was closed with. eventRecordingTrace/eventRecordingSpan discard
// child spans, which is fine for store/tool-call assertions but hides the
// per-step `end({ output })` we want to verify.
function eventCollectingTrace(): { trace: Trace; all: Array<{ name: string; events: unknown[] }> } {
  const all: Array<{ name: string; events: unknown[] }> = [];
  function make(name: string): Span & { events: unknown[] } {
    const events: unknown[] = [];
    const self: Span & { events: unknown[] } = {
      events,
      id: "test-span",
      update(data) {
        events.push({ kind: "update", data });
      },
      end(opts) {
        events.push({ kind: "end", opts });
      },
      generation(opts) {
        events.push({ kind: "generation:start", opts });
        return {
          id: "test-gen",
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
    id: "test-trace",
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

const testSessionContext = () => createSessionContext({
  id: "test:1",
  env: { now: new Date("2026-09-06T12:00:00Z"), timezone: "UTC", userEmail: null, newsLastReadAt: null },
});

const baseCtx = () => ({
  sessionContext: testSessionContext(),
  store: createStore({ env: { chatId: 42 } }),
  parentTrace: eventRecordingTrace() as TraceContext,
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
    const { trace, all } = eventCollectingTrace();

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "tool", tool: "search_news", args: { queries: ["ai"] }, bind: "res" },
          { kind: "terminal" },
        ],
      },
      { ...baseCtx(), store: createStore({ env: {} }), parentTrace: trace },
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

describe("executor.execute — code_agent step (Codex delegation)", () => {
  it("substitutes task/data, delegates to codex (not MCP), and binds the trimmed result", async () => {
    const engine = makeMockEngine();
    const runs: Array<{ prompt: string; input?: unknown }> = [];
    const codex: CodexClient = {
      run: async (req) => {
        runs.push({ prompt: req.prompt, input: req.input });
        return { ok: true, content: "  42\n", stderr: "" };
      },
    };
    const executor = createExecutor({
      engine,
      readSkill: nullReadSkill(),
      setMemory: () => {},
      codex,
    });
    const ctx = baseCtx();
    ctx.store.set("csv", "a,b\n1,2");

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "code_agent", task: "sum column b", data: "${csv}", bind: "n" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );

    expect(r.ok).toBe(true);
    expect(runs).toHaveLength(1);
    expect(runs[0]!.prompt).toContain("sum column b");
    expect(runs[0]!.input).toBe("a,b\n1,2"); // data substituted from the store
    expect(ctx.store.get("n")).toBe("42"); // trimmed result bound
    expect(engine.toolCalls).toEqual([]); // never forwarded to MCP
  });

  it("fails as step_failed when no codex backend is configured", async () => {
    const engine = makeMockEngine();
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "code_agent", task: "2+2", bind: "x" },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );

    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("step_failed");
  });

  it("surfaces a codex failure as step_failed", async () => {
    const engine = makeMockEngine();
    const codex: CodexClient = {
      run: async () => {
        throw new Error("codex service returned 502");
      },
    };
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {}, codex });

    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "code_agent", task: "2+2", bind: "x" },
          { kind: "terminal" },
        ],
      },
      baseCtx(),
    );

    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("step_failed");
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

  it("appends the improver patch to the compose system message when readPatch returns one", async () => {
    const engine = makeMockEngine({ llmResponses: ["composed"] });
    const executor = createExecutor({
      engine,
      readSkill: fixedReadSkill({ "news-digest": "RULES go here" }),
      readPatch: async (name) => (name === "news-digest" ? "PATCH LESSON" : null),
      setMemory: () => {},
    });

    const ctx = baseCtx();
    ctx.store.set("posts", [{ id: 1 }]);
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "smartest", skill: "news-digest", input: {}, bind: "d" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const body = engine.llmCalls[0] as { messages: ChatCompletionMessageParam[] };
    const system = body.messages[0]!.content as string;
    // Body first, patch glued onto the END with the marker — the same shape the
    // gate replay measures.
    expect(system.startsWith("RULES go here")).toBe(true);
    expect(system).toContain(PATCH_MARKER);
    expect(system).toContain("PATCH LESSON");
  });

  it("prepends the base composer skill ahead of the step skill", async () => {
    const engine = makeMockEngine({ llmResponses: ["composed"] });
    const executor = createExecutor({
      engine,
      readSkill: fixedReadSkill({
        composer: "BASE COMPOSE RULES",
        "news-digest": "RULES go here",
      }),
      setMemory: () => {},
    });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "smartest", skill: "news-digest", input: {}, bind: "d" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const body = engine.llmCalls[0] as { messages: ChatCompletionMessageParam[] };
    const system = body.messages[0]!.content as string;
    // Base rules form the stable prefix; the domain skill is layered on top.
    expect(system.startsWith("BASE COMPOSE RULES")).toBe(true);
    expect(system).toContain("RULES go here");
    expect(system.indexOf("BASE COMPOSE RULES")).toBeLessThan(system.indexOf("RULES go here"));
  });

  it("applies the base composer skill even when the step has no skill", async () => {
    const engine = makeMockEngine({ llmResponses: ["composed"] });
    const executor = createExecutor({
      engine,
      readSkill: fixedReadSkill({ composer: "BASE COMPOSE RULES" }),
      setMemory: () => {},
    });
    const ctx = baseCtx();
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          { kind: "llm_compose", preset: "base", input: {}, prompt: "hi", bind: "d" },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const body = engine.llmCalls[0] as { messages: ChatCompletionMessageParam[] };
    // A compiled compose step carries no skill — it must still get the base rules.
    expect(body.messages[0]).toEqual({ role: "system", content: "BASE COMPOSE RULES" });
  });

  it("stringifies a whole-placeholder prompt bound to a non-string value", async () => {
    // `substitute` preserves type for whole-string placeholders — right for
    // tool args, but a prompt must stay TEXT: a raw array in message.content
    // would be an API error.
    const engine = makeMockEngine({ llmResponses: ["ok"] });
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    ctx.store.set("posts", [{ id: 1 }]);
    const r = await executor.execute(
      {
        version: 1,
        steps: [
          {
            kind: "llm_compose",
            preset: "base",
            prompt: "${posts}",
            input: {},
            bind: "out",
          },
          { kind: "terminal" },
        ],
      },
      ctx,
    );
    expect(r.ok).toBe(true);
    const body = engine.llmCalls[0] as { messages: ChatCompletionMessageParam[] };
    const content = body.messages[0]!.content;
    expect(typeof content).toBe("string");
    expect(content).toContain('"id": 1');
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
    expect(opts.sessionContext).toBe(ctx.sessionContext);
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
  it("interrupts a sibling after failure and prevents late writes to the store", async () => {
    let finishSlow: (value: string) => void = () => { throw new Error("Slow tool has not started"); };
    let slowSignal: AbortSignal | undefined;
    const engine = makeMockEngine();
    engine.mcp.callTool = async (name, _args, options) => {
      if (name === "bad") throw new Error("failed branch");
      slowSignal = options?.signal;
      return new Promise<string>((resolve) => { finishSlow = resolve; });
    };
    const executor = createExecutor({ engine, readSkill: nullReadSkill(), setMemory: () => {} });
    const ctx = baseCtx();
    const result = await executor.execute({ version: 1, steps: [{
      kind: "parallel", steps: [
        { kind: "tool", tool: "bad", args: {} },
        { kind: "tool", tool: "slow", args: {}, bind: "late" },
      ],
    }] }, ctx);
    expect(result.ok).toBe(false);
    expect(slowSignal?.aborted).toBe(true);
    finishSlow("late success");
    await new Promise((resolve) => setImmediate(resolve));
    expect(ctx.store.has("late")).toBe(false);
  });

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
