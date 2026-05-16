import type { ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import type { LangfuseTraceClient } from "langfuse";
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
  // `readSkill` at `startSession` time. Missing here is a hard error —
  // the caller decides whether to skip the signal. Engine-level
  // meta-skills (`routing`, `handoff`) come from `EngineOpts.skills` and
  // are added on top unless `includeEngineSkills: false`.
  skills?: string[];
  // Pre-resolved skill contents (name → markdown). Set by the engine
  // after readSkill; Session uses these to (a) compose the actual system
  // message sent to the LLM and (b) expose each skill separately on
  // `trace.metadata.skills` so the Langfuse UI isn't flooded with
  // skill text in every generation's input.
  resolvedSkills?: Record<string, string>;
  // Opt out of engine-level meta-skills (`routing`, `handoff`) for this
  // session. Default true. Sub-agents set this to false so they get only
  // the focused per-task skill set without the always-on parent extras.
  includeEngineSkills?: boolean;
  model?: string;
  reasoningEffort?: ReasoningEffort;
  maxIterations?: number;
  parentId?: string;
  // Optional Langfuse trace metadata. `tags` show up as filter chips in the
  // UI (use for `signal.source` so you can slice by domain); `metadata` is
  // freeform key/value (use for `signal.id`, watermarks, anything you'd
  // grep traces for later). No-op when `engine.langfuse` is null.
  tags?: string[];
  metadata?: Record<string, unknown>;
  // Langfuse session id. Traces sharing a sessionId group together in the
  // UI's "Sessions" view. We use `${signal.source}:${signal.id}` for the
  // primary session AND its recovery — so a crashed run and its
  // user-facing error report end up side-by-side under one session.
  sessionId?: string;
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

// Sub-agent: a fresh child Session spawned mid-loop with a focused skill
// set and no inherited message history. The parent only sees the
// sub-agent's final string result, which keeps its own context lean —
// instead of growing by the size of the sub-agent's full transcript,
// the parent grows by the sub-agent's distilled answer.
const INVOKE_SUB_AGENT_TOOL_NAME = "invoke_sub_agent";
const INVOKE_SUB_AGENT_TOOL: ChatCompletionTool = {
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

interface InvokeSubAgentArgs {
  skills?: string[];
  system_prompt?: string;
  prompt?: string;
  max_iterations?: number;
  reasoning_effort?: ReasoningEffort;
}

// DeepSeek extends OpenAI's assistant message shape with `reasoning_content`
// (the thinking text). Required in the request history whenever the next
// call uses thinking-mode — even if it's empty.
type DeepSeekAssistantHistory = ChatCompletionMessageParam & {
  reasoning_content?: string;
};

// Replace the system message content with a short stub for Langfuse
// logging. The full text is already in `trace.metadata` — repeating it
// in every generation's `input` just buries the actual conversation.
function redactSystemForTrace(
  messages: ChatCompletionMessageParam[],
): ChatCompletionMessageParam[] {
  return messages.map((m) => {
    if (m.role !== "system") return m;
    return { ...m, content: "[system prompt + skills — see trace metadata]" };
  });
}

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
  // Langfuse session id. Stored so spawned sub-agents can inherit it
  // (their child trace lands in the same Langfuse session as the parent).
  readonly sessionId?: string;
  readonly messages: ChatCompletionMessageParam[] = [];
  private readonly engine: Engine;
  private model: string;
  private reasoningEffort: ReasoningEffort;
  private readonly maxIterations: number;
  private readonly trace: LangfuseTraceClient | null;
  private subAgentCounter = 0;
  private closed = false;

  constructor(engine: Engine, opts: SessionOpts) {
    this.engine = engine;
    this.id = opts.id;
    this.parentId = opts.parentId;
    this.sessionId = opts.sessionId;
    this.model = opts.model ?? engine.defaultModel;
    this.reasoningEffort = opts.reasoningEffort ?? "disabled";
    this.maxIterations = opts.maxIterations ?? DEFAULT_MAX_ITERATIONS;

    // Compose the actual system message sent to the LLM: caller's prompt
    // first, then each resolved skill body, joined with `---`. Skills
    // additionally land in `trace.metadata.skills` as a name→content
    // dict so the Langfuse UI can present them structured instead of as
    // one giant blob inside every generation's input.
    const skillsMap = opts.resolvedSkills ?? {};
    const systemParts: string[] = [];
    if (opts.systemPrompt) systemParts.push(opts.systemPrompt);
    for (const content of Object.values(skillsMap)) systemParts.push(content);
    const combinedSystem = systemParts.join("\n\n---\n\n");
    if (combinedSystem.length > 0) {
      this.messages.push({ role: "system", content: combinedSystem });
    }

    // One Langfuse trace per session. Generations + tool spans get
    // attached to it inside `runUntilSettled`. Output + final status are
    // updated when the loop exits (success, error, or maxIterations).
    // `trace.input` is intentionally NOT set here — it's populated in
    // `runUntilSettled` from the first user message so the Langfuse
    // Session-replay shows a clean `user → assistant` exchange instead of
    // the long system prompt. System prompt + skills live in metadata.
    this.trace =
      engine.langfuse?.trace({
        id: this.id,
        name: this.id,
        sessionId: opts.sessionId,
        tags: opts.tags,
        metadata: {
          ...opts.metadata,
          systemPrompt: opts.systemPrompt,
          skills: skillsMap,
          model: this.model,
          reasoningEffort: this.reasoningEffort,
          maxIterations: this.maxIterations,
          parentId: this.parentId,
        },
      }) ?? null;
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

    // Surface the user's prompt on the trace so Langfuse Session-replay
    // renders a real `user → assistant` exchange. Done here (not in the
    // constructor) because the caller pushes the user message AFTER
    // startSession returns.
    const firstUserMessage = this.messages.find((m) => m.role === "user");
    if (firstUserMessage && this.trace) {
      this.trace.update({ input: firstUserMessage.content });
    }

    try {
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
            INVOKE_SUB_AGENT_TOOL,
          ],
          ...(this.reasoningEffort === "disabled"
            ? { thinking: { type: "disabled" as const } }
            : { thinking: { type: "enabled" as const }, reasoning_effort: this.reasoningEffort }),
        };

        // Generation span = one LLM call. Langfuse computes latency from
        // startTime/endTime and stores prompt/completion tokens from `usage`.
        // The system message itself is redacted from the logged input —
        // its full content already lives in `trace.metadata.systemPrompt`
        // and `trace.metadata.skills`, so repeating it in every iter just
        // floods the UI.
        const generation = this.trace?.generation({
          name: `iter-${i}`,
          model: this.model,
          modelParameters: {
            reasoning_effort: this.reasoningEffort,
            thinking: this.reasoningEffort === "disabled" ? "disabled" : "enabled",
          },
          input: redactSystemForTrace(this.messages),
          startTime: new Date(),
        });

        let response;
        try {
          // @ts-expect-error DeepSeek extends ChatCompletionCreateParams with `thinking` + `reasoning_effort`.
          response = await llm.chat.completions.create(body);
        } catch (err) {
          generation?.end({
            output: { error: (err as Error).message },
            level: "ERROR",
            statusMessage: (err as Error).message,
          });
          throw err;
        }

        const choice = response.choices[0]!;
        const { message } = choice;
        this.messages.push(message);

        generation?.end({
          output: message,
          usage: response.usage
            ? {
                input: response.usage.prompt_tokens,
                output: response.usage.completion_tokens,
                total: response.usage.total_tokens,
                unit: "TOKENS",
              }
            : undefined,
        });

        this.engine.log(this.id, `iter ${i} finish=${choice.finish_reason} tool_calls=${message.tool_calls?.length ?? 0}`);

        if (!message.tool_calls?.length) {
          this.trace?.update({ output: message.content ?? "" });
          return message.content ?? "";
        }

        // Dispatch all tool calls in this round in parallel. The model
        // expects parallel-tool-call semantics — if it emits 3 tool_calls,
        // running them sequentially adds N×latency for no reason. Results
        // are still pushed in the original tool_calls order so the
        // message buffer is deterministic regardless of completion order.
        const toolResults = await Promise.all(
          message.tool_calls.map(async (call) => {
            // Span per tool call. We open it BEFORE parsing args so a
            // malformed-JSON throw still leaves a measurable, attributed
            // span in the trace.
            const span = this.trace?.span({
              name: call.function.name,
              input: { raw_arguments: call.function.arguments },
              startTime: new Date(),
            });

            let result: string;
            try {
              const args = JSON.parse(call.function.arguments || "{}") as Record<string, unknown>;
              span?.update({ input: args });
              this.engine.log(this.id, `→ ${call.function.name}(${JSON.stringify(args)})`);

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
              } else if (name === INVOKE_SUB_AGENT_TOOL_NAME) {
                result = await this.applyInvokeSubAgent(args as InvokeSubAgentArgs);
              } else {
                result = await mcp.callTool(name, args);
              }
            } catch (err) {
              span?.end({
                output: { error: (err as Error).message },
                level: "ERROR",
                statusMessage: (err as Error).message,
              });
              throw err;
            }

            span?.end({ output: result });
            this.engine.log(this.id, `← ${call.function.name} ${result.length}b: ${result}`);

            return { call, result };
          }),
        );

        for (const { call, result } of toolResults) {
          this.messages.push({
            role: "tool",
            tool_call_id: call.id,
            content: result,
          });
        }
      }

      throw new Error(`session ${this.id} exceeded maxIterations=${this.maxIterations}`);
    } catch (err) {
      // Trace-level error marker. Individual generation/span errors are
      // already attributed above; this surfaces failed sessions in the
      // Langfuse list view (sort by metadata.error or filter by output.error).
      this.trace?.update({
        output: { error: (err as Error).message },
        metadata: { error: true },
      });
      throw err;
    }
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

  private async applyInvokeSubAgent(args: InvokeSubAgentArgs): Promise<string> {
    if (!Array.isArray(args.skills) || args.skills.length === 0) {
      return `[invoke_sub_agent error] "skills" must be a non-empty string array`;
    }
    if (args.skills.some((s) => s.length === 0)) {
      return `[invoke_sub_agent error] every entry in "skills" must be a non-empty string`;
    }
    if (typeof args.prompt !== "string" || args.prompt.length === 0) {
      return `[invoke_sub_agent error] "prompt" must be a non-empty string`;
    }

    this.subAgentCounter += 1;
    // `__sub` (double underscore) keeps the id ASCII-only and avoids
    // `/` — some Langfuse path/upsert semantics got confused when the
    // trace id contained a slash, leaving the parent's name overwritten
    // by the child's. Plain underscores stay safe.
    const childId = `${this.id}__sub${this.subAgentCounter}`;

    let child: Session;
    try {
      child = await this.engine.startSession({
        id: childId,
        // Sub-agent's system message = optional parent-provided framing
        // + the named skills' content. NO session-context, NO envContext,
        // NO engine meta-skills. This is the entire point — a slim,
        // focused worker with exactly what the parent decided it needs.
        systemPrompt: args.system_prompt,
        skills: args.skills,
        includeEngineSkills: false,
        reasoningEffort: args.reasoning_effort ?? "disabled",
        maxIterations: args.max_iterations ?? 50,
        parentId: this.id,
        // Inherit the parent's Langfuse session so the child trace
        // lands in the same Sessions-view row.
        sessionId: this.sessionId,
        tags: ["sub-agent", ...args.skills],
        metadata: {
          parent_id: this.id,
          parent_session_id: this.sessionId,
          invoked_skills: args.skills,
        },
      });
    } catch (err) {
      return `[invoke_sub_agent error] failed to start: ${(err as Error).message}`;
    }

    child.messages.push({ role: "user", content: args.prompt });

    try {
      return await child.run();
    } catch (err) {
      return `[invoke_sub_agent error] sub-agent crashed: ${(err as Error).message}`;
    } finally {
      this.engine.endSession(childId);
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
