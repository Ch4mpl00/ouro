import { describe, expect, it } from "vitest";
import { createWorkflowSchema, formatWorkflowErrors, parseWorkflow, type Workflow } from "./dsl";

const FAKE_TOOLS = [
  "list_news",
  "send_telegram_message",
  "set_memory",
  "get_telegram_chat_history",
  "search_news",
  "start_typing",
] as const;

const FAKE_SKILLS = [
  "news-digest",
  "tech-digest",
  "news-query",
  "telegram",
] as const;

function makeSchema() {
  return createWorkflowSchema({
    knownTools: FAKE_TOOLS,
    knownSkills: FAKE_SKILLS,
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

describe("example fixture matches schema", () => {
  it("loads and validates news-digest.example.json", async () => {
    const url = new URL("./examples/news-digest.example.json", import.meta.url);
    const fs = await import("node:fs/promises");
    const raw = await fs.readFile(url, "utf8");
    const parsed = JSON.parse(raw);
    const { WorkflowSchema } = makeSchema();
    const r = WorkflowSchema.safeParse(parsed);
    if (!r.success) {
      // Show why on failure so the test output explains itself.
      console.error(formatWorkflowErrors(r.error));
    }
    expect(r.success).toBe(true);
  });
});
