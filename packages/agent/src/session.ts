import type { ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import type { Engine } from "./engine";
import { setMemory } from "./db/memory";
import { listSkills, readSkill, saveSkill } from "./skills";

// One isolated conversation thread. Owns its own message buffer, system
// prompt, model and iteration budget. Shares the engine's OpenAI client
// and MCP connection — does not create or close them.
//
// The `model` and `reasoningEffort` fields are mutable: the session may
// switch tiers mid-loop via the synthetic `handoff` tool (see below).

export type ReasoningEffort = "disabled" | "high" | "max";

export interface SessionOpts {
  id: string;
  // Pre-assembled context that goes at the top of the system prompt (e.g.
  // the session-context block + the signal's envContext). Skills are
  // resolved separately by the engine and appended after this.
  systemPrompt?: string;
  // Per-session skills — typically the primary domain skill matching the
  // signal source (e.g. `nashdom-bill`). The engine resolves these via
  // `readSkill` at `startSession` time and prepends their content into the
  // system prompt. Missing here is a hard error — the caller decides
  // whether to skip the signal. Engine-level meta-skills (`routing`,
  // `handoff`) come from `EngineOpts.skills` and are added on top unless
  // `includeEngineSkills: false`.
  skills?: string[];
  model?: string;
  reasoningEffort?: ReasoningEffort;
  maxIterations?: number;
  parentId?: string;
}

const DEFAULT_MAX_ITERATIONS = 100;

// Synthetic tool intercepted inside the session — never forwarded to MCP.
// Lets the cheap-tier model promote (or demote) the current session's
// reasoning effort and model. The actual "when to use" rules live in
// skills/handoff.md, which the engine appends to every session as an
// engine-level skill.
const HANDOFF_TOOL_NAME = "handoff";
const HANDOFF_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: HANDOFF_TOOL_NAME,
    description:
      "Switch THIS session's reasoning effort (and optionally the model) starting from the next turn. " +
      "Use this to escalate when the task needs more thinking, or to de-escalate when handing a finished " +
      "result off to a cheap-tier reply. Consult the handoff skill (appended to your system prompt) for " +
      "when to use each tier. Takes effect on the next assistant turn; this turn ends with the tool result.",
    parameters: {
      type: "object",
      properties: {
        reasoning_effort: {
          type: "string",
          enum: ["disabled", "high", "max"],
          description: "Target tier for the next turn.",
        },
        model: {
          type: "string",
          description: "Optional model override (e.g. 'deepseek-reasoner'). Omit to keep current.",
        },
        reason: {
          type: "string",
          description: "Short justification (logged).",
        },
      },
      required: ["reasoning_effort", "reason"],
    },
  },
};

interface HandoffArgs {
  reasoning_effort?: ReasoningEffort;
  model?: string;
  reason?: string;
}

