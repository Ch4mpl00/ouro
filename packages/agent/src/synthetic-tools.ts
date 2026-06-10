import type { ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import { z } from "zod";
import { isPresetName, PRESET_NAMES } from "./models";
import type { MemoryStore } from "./db/memory";
import type { SkillStore } from "./skills";
import type { AgentLoopOpts } from "./agent-loop";
import type { TraceContext } from "./tracing";

// Agent-side synthetic tools — intercepted inside the AgentLoop and never
// forwarded to the MCP server. Each registry entry is SELF-CONTAINED: the
// OpenAI tool definition, the zod schema for the (untrusted, LLM-provided)
// args, and the handler — adding a new tool really is one new entry here,
// no agent-loop changes. The dispatcher in agent-loop.ts looks the entry up
// by name and calls `run`, which validates args against the schema before
// invoking the typed handler.
//
// Handlers receive a narrow SyntheticToolContext, not the whole loop —
// exactly the dependencies they use, so the contract is the signature.
//
// These live for the AgentLoop — i.e. the agentic fallback path and
// `llm_agent` workflow steps. The default workflow path does NOT load this
// registry: its tool / llm_compose steps call MCP and the LLM directly.
// The one exception is `set_memory`, which a workflow `tool` step also
// needs (watermark writes) — the executor dispatches it to the same
// agent.db writer without going through an AgentLoop (see
// workflow/execute.ts execSetMemory).

// What a synthetic-tool handler may touch. Provided by the AgentLoop at
// dispatch time; every field is something at least one tool genuinely uses.
export interface SyntheticToolContext {
  loopId: string;
  // Undefined for top-level loops; set for sub-agents. `invoke_sub_agent`
  // uses it to stay parent-only (no recursive delegation).
  parentId?: string;
  // Trace-grouping session id, inherited by spawned sub-agents.
  sessionId?: string;
  log(...parts: unknown[]): void;
  skillStore: SkillStore;
  memory: MemoryStore;
  // Spawn/end a child loop (invoke_sub_agent). Narrow structural handle —
  // the handler only pushes the prompt and runs to completion.
  startAgentLoop(opts: AgentLoopOpts): Promise<{
    messages: ChatCompletionMessageParam[];
    run(): Promise<string>;
  }>;
  endAgentLoop(id: string): void;
  // Allocates the next `<loopId>__subN` child id (parent owns the counter).
  allocSubAgentId(): string;
}

export interface SyntheticTool {
  def: ChatCompletionTool;
  visibleTo?: (ctx: SyntheticToolContext) => boolean;
  // Validates raw args and runs the handler. `span` is the trace span the
  // dispatch loop opened for this call; most tools ignore it —
  // `invoke_sub_agent` reuses it as the child's trace scope so the
  // sub-agent's iters render nested inside the parent's span.
  run(
    ctx: SyntheticToolContext,
    rawArgs: Record<string, unknown>,
    span: TraceContext,
  ): Promise<string> | string;
}

// Flatten zod issues into one `path: message; …` line for the
// `[<tool> error] …` result fed back to the model.
function zodIssueText(error: z.ZodError): string {
  return error.issues
    .map((i) => `${i.path.join(".") || "args"}: ${i.message}`)
    .join("; ");
}

// Entry builder: closes over the schema so `run` validates before the
// typed handler executes — handlers never see unvalidated args, and the
// registry needs no casts.
function defineTool<A>(opts: {
  def: ChatCompletionTool;
  schema: z.ZodType<A>;
  visibleTo?: (ctx: SyntheticToolContext) => boolean;
  handle: (
    ctx: SyntheticToolContext,
    args: A,
    span: TraceContext,
  ) => Promise<string> | string;
}): SyntheticTool {
  return {
    def: opts.def,
    visibleTo: opts.visibleTo,
    run(ctx, rawArgs, span) {
      const parsed = opts.schema.safeParse(rawArgs);
      if (!parsed.success) {
        return `[${opts.def.function.name} error] ${zodIssueText(parsed.error)}`;
      }
      return opts.handle(ctx, parsed.data, span);
    },
  };
}

// ─── set_memory ──────────────────────────────────────────────────────
// Agent-side writes to the local memory KV (`agent.db memory`). Bypasses
// MCP so the integration server stays stateless w.r.t. agent reasoning
// state. Reads happen via the `Current context` block in the system
// prompt, populated by the supervisor at session start.
export const SET_MEMORY_TOOL_NAME = "set_memory";
export const SET_MEMORY_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: SET_MEMORY_TOOL_NAME,
    description:
      "Persist a small piece of agent-side state to the local memory KV. " +
      "Use for watermarks, last-seen markers, counters, or any note the " +
      "agent wants to recall in a future session. Well-known keys (e.g. " +
      "`news_digest.last_read_at`) are auto-injected into the `Current " +
      "context` block of future system prompts. Values are stored as " +
      "strings — JSON-stringify complex payloads yourself.",
    parameters: {
      type: "object",
      properties: {
        key: {
          type: "string",
          description: "Memory key, e.g. `news_digest.last_read_at`.",
        },
        value: {
          type: "string",
          description: "Value to store. Use ISO timestamps for time markers.",
        },
      },
      required: ["key", "value"],
    },
  },
};

