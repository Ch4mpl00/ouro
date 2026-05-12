import type { ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import type { Engine } from "./engine";

// One isolated conversation thread. Owns its own message buffer, system
// prompt, model and iteration budget. Shares the engine's OpenAI client
// and MCP connection — does not create or close them.
//
// The `model` and `reasoningEffort` fields are mutable: the session may
// switch tiers mid-loop via the synthetic `handoff` tool (see below).

export type ReasoningEffort = "disabled" | "high" | "max";

export interface SessionOpts {
  id: string;
  systemPrompt?: string;
  model?: string;
  reasoningEffort?: ReasoningEffort;
  maxIterations?: number;
  parentId?: string;
}

const DEFAULT_MAX_ITERATIONS = 100;

// Synthetic tool intercepted inside the session — never forwarded to MCP.
// Lets the cheap-tier model promote (or demote) the current session's
// reasoning effort and model. The actual "when to use" rules live in
// skills/handoff.md, which the supervisor appends to every system prompt.
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
      const body = {
        model: this.model,
        messages: this.messages,
        tools: [...mcp.tools, HANDOFF_TOOL],
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

        const result =
          call.function.name === HANDOFF_TOOL_NAME
            ? this.applyHandoff(args as HandoffArgs)
            : await mcp.callTool(call.function.name, args);

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
