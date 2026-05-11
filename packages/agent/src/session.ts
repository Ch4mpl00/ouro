import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { Engine } from "./engine";

// One isolated conversation thread. Owns its own message buffer, system
// prompt, model and iteration budget. Shares the engine's OpenAI client
// and MCP connection — does not create or close them.

export interface SessionOpts {
  id: string;
  systemPrompt?: string;
  model?: string;
  maxIterations?: number;
  parentId?: string;
}

const DEFAULT_MAX_ITERATIONS = 100;

export class Session {
  readonly id: string;
  readonly parentId?: string;
  readonly messages: ChatCompletionMessageParam[] = [];
  private readonly engine: Engine;
  private readonly model: string;
  private readonly maxIterations: number;
  private closed = false;

  constructor(engine: Engine, opts: SessionOpts) {
    this.engine = engine;
    this.id = opts.id;
    this.parentId = opts.parentId;
    this.model = opts.model ?? engine.defaultModel;
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
      const response = await llm.chat.completions.create({
        model: this.model,
        messages: this.messages,
        tools: mcp.tools,
      });

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

        const result = await mcp.callTool(call.function.name, args);
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

  close(): void {
    this.closed = true;
  }
}
