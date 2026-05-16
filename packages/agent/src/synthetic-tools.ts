import type { ChatCompletionTool } from "openai/resources/chat/completions";
import type { ReasoningEffort, Session } from "./session";

// Agent-side synthetic tools — intercepted inside the Session loop and
// never forwarded to the MCP server. Each tool is declared as the
// (OpenAI tool definition + Session-method handler) pair below, then
// listed in SYNTHETIC_TOOLS at the bottom of the file. The Session
// loop builds its per-call `tools` array by filtering through
// `visibleTo` and dispatches incoming tool calls by name lookup in
// SYNTHETIC_TOOLS_BY_NAME — adding a new tool is one new entry here,
// no changes to session.ts beyond declaring the handler method.

// ─── handoff ─────────────────────────────────────────────────────────
// Lets the cheap-tier model promote (or demote) the current session's
// reasoning effort and model. The actual "when to use" rules live in
// skills/handoff.md, which the engine appends to every session as an
// engine-level skill.
export const HANDOFF_TOOL_NAME = "handoff";
export const HANDOFF_TOOL: ChatCompletionTool = {
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

export interface HandoffArgs {
  reasoning_effort?: ReasoningEffort;
  model?: string;
  reason?: string;
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

export interface SetMemoryArgs {
  key?: string;
  value?: string;
}

// ─── read_skill / write_skill / list_skills ──────────────────────────
// Skills are agent reasoning config, not integration state — there's no
// point round-tripping through MCP to reach files the agent process can
// read directly. `readSkill` resolves the live overlay
// (`skills/<name>.md`) with fallback to the shipped default
// (`skills.default/<name>.md`); `saveSkill` always writes to the
// overlay, leaving defaults untouched as a reset point.
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

export interface SkillNameArg {
  name?: string;
}
export interface WriteSkillArgs {
  name?: string;
  content?: string;
}

// ─── invoke_sub_agent ────────────────────────────────────────────────
// A fresh child Session spawned mid-loop with a focused skill set and no
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
      "sub-agent loads ONLY the skills you name (no routing, no handoff, " +
      "no parent history), has access to every MCP tool, runs to " +
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
        reasoning_effort: {
          type: "string",
          enum: ["disabled", "high", "max"],
          description: "Optional reasoning effort. Default `disabled`.",
        },
      },
      required: ["skills", "prompt"],
    },
  },
};

export interface InvokeSubAgentArgs {
  skills?: string[];
  system_prompt?: string;
  prompt?: string;
  max_iterations?: number;
  reasoning_effort?: ReasoningEffort;
}

// ─── registry ────────────────────────────────────────────────────────
export interface SyntheticTool {
  def: ChatCompletionTool;
  visibleTo?: (session: Session) => boolean;
  handle: (
    session: Session,
    args: Record<string, unknown>,
  ) => Promise<string> | string;
}

export const SYNTHETIC_TOOLS: SyntheticTool[] = [
  {
    def: HANDOFF_TOOL,
    handle: (s, args) => s.applyHandoff(args as HandoffArgs),
  },
  {
    def: SET_MEMORY_TOOL,
    handle: (s, args) => s.applySetMemory(args as SetMemoryArgs),
  },
  {
    def: READ_SKILL_TOOL,
    handle: (s, args) => s.applyReadSkill(args as SkillNameArg),
  },
  {
    def: WRITE_SKILL_TOOL,
    handle: (s, args) => s.applyWriteSkill(args as WriteSkillArgs),
  },
  {
    def: LIST_SKILLS_TOOL,
    handle: (s) => s.applyListSkills(),
  },
  {
    def: INVOKE_SUB_AGENT_TOOL,
    // Top-level sessions only. Sub-agents are focused workers; if they
    // can't finish without further delegation, the parent picked the
    // wrong skill — not a job for recursion.
    visibleTo: (s) => s.parentId === undefined,
    handle: (s, args) => s.applyInvokeSubAgent(args as InvokeSubAgentArgs),
  },
];

export const SYNTHETIC_TOOLS_BY_NAME = new Map(
  SYNTHETIC_TOOLS.map((t) => [t.def.function.name, t] as const),
);