// Shared with the workflow executor's set_memory step (see
// workflow/execute.ts) so both paths validate identically.
export const SetMemoryArgsSchema = z.object({
  key: z.string().min(1),
  value: z.string(),
});
export type SetMemoryArgs = z.infer<typeof SetMemoryArgsSchema>;

// ─── read_skill / write_skill / list_skills ──────────────────────────
// Skills are agent reasoning config, not integration state — there's no
// point round-tripping through MCP to reach files the agent process can
// read directly.
export const READ_SKILL_TOOL_NAME = "read_skill";
export const READ_SKILL_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: READ_SKILL_TOOL_NAME,
    description:
      "Return the raw text of a skill (`skills/<name>.md`). Reads the live " +
      "overlay if present, otherwise falls back to the shipped default " +
      "(`skills.default/<name>.md`). Use this to consult another skill's " +
      "rules mid-session (e.g. the telegram handler reading `news-digest` " +
      "before composing a digest).",
    parameters: {
      type: "object",
      properties: {
        name: {
          type: "string",
          description: "Skill name without .md extension (matches signal source).",
        },
      },
      required: ["name"],
    },
  },
};

export const WRITE_SKILL_TOOL_NAME = "write_skill";
export const WRITE_SKILL_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: WRITE_SKILL_TOOL_NAME,
    description:
      "Overwrite a skill with new content. Always writes to the live " +
      "overlay — the shipped default stays intact, so deleting the live " +
      "file at any time restores the original. Used by the `dreaming` " +
      "skill to revise instructions based on observed patterns. Pass the " +
      "complete new body; the file is replaced, not patched.",
    parameters: {
      type: "object",
      properties: {
        name: {
          type: "string",
          description: "Skill name without .md extension.",
        },
        content: {
          type: "string",
          description: "Full new content of the skill file.",
        },
      },
      required: ["name", "content"],
    },
  },
};

export const LIST_SKILLS_TOOL_NAME = "list_skills";
export const LIST_SKILLS_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: LIST_SKILLS_TOOL_NAME,
    description:
      "List all available skills (union of live overlay + shipped defaults). " +
      "Each entry includes `source: 'live'|'default'` showing which layer " +
      "is active for that name. Useful for the `dreaming` skill to survey " +
      "what's edit-able before deciding what to revise.",
    parameters: { type: "object", properties: {} },
  },
};

export const SkillNameArgSchema = z.object({
  name: z.string().min(1),
});
export type SkillNameArg = z.infer<typeof SkillNameArgSchema>;

