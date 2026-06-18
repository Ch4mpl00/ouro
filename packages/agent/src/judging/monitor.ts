// Live-trend monitor + patch bookkeeping (Phase 3, п3), all pure. The gate can
// be fooled (it optimizes the judge's score on a frozen cluster); PROD is the
// ground truth. After a patch ships, the cron worker gathers the target axis's
// scores on traces that ran AFTER the ship and asks `decideRevert` whether the
// live trend held above the pre-ship baseline. If it confidently fell, the
// worker removes the lesson. The IO (reading post-ship judgements, deleting the
// file) lives in the worker; this module just does the stats + string surgery.

export function mean(xs: number[]): number | null {
  return xs.length === 0 ? null : xs.reduce((a, b) => a + b, 0) / xs.length;
}

export interface Baseline {
  mean: number;
  n: number;
}

export type RevertDecision = "insufficient" | "keep" | "revert";

export interface RevertVerdict {
  decision: RevertDecision;
  postMean: number | null;
  postN: number;
  baselineMean: number;
  // The two-sample noise band the drop had to clear to count as a real regression.
  margin: number | null;
}

// Decide whether a shipped patch's live trend regressed below its pre-ship
// baseline. Conservative by construction: we only revert on a CONFIDENT drop
// (baseline − post beyond k·σ·√(1/postN + 1/baselineN), the two-sample noise
// band), never on a flat trend — a patch that passed the gate and merely fails
// to help in prod is harmless and stays. `minN` guards against reverting on a
// handful of post-ship traces; below it we keep watching ("insufficient").
export function decideRevert(
  baseline: Baseline,
  postScores: number[],
  sigma: number | null,
  k: number,
  minN: number,
): RevertVerdict {
  const postN = postScores.length;
  const base: RevertVerdict = {
    decision: "insufficient",
    postMean: mean(postScores),
    postN,
    baselineMean: baseline.mean,
    margin: null,
  };
  if (postN < minN) return base;
  const postMean = mean(postScores);
  if (postMean === null) return base;
  const margin = sigma === null ? 0 : k * sigma * Math.sqrt(1 / postN + 1 / Math.max(1, baseline.n));
  const drop = baseline.mean - postMean;
  return { ...base, margin, decision: drop > margin ? "revert" : "keep" };
}

// ─── patch lessons (append-only blocks separated by a blank line) ────

// Split a .patch.md into its appended lesson blocks (the unit improve.ts joins
// with a blank line). Empty/whitespace input → no lessons.
export function splitLessons(patch: string): string[] {
  return patch
    .split(/\n\s*\n/)
    .map((b) => b.trim())
    .filter((b) => b.length > 0);
}

export function countLessons(patch: string): number {
  return splitLessons(patch).length;
}

// True when adding one more lesson would exceed the per-skill budget — the guard
// against the giant-prompt regression as lessons accumulate.
export function budgetExceeded(patch: string, maxLessons: number): boolean {
  return countLessons(patch) >= maxLessons;
}

// Surgically remove ONE lesson (the auto-revert of a single bad ship), leaving
// the other lessons intact. Returns the rebuilt patch ("" if nothing remains —
// the caller deletes the file then). Matching is on trimmed text.
export function removeLesson(patch: string, lesson: string): string {
  const want = lesson.trim();
  const kept = splitLessons(patch).filter((b) => b !== want);
  return kept.length === 0 ? "" : `${kept.join("\n\n")}\n`;
}
