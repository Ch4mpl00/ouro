// Named model presets. Each preset bundles a concrete model id together
// with its reasoning_effort. Sessions pick a preset by name instead of
// configuring model + effort separately — the two are coupled in
// practice (we run the cheap chat model with thinking off, the expensive
// thinking model with effort=max) and naming the pair makes call sites
// read as intent ("base reply", "smart digest") rather than
// implementation knobs.
//
// Provider routing (DeepSeek vs OpenAI endpoint) is derived from the
// model name in `engine.resolveProvider` — adding a preset that uses a
// "deepseek-*" name will route through DeepSeek automatically; any
// other prefix routes through OpenAI.

export type ReasoningEffort = "disabled" | "high" | "max";

export interface ModelPreset {
  model: string;
  reasoningEffort: ReasoningEffort;
}

export type PresetName = "base" | "smart" | "smartest";

// Defaults applied at engine startup when env overrides are absent.
// `base`     — non-thinking chat, OpenAI provider. Default for primary
//              Telegram replies, scheduler dispatch, recovery — the
//              bulk of signals.
// `smart`    — DeepSeek with thinking on. Used for sub-agents that do
//              real editorial / parsing work (digests, semantic dedup,
//              PDF amount extraction).
// `smartest` — OpenAI full GPT-5.4. Reserved for the planner role
//              where strict structured-output guarantees and a single
//              high-quality decision matter more than per-call cost
//              (the planner emits one compact plan per signal, then
//              the runtime takes over deterministically).
export const DEFAULT_PRESETS: Record<PresetName, ModelPreset> = {
  base: { model: "gpt-5.4-mini", reasoningEffort: "disabled" },
  smart: { model: "deepseek-v4-pro", reasoningEffort: "max" },
  smartest: { model: "gpt-5.4", reasoningEffort: "max" },
};

export const PRESET_NAMES: readonly PresetName[] = ["base", "smart", "smartest"];

export function isPresetName(value: unknown): value is PresetName {
  return typeof value === "string" && (PRESET_NAMES as readonly string[]).includes(value);
}
