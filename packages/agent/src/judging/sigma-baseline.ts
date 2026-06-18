import { readFileSync } from "node:fs";
import { JUDGE_PROMPT_VERSION } from "./schema";
import type { NoiseAxis } from "./noise";

// Shared reader for the committed judge-noise baseline (judging/noise-baseline.json,
// written by `pnpm judge:noise`). The gate, the improver, and the cron worker all
// read the SAME per-(judge model | prompt version) σ floor from here so they
// can't drift. `found` lets a caller warn when there's no baseline (verdicts then
// read "no-baseline").

const BASELINE_PATH = "packages/agent/src/judging/noise-baseline.json";

interface BaselineEntry {
  axes: Array<{ axis: NoiseAxis; pooledSigma: number }>;
}

export function loadSigmaBaseline(
  judgeModel: string,
  promptVersion: string = JUDGE_PROMPT_VERSION,
): { sigma: Partial<Record<NoiseAxis, number>>; found: boolean } {
  let entries: Record<string, BaselineEntry>;
  try {
    entries = JSON.parse(readFileSync(BASELINE_PATH, "utf-8")) as Record<string, BaselineEntry>;
  } catch {
    return { sigma: {}, found: false };
  }
  const entry = entries[`${judgeModel}|${promptVersion}`];
  if (!entry) return { sigma: {}, found: false };
  const sigma: Partial<Record<NoiseAxis, number>> = {};
  for (const a of entry.axes) sigma[a.axis] = a.pooledSigma;
  return { sigma, found: true };
}