// Second synthetic tool — agent-side writes to the local memory KV
// (`agent.db memory`). Bypasses MCP so the integration server stays
// stateless w.r.t. agent reasoning state. Reads happen via the
// `Current context` block in the system prompt, populated by the
// supervisor at session start — no `get_memory` tool needed.
const SET_MEMORY_TOOL_NAME = "set_memory";
const SET_MEMORY_TOOL: ChatCompletionTool = {
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

interface SetMemoryArgs {
  key?: string;
  value?: string;
}

// Skill tools are agent-side because skills are agent reasoning config,
// not integration state — there's no point round-tripping through MCP to
// reach files the agent process can read directly. `readSkill` resolves
// the live overlay (`skills/<name>.md`) with fallback to the shipped
// default (`skills.default/<name>.md`); `saveSkill` always writes to the
// overlay, leaving defaults untouched as a reset point.
const READ_SKILL_TOOL_NAME = "read_skill";
const READ_SKILL_TOOL: ChatCompletionTool = {
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

const WRITE_SKILL_TOOL_NAME = "write_skill";
const WRITE_SKILL_TOOL: ChatCompletionTool = {
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

const LIST_SKILLS_TOOL_NAME = "list_skills";
const LIST_SKILLS_TOOL: ChatCompletionTool = {
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

interface SkillNameArg {
  name?: string;
}
interface WriteSkillArgs {
  name?: string;
  content?: string;
}

// DeepSeek extends OpenAI's assistant message shape with `reasoning_content`
// (the thinking text). Required in the request history whenever the next
// call uses thinking-mode — even if it's empty.
type DeepSeekAssistantHistory = ChatCompletionMessageParam & {
  reasoning_content?: string;
};

function ensureReasoningContentOnHistory(messages: ChatCompletionMessageParam[]): void {
  for (const m of messages) {
    if (m.role !== "assistant") continue;
    const extended = m as DeepSeekAssistantHistory;
    if (extended.reasoning_content === undefined) {
      extended.reasoning_content = "";
    }
  }
}

export class Session {
  readonly id: string;
  readonly parentId?: string;
  readonly messages: ChatCompletionMessageParam[] = [];
  private readonly engine: Engine;
  private model: string;
  private reasoningEffort: ReasoningEffort;
  private readonly maxIterations: number;
  private closed = false;

  constructor(engine: Engine, opts: SessionOpts) {
    this.engine = engine;
    this.id = opts.id;
    this.parentId = opts.parentId;
    this.model = opts.model ?? engine.defaultModel;
    this.reasoningEffort = opts.reasoningEffort ?? "disabled";
    this.maxIterations = opts.maxIterations ?? DEFAULT_MAX_ITERATIONS;

    if (opts.systemPrompt) {
      this.messages.push({ role: "system", content: opts.systemPrompt });
    }
  }

  async send(userText: string): Promise<string> {
    this.messages.push({ role: "user", content: userText });
    return this.run();
  }

  // Run the agent loop with whatever is currently in `this.messages`.
  // Useful when the caller pre-loaded history (e.g. a Telegram batch) and
  // just wants the LLM to react — no extra user message to push.
  async run(): Promise<string> {
    if (this.closed) throw new Error(`session ${this.id} is closed`);
    return this.runUntilSettled();
  }

  private async runUntilSettled(): Promise<string> {
    const { llm, mcp } = this.engine;

    for (let i = 0; i < this.maxIterations; i++) {
      // DeepSeek's thinking mode requires every prior assistant turn in the
      // history to carry a `reasoning_content` field. Turns produced under
      // `thinking=disabled` lack it, so once we escalate via `handoff`, the
      // very next request 400s with "reasoning_content must be passed back".
      // Stamp an empty string on every assistant message missing the field
      // whenever we're about to send in thinking-enabled mode.
      if (this.reasoningEffort !== "disabled") {
        ensureReasoningContentOnHistory(this.messages);
      }

      const body = {
        model: this.model,
        messages: this.messages,
        tools: [
          ...mcp.tools,
          HANDOFF_TOOL,
          SET_MEMORY_TOOL,
          READ_SKILL_TOOL,
          WRITE_SKILL_TOOL,
          LIST_SKILLS_TOOL,
        ],
        ...(this.reasoningEffort === "disabled"
          ? { thinking: { type: "disabled" as const } }
          : { thinking: { type: "enabled" as const }, reasoning_effort: this.reasoningEffort }),
      };
      // @ts-expect-error DeepSeek extends ChatCompletionCreateParams with `thinking` + `reasoning_effort`.
      const response = await llm.chat.completions.create(body);

      const choice = response.choices[0]!;
      const { message } = choice;
      this.messages.push(message);

      this.engine.log(this.id, `iter ${i} finish=${choice.finish_reason} tool_calls=${message.tool_calls?.length ?? 0}`);

      if (!message.tool_calls?.length) {
        return message.content ?? "";
      }

      for (const call of message.tool_calls) {
        const args = JSON.parse(call.function.arguments || "{}") as Record<string, unknown>;
        this.engine.log(this.id, `→ ${call.function.name}(${JSON.stringify(args)})`);

        let result: string;
        const name = call.function.name;
        if (name === HANDOFF_TOOL_NAME) {
          result = this.applyHandoff(args as HandoffArgs);
        } else if (name === SET_MEMORY_TOOL_NAME) {
          result = this.applySetMemory(args as SetMemoryArgs);
        } else if (name === READ_SKILL_TOOL_NAME) {
          result = await this.applyReadSkill(args as SkillNameArg);
        } else if (name === WRITE_SKILL_TOOL_NAME) {
          result = await this.applyWriteSkill(args as WriteSkillArgs);
        } else if (name === LIST_SKILLS_TOOL_NAME) {
          result = await this.applyListSkills();
        } else {
          result = await mcp.callTool(name, args);
        }

        this.engine.log(this.id, `← ${result.length}b: ${result}`);

        this.messages.push({
          role: "tool",
          tool_call_id: call.id,
          content: result,
        });
      }
    }

    throw new Error(`session ${this.id} exceeded maxIterations=${this.maxIterations}`);
  }

  private async applyReadSkill(args: SkillNameArg): Promise<string> {
    if (typeof args.name !== "string" || args.name.length === 0) {
      return `[read_skill error] name must be a non-empty string`;
    }
    try {
      const content = await readSkill(args.name);
      if (content === null) {
        return JSON.stringify({ name: args.name, found: false, content: null });
      }
      return JSON.stringify({
        name: args.name,
        found: true,
        content,
        sizeBytes: Buffer.byteLength(content, "utf-8"),
      });
    } catch (err) {
      return `[read_skill error] ${(err as Error).message}`;
    }
  }

  private async applyWriteSkill(args: WriteSkillArgs): Promise<string> {
    if (typeof args.name !== "string" || args.name.length === 0) {
      return `[write_skill error] name must be a non-empty string`;
    }
    if (typeof args.content !== "string" || args.content.length === 0) {
      return `[write_skill error] content must be a non-empty string`;
    }
    try {
      const written = await saveSkill(args.name, args.content);
      this.engine.log(this.id, `write_skill ${args.name} (${written.sizeBytes}b → ${written.path})`);
      return JSON.stringify({ ok: true, name: args.name, ...written });
    } catch (err) {
      return `[write_skill error] ${(err as Error).message}`;
    }
  }

  private async applyListSkills(): Promise<string> {
    try {
      const skills = await listSkills();
      return JSON.stringify({ count: skills.length, skills });
    } catch (err) {
      return `[list_skills error] ${(err as Error).message}`;
    }
  }

  private applySetMemory(args: SetMemoryArgs): string {
    if (typeof args.key !== "string" || args.key.length === 0) {
      return `[set_memory error] key must be a non-empty string`;
    }
    if (typeof args.value !== "string") {
      return `[set_memory error] value must be a string (got ${typeof args.value})`;
    }
    setMemory(args.key, args.value);
    this.engine.log(this.id, `set_memory ${args.key} = ${args.value.slice(0, 80)}`);
    return `ok — stored ${args.key}`;
  }

  private applyHandoff(args: HandoffArgs): string {
    const target: ReasoningEffort | undefined = args.reasoning_effort;
    if (target !== "disabled" && target !== "high" && target !== "max") {
      return `[handoff error] reasoning_effort must be one of disabled|high|max, got ${JSON.stringify(target)}`;
    }
    this.reasoningEffort = target;
    if (typeof args.model === "string" && args.model.length > 0) {
      this.model = args.model;
    }
    const reason = typeof args.reason === "string" ? args.reason : "(no reason)";
    this.engine.log(this.id, `handoff → model=${this.model} effort=${this.reasoningEffort} reason=${reason}`);
    return `ok — switched to model=${this.model} effort=${this.reasoningEffort}`;
  }

  close(): void {
    this.closed = true;
  }
}
