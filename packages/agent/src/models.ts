// Named model presets. Each preset bundles a concrete model id together
// with its reasoning_effort. Sessions pick a preset by name instead of
// configuring model + effort separately — the two are coupled in
// practice (we run the cheap chat model with thinking off, the expensive
// thinking model with effort=max) and naming the pair makes call sites
// read as intent ("base reply", "smart digest") rather than
// implementation knobs.
//
// Provider routing (DeepSeek vs Gemini vs OpenAI endpoint) is derived from
// the model name in `engine.resolveProvider` — a "deepseek-*" name routes
// through DeepSeek, a "gemini-*" name through Gemini's OpenAI-compatible
// endpoint, any other prefix through OpenAI.

export type ReasoningEffort = "disabled" | "high" | "max";

export interface ModelPreset {
  model: string;
  reasoningEffort: ReasoningEffort;
}

export type PresetName = "base" | "smart" | "smartest" | "compiler";

// Defaults applied at engine startup when env overrides are absent.
// `base`     — non-thinking chat, OpenAI provider. Default for primary
//              Telegram replies, scheduler dispatch, recovery — the
//              bulk of signals.
// `smart`    — DeepSeek with thinking on. Used for sub-agents that do
//              real editorial / parsing work (digests, semantic dedup,
//              PDF amount extraction).
// `smartest` — OpenAI full GPT-5.4. A reserve high-end preset, still
//              selectable by sub-agents/handoff; no longer the compiler's
//              default (see `compiler`).
// `compiler` — Gemini 3 Flash (preview). The model the WORKFLOW COMPILER runs
//              on: Test A showed the Gemini-3 generation rebuilds the
//              non-obvious dedup step 5/5 (vs gpt-5.4-mini 3/5, gemini-2.5-flash
//              0/10), on par with gemini-3.5-flash but ~3x cheaper. The compiler
//              emits ONE structured plan per signal, so structured-output
//              reliability + cost win here. Not in PRESET_NAMES: it's the
//              compiler's own model, not a preset a workflow step or sub-agent
//              picks. (Preview availability wobbles — the Gemini provider
//              retries 429/5xx so a transient blip doesn't fail compilation.)
export const DEFAULT_PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
  compiler: { model: "gemini-3-flash-preview", reasoningEffort: "disabled" },
};

// Presets selectable inside a workflow (llm_compose/llm_agent `preset`) or by
// a sub-agent. `compiler` is intentionally excluded — it's the compiler's own
// model, resolved directly by COMPILER_PRESET, never chosen by a workflow step.
export const PRESET_NAMES: readonly PresetName[] = ["base", "smart", "smartest"];

export function isPresetName(value: unknown): value is PresetName {
  return typeof value === "string" && (PRESET_NAMES as readonly string[]).includes(value);
}