export const WriteSkillArgsSchema = z.object({
  name: z.string().min(1),
  content: z.string().min(1),
});
export type WriteSkillArgs = z.infer<typeof WriteSkillArgsSchema>;

// ─── invoke_sub_agent ────────────────────────────────────────────────
// A fresh child loop spawned mid-session with a focused skill set and no
// inherited message history. The parent only sees the sub-agent's final
// string result, which keeps its own context lean — instead of growing
// by the size of the sub-agent's full transcript, the parent grows by
// the sub-agent's distilled answer.
export const INVOKE_SUB_AGENT_TOOL_NAME = "invoke_sub_agent";
export const INVOKE_SUB_AGENT_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: INVOKE_SUB_AGENT_TOOL_NAME,
    description:
      "Delegate a focused task to a sub-agent with a clean context. The " +
      "sub-agent loads ONLY the skills you name (no routing, no parent " +
      "history), has access to every MCP tool, runs to " +
      "completion, and returns its final text result here as the tool " +
      "output. Use this whenever the user's request maps to a dedicated " +
      "domain skill — e.g. `news-digest`, `tech-digest`, `channel-digest`, " +
      "`nashdom-bill`. DO NOT also `read_skill` that skill yourself: " +
      "delegation replaces local loading, keeping your own context lean. " +
      "Side effects performed inside the sub-agent (Telegram messages, " +
      "memory writes, etc.) take effect immediately — if the sub-agent's " +
      "skill sends the user-facing reply itself, you don't need to " +
      "forward its output again.",
    parameters: {
      type: "object",
      properties: {
        skills: {
          type: "array",
          items: { type: "string" },
          description:
            "Skill names to load in the sub-agent (e.g. [\"news-digest\"]). " +
            "At least one. The sub-agent's system message is composed " +
            "from these alone — no engine meta-skills.",
        },
        system_prompt: {
          type: "string",
          description:
            "Optional goal / framing / constraints the PARENT wants the " +
            "sub-agent to follow on top of its skill. Goes into the " +
            "sub-agent's system message ahead of the skill content. Use " +
            "this to set scope (\"only fetch X, not Y\"), output format " +
            "(\"return JSON\", \"reply in Russian\"), delivery target " +
            "(\"send to chat=<id> thread=<n>\"), or any other context the " +
            "skill itself doesn't know about. Skip when the skill is " +
            "self-sufficient.",
        },
        prompt: {
          type: "string",
          description:
            "Task / user-facing request to hand to the sub-agent — goes in " +
            "as a user message and shows up as the sub-agent's trace " +
            "input. Use the user's verbatim wording when possible. For " +
            "self-initiated tasks (no user message) put the trigger " +
            "description here.",
        },
        max_iterations: {
          type: "number",
          description: "Optional iteration budget for the sub-agent. Default 50.",
        },
        preset: {
          type: "string",
          enum: [...PRESET_NAMES],
          description:
            "Model preset for the sub-agent. `base` — cheap chat model, " +
            "no thinking (default; use for simple one-offs and lookups). " +
            "`smart` — DeepSeek with thinking on (use for real editorial / " +
            "parsing work: digests, semantic dedup, PDF amount extraction). " +
            "Default `base`.",
        },
      },
      required: ["skills", "prompt"],
    },
  },
};

export const InvokeSubAgentArgsSchema = z.object({
  skills: z.array(z.string().min(1)).min(1),
  prompt: z.string().min(1),
  system_prompt: z.string().optional(),
  max_iterations: z.number().int().positive().optional(),
  // Inline literals (zod widens a readonly PRESET_NAMES to `string`); a
  // typo here surfaces as a type error where preset feeds startAgentLoop.
  preset: z.enum(["base", "smart", "smartest"]).optional(),
});
export type InvokeSubAgentArgs = z.infer<typeof InvokeSubAgentArgsSchema>;

// ─── registry ────────────────────────────────────────────────────────

