import { describe, expect, it } from "vitest";
import type OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { createOpenAiProvider } from "./openai";
import { createDeepseekProvider } from "./deepseek";
import { createGeminiProvider } from "./gemini";

// Fake OpenAI-shaped client: captures the request body and returns a canned
// completion. Casting an incomplete stand-in to the full SDK type is the
// established test pattern here — the provider only ever touches
// chat.completions.create.
function fakeClient(usage: Record<string, unknown>): {
  client: OpenAI;
  bodies: Array<Record<string, unknown>>;
} {
  const bodies: Array<Record<string, unknown>> = [];
  const client = {
    chat: {
      completions: {
        create: async (body: Record<string, unknown>) => {
          bodies.push(body);
          return {
            choices: [
              { message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
            ],
            usage,
          };
        },
      },
    },
  } as unknown as OpenAI;
  return { client, bodies };
}

const OPENAI_USAGE = {
  prompt_tokens: 100,
  completion_tokens: 20,
  total_tokens: 120,
  prompt_tokens_details: { cached_tokens: 64 },
};

const DEEPSEEK_USAGE = {
  prompt_tokens: 100,
  completion_tokens: 20,
  total_tokens: 120,
  prompt_cache_hit_tokens: 48,
  prompt_cache_miss_tokens: 52,
};

describe("openai provider", () => {
  it("sends no thinking / no reasoning_effort, omits empty tools", async () => {
    const { client, bodies } = fakeClient(OPENAI_USAGE);
    const provider = createOpenAiProvider(client);
    await provider.complete({
      model: "gpt-5.4",
      messages: [{ role: "user", content: "x" }],
      reasoningEffort: "max",
    });
    expect(bodies[0]).toEqual({
      model: "gpt-5.4",
      messages: [{ role: "user", content: "x" }],
    });
    expect(bodies[0]!.thinking).toBeUndefined();
    expect(bodies[0]!.reasoning_effort).toBeUndefined();
  });

  it("passes tools and response_format through when provided", async () => {
    const { client, bodies } = fakeClient(OPENAI_USAGE);
    const provider = createOpenAiProvider(client);
    const tools = [
      { type: "function" as const, function: { name: "t", parameters: {} } },
    ];
    await provider.complete({
      model: "gpt-5.4",
      messages: [],
      reasoningEffort: "disabled",
      tools,
      responseFormat: { type: "json_object" },
    });
    expect(bodies[0]!.tools).toEqual(tools);
    expect(bodies[0]!.response_format).toEqual({ type: "json_object" });
  });

  it("normalizes usage incl. cached from prompt_tokens_details", async () => {
    const { client } = fakeClient(OPENAI_USAGE);
    const provider = createOpenAiProvider(client);
    const r = await provider.complete({ model: "gpt-5.4", messages: [], reasoningEffort: "disabled" });
    expect(r.usage).toEqual({ input: 100, output: 20, total: 120, cached: 64 });
    expect(r.finishReason).toBe("stop");
    expect(r.message.content).toBe("ok");
  });
});

describe("deepseek provider", () => {
  it("disabled effort: thinking:disabled, no reasoning_effort", async () => {
    const { client, bodies } = fakeClient(DEEPSEEK_USAGE);
    const provider = createDeepseekProvider(client);
    await provider.complete({ model: "deepseek-v4-pro", messages: [], reasoningEffort: "disabled" });
    expect(bodies[0]!.thinking).toEqual({ type: "disabled" });
    expect(bodies[0]!.reasoning_effort).toBeUndefined();
  });

  it("enabled effort: thinking:enabled + reasoning_effort", async () => {
    const { client, bodies } = fakeClient(DEEPSEEK_USAGE);
    const provider = createDeepseekProvider(client);
    await provider.complete({ model: "deepseek-v4-pro", messages: [], reasoningEffort: "max" });
    expect(bodies[0]!.thinking).toEqual({ type: "enabled" });
    expect(bodies[0]!.reasoning_effort).toBe("max");
  });

  it("stamps reasoning_content on prior assistant turns when thinking-enabled", async () => {
    const { client } = fakeClient(DEEPSEEK_USAGE);
    const provider = createDeepseekProvider(client);
    const messages: ChatCompletionMessageParam[] = [
      { role: "user", content: "hi" },
      { role: "assistant", content: "prior" },
    ];
    await provider.complete({ model: "deepseek-v4-pro", messages, reasoningEffort: "max" });
    const assistant = messages[1] as { reasoning_content?: string };
    expect(assistant.reasoning_content).toBe("");
  });

  it("does NOT stamp reasoning_content when thinking-disabled", async () => {
    const { client } = fakeClient(DEEPSEEK_USAGE);
    const provider = createDeepseekProvider(client);
    const messages: ChatCompletionMessageParam[] = [
      { role: "assistant", content: "prior" },
    ];
    await provider.complete({ model: "deepseek-v4-pro", messages, reasoningEffort: "disabled" });
    const assistant = messages[0] as { reasoning_content?: string };
    expect(assistant.reasoning_content).toBeUndefined();
  });

  it("normalizes usage incl. cached from prompt_cache_hit_tokens", async () => {
    const { client } = fakeClient(DEEPSEEK_USAGE);
    const provider = createDeepseekProvider(client);
    const r = await provider.complete({ model: "deepseek-v4-pro", messages: [], reasoningEffort: "max" });
    expect(r.usage).toEqual({ input: 100, output: 20, total: 120, cached: 48 });
  });
});

describe("gemini provider", () => {
  it("disabled effort: omits reasoning_effort (dynamic thinking budget)", async () => {
    const { client, bodies } = fakeClient(OPENAI_USAGE);
    const provider = createGeminiProvider(client);
    await provider.complete({
      model: "gemini-3.5-flash",
      messages: [{ role: "user", content: "x" }],
      reasoningEffort: "disabled",
    });
    expect(bodies[0]).toEqual({
      model: "gemini-3.5-flash",
      messages: [{ role: "user", content: "x" }],
    });
    expect(bodies[0]!.reasoning_effort).toBeUndefined();
    expect(bodies[0]!.thinking).toBeUndefined();
  });

  it("non-disabled effort: maps to reasoning_effort 'high' (no 'max' in Gemini's enum)", async () => {
    const { client, bodies } = fakeClient(OPENAI_USAGE);
    const provider = createGeminiProvider(client);
    await provider.complete({ model: "gemini-3.5-flash", messages: [], reasoningEffort: "max" });
    expect(bodies[0]!.reasoning_effort).toBe("high");
  });

  it("passes tools and response_format through, normalizes usage like OpenAI", async () => {
    const { client, bodies } = fakeClient(OPENAI_USAGE);
    const provider = createGeminiProvider(client);
    const tools = [{ type: "function" as const, function: { name: "t", parameters: {} } }];
    const r = await provider.complete({
      model: "gemini-3.5-flash",
      messages: [],
      reasoningEffort: "disabled",
      tools,
      responseFormat: { type: "json_object" },
    });
    expect(bodies[0]!.tools).toEqual(tools);
    expect(bodies[0]!.response_format).toEqual({ type: "json_object" });
    expect(r.usage).toEqual({ input: 100, output: 20, total: 120, cached: 64 });
  });
});
