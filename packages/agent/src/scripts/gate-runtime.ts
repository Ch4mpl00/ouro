import OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { DEEPSEEK_BASE_URL, GEMINI_BASE_URL, retryOnTransient } from "../providers";
import type { GateNodeTarget } from "../judging/gate";
import type { NodeMaterial } from "../judging/materials";
import type { ChatMessage } from "../judging/patch";
import type { Observation } from "../trace-model";

// Script-side runtime for the gate / improver: re-running the generator under the
// recorded model, and lifting a NodeMaterial + its observation into a replayable
// GateNodeTarget. Shared by judge-gate.ts and improve.ts so the two can't drift.

// Lazy, per-provider — built on first use (after the caller has loaded env) and
// only for the provider actually needed, so a deepseek-only run never requires a
// GEMINI/OPENAI key just to import this module.
let openaiClient: OpenAI | undefined;
let deepseekClient: OpenAI | undefined;
let geminiClient: OpenAI | undefined;

export function clientFor(model: string): OpenAI {
  if (model.startsWith("deepseek")) {
    return (deepseekClient ??= new OpenAI({ apiKey: process.env.DEEPSEEK_API_KEY, baseURL: DEEPSEEK_BASE_URL }));
  }
  if (model.startsWith("gemini")) {
    return (geminiClient ??= new OpenAI({ apiKey: process.env.GEMINI_API_KEY, baseURL: GEMINI_BASE_URL }));
  }
  return (openaiClient ??= new OpenAI({ apiKey: process.env.OPENAI_API_KEY }));
}

function toParam(m: ChatMessage): ChatCompletionMessageParam {
  const content = typeof m.content === "string" ? m.content : JSON.stringify(m.content);
  if (m.role === "system") return { role: "system", content };
  if (m.role === "assistant") return { role: "assistant", content };
  return { role: "user", content };
}

// Re-run the generator under the recorded model. jsonMode mirrors production
// (planner emits JSON, composer prose).
export async function runModel(messages: ChatMessage[], model: string, jsonMode: boolean): Promise<string> {
  const res = await retryOnTransient(
    () =>
      clientFor(model).chat.completions.create({
        model,
        messages: messages.map(toParam),
        ...(jsonMode ? { response_format: { type: "json_object" } } : {}),
      }),
    { maxRetries: 5, baseDelayMs: 3000 },
  );
  return res.choices[0]?.message.content ?? "";
}

function recordedMessages(input: unknown): ChatMessage[] {
  if (!Array.isArray(input)) return [];
  return input.filter((m): m is ChatMessage => typeof m === "object" && m !== null && "role" in m);
}

// Lift a judged node + its recorded observation into a replayable target.
// Returns null when no generator model can be resolved (can't replay).
export function buildGateTarget(node: NodeMaterial, obs: Observation | undefined): GateNodeTarget | null {
  const model = obs?.model ?? process.env.AGENT_MODEL;
  if (!model) return null;
  return {
    observationId: node.observationId,
    kind: node.kind,
    skill: node.skill,
    label: node.label,
    contract: node.contract,
    inputText: node.inputText,
    originalOutput: node.outputText,
    model,
    recordedInput: recordedMessages(obs?.input),
    jsonMode: node.kind === "planner",
  };
}