export const SYNTHETIC_TOOLS: SyntheticTool[] = [
  defineTool({
    def: SET_MEMORY_TOOL,
    schema: SetMemoryArgsSchema,
    handle: (ctx, { key, value }) => {
      ctx.memory.set(key, value);
      ctx.log(`set_memory ${key} = ${value.slice(0, 80)}`);
      return `ok — stored ${key}`;
    },
  }),

  defineTool({
    def: READ_SKILL_TOOL,
    schema: SkillNameArgSchema,
    handle: async (ctx, { name }) => {
      try {
        const skill = await ctx.skillStore.readSkill(name);
        if (skill === null) {
          return JSON.stringify({ name, found: false, content: null });
        }
        return JSON.stringify({
          name,
          found: true,
          content: skill.body,
          tools: skill.tools,
          source: skill.source,
          sizeBytes: Buffer.byteLength(skill.body, "utf-8"),
        });
      } catch (err) {
        return `[read_skill error] ${(err as Error).message}`;
      }
    },
  }),

  defineTool({
    def: WRITE_SKILL_TOOL,
    schema: WriteSkillArgsSchema,
    handle: async (ctx, { name, content }) => {
      try {
        const written = await ctx.skillStore.saveSkill(name, content);
        ctx.log(`write_skill ${name} (${written.sizeBytes}b → ${written.path})`);
        return JSON.stringify({ ok: true, name, ...written });
      } catch (err) {
        return `[write_skill error] ${(err as Error).message}`;
      }
    },
  }),

  defineTool({
    def: LIST_SKILLS_TOOL,
    schema: z.object({}),
    handle: async (ctx) => {
      try {
        const skills = await ctx.skillStore.listSkills();
        return JSON.stringify({ count: skills.length, skills });
      } catch (err) {
        return `[list_skills error] ${(err as Error).message}`;
      }
    },
  }),

  defineTool({
    def: INVOKE_SUB_AGENT_TOOL,
    schema: InvokeSubAgentArgsSchema,
    // Top-level sessions only. Sub-agents are focused workers; if they
    // can't finish without further delegation, the parent picked the
    // wrong skill — not a job for recursion.
    visibleTo: (ctx) => ctx.parentId === undefined,
    handle: async (ctx, args, span) => {
      const { skills, prompt, system_prompt, max_iterations, preset } = args;
      const childId = ctx.allocSubAgentId();

      let child;
      try {
        child = await ctx.startAgentLoop({
          id: childId,
          // Sub-agent's system message = optional parent-provided framing
          // + the named skills' content. NO session-context, NO envContext,
          // NO engine meta-skills. This is the entire point — a slim,
          // focused worker with exactly what the parent decided it needs.
          systemPrompt: system_prompt,
          skills,
          includeEngineSkills: false,
          // Narrow via the guard rather than leaning on zod's enum-literal
          // inference; robust for any tooling, defaults on absent/invalid.
          preset: isPresetName(preset) ? preset : "base",
          maxIterations: max_iterations ?? 50,
          parentId: ctx.loopId,
          sessionId: ctx.sessionId,
          // Nest the sub-agent inside the parent's `invoke_sub_agent` span.
          // All iter generations + tool spans the child opens land here, so
          // the parent's trace view shows the whole sub-session inline.
          traceScope: span,
        });
      } catch (err) {
        return `[invoke_sub_agent error] failed to start: ${(err as Error).message}`;
      }

      child.messages.push({ role: "user", content: prompt });

      try {
        return await child.run();
      } catch (err) {
        return `[invoke_sub_agent error] sub-agent crashed: ${(err as Error).message}`;
      } finally {
        ctx.endAgentLoop(childId);
      }
    },
  }),
];

export const SYNTHETIC_TOOLS_BY_NAME = new Map(
  SYNTHETIC_TOOLS.map((t) => [t.def.function.name, t] as const),
);
