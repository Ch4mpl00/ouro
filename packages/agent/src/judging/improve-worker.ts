import type { JudgementRecord, TraceStore } from "../db/trace-store";
import type { ImproverStore } from "../db/improver-store";
import type { SkillStore } from "../skills";
import type { JudgeBackend, JudgeProvider } from "./judge-backend";
import { NOISE_AXES, type NoiseAxis } from "./noise";
import { JUDGE_PROMPT_VERSION } from "./schema";
import type { TraceSource } from "./trace-source";
import { runImproveCycle, type CycleResult } from "./improve-cycle";
import { decideRevert, removeLesson } from "./monitor";

// The closed-loop improver's cron driver (Phase 3, п3). On each tick it walks
// every (skill, axis) present in the judged corpus and, per pair:
//   1. MONITOR — if the last ship is still "pending", compare the axis's prod
//      scores on traces that ran AFTER the ship against the pre-ship baseline.
//      Confident drop → AUTO-REVERT (remove the lesson); held → mark "kept";
//      too few post-ship traces → keep watching and DON'T author (one change at
//      a time, so each ship's effect stays attributable).
//   2. IMPROVE — otherwise run one runImproveCycle and record the outcome.
// Mirrors judge-worker: a long-running poll loop, one service in compose. The
// gate is the in-process guard; THIS adds prod as the second, ground-truth guard.

export interface ImproveWorkerDeps {
  store: Pick<TraceStore, "listJudgements" | "listJudgedSkills">;
  improverStore: ImproverStore;
  skillStore: SkillStore;
  source: TraceSource;
  backend: JudgeBackend;
  sigma: Partial<Record<NoiseAxis, number>>;
  log?: (msg: string) => void;
}

export interface ImproveWorkerOpts {
  provider: JudgeProvider;
  apply: boolean; // false → shadow mode (propose + log, never ship). Flip on after prod-validation.
  pollIntervalMs: number;
  once: boolean;
  recentDays: number;
  minMonitorN: number; // min post-ship traces before the monitor will judge a trend
  k: number;
  budget: number;
  cluster: number;
  holdout: number;
  samples: number;
  absMax: number;
  bar: number;
  holdoutMin: number;
  maxAttempts: number;
  guardFaithfulness: boolean;
  // Cost ceiling: at most this many improve cycles (the codex-heavy step) per
  // tick. Monitoring/auto-revert is free and runs for ALL pairs every tick; only
  // AUTHORING is capped. Pairs are picked least-recently-attempted first
  // (round-robin), so an unattended tick can't blow the shared codex quota.
  maxCyclesPerTick: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function boolEnv(name: string, fallback: boolean): boolean {
  const value = process.env[name];
  if (value === undefined) return fallback;
  return ["1", "true", "yes", "on"].includes(value.toLowerCase());
}

export function improveWorkerOptsFromEnv(): ImproveWorkerOpts {
  const provider: JudgeProvider = process.env.JUDGE_PROVIDER === "openai" ? "openai" : "codex";
  return {
    provider,
    apply: boolEnv("IMPROVE_APPLY", false),
    pollIntervalMs: Number(process.env.IMPROVE_POLL_INTERVAL_MS ?? 86_400_000), // daily; cap below bounds per-tick cost
    once: boolEnv("IMPROVE_ONCE", false),
    recentDays: Number(process.env.IMPROVE_RECENT_DAYS ?? 14),
    minMonitorN: Number(process.env.IMPROVE_MIN_MONITOR_N ?? 5),
    k: Number(process.env.IMPROVE_K ?? 2),
    budget: Number(process.env.IMPROVE_BUDGET ?? 8),
    cluster: Number(process.env.IMPROVE_CLUSTER ?? 3),
    holdout: Number(process.env.IMPROVE_HOLDOUT ?? 3),
    samples: Number(process.env.IMPROVE_SAMPLES ?? 2),
    absMax: Number(process.env.IMPROVE_ABS_MAX ?? 0.6),
    bar: Number(process.env.IMPROVE_BAR ?? 0.75),
    holdoutMin: Number(process.env.IMPROVE_HOLDOUT_MIN ?? 0.85),
    maxAttempts: Number(process.env.IMPROVE_MAX_ATTEMPTS ?? 1), // 1 = no informed retry (retry doubles codex)
    guardFaithfulness: boolEnv("IMPROVE_GUARD_FAITHFULNESS", false),
    maxCyclesPerTick: Number(process.env.IMPROVE_MAX_CYCLES_PER_TICK ?? 1),
  };
}

// Outcome of the monitor step: "watching" / "reverted" / "kept" means we already
// acted on a pending ship; "author" means there is nothing pending so the caller
// should run an improve cycle. "watching" tells the caller to SKIP authoring.
type MonitorAction = "author" | "watching" | "reverted" | "kept";

async function monitorPending(
  deps: ImproveWorkerDeps,
  opts: ImproveWorkerOpts,
  skill: string,
  axis: NoiseAxis,
  records: JudgementRecord[],
  nowISO: string,
): Promise<MonitorAction> {
  const log = deps.log ?? (() => {});
  const state = deps.improverStore.get(skill, axis);
  if (!state || state.monitorStatus !== "pending" || !state.shippedAt || state.baselineMean === null) {
    return "author";
  }

  const post = records
    .filter((r) => r.startedAt > state.shippedAt! && r.scores[axis] !== null)
    .map((r) => r.scores[axis]!);
  const verdict = decideRevert(
    { mean: state.baselineMean, n: state.baselineN ?? 1 },
    post,
    deps.sigma[axis] ?? null,
    opts.k,
    opts.minMonitorN,
  );
  const post3 = verdict.postMean === null ? "—" : verdict.postMean.toFixed(3);
  log(
    `[${skill}/${axis}] monitor: baseline ${state.baselineMean.toFixed(3)} vs post ${post3} (n=${verdict.postN}) → ${verdict.decision}`,
  );

  if (verdict.decision === "insufficient") return "watching";

  if (verdict.decision === "revert") {
    const patch = (await deps.skillStore.readPatch(skill)) ?? "";
    const rebuilt = removeLesson(patch, state.shippedLesson ?? "");
    if (rebuilt.trim().length === 0) await deps.skillStore.deletePatch(skill);
    else await deps.skillStore.savePatch(skill, rebuilt);
    deps.improverStore.upsert(skill, axis, nowISO, {
      lastOutcome: "reverted",
      shippedAt: null,
      shippedLesson: null,
      baselineMean: null,
      baselineN: null,
      monitorStatus: null,
    });
    log(`[${skill}/${axis}] AUTO-REVERTED — live trend fell below baseline; removed the lesson.`);
    return "reverted";
  }

  // kept: the ship held in prod — settle it (stop monitoring) and let the caller
  // author the next improvement this round.
  deps.improverStore.upsert(skill, axis, nowISO, {
    lastOutcome: "kept",
    shippedAt: null,
    shippedLesson: null,
    baselineMean: null,
    baselineN: null,
    monitorStatus: "kept",
  });
  log(`[${skill}/${axis}] ship held in prod — kept.`);
  return "kept";
}

function recordCycle(
  deps: ImproveWorkerDeps,
  skill: string,
  axis: NoiseAxis,
  result: CycleResult,
  nowISO: string,
): void {
  if (result.outcome === "shipped") {
    deps.improverStore.upsert(skill, axis, nowISO, {
      lastOutcome: "shipped",
      shippedAt: nowISO,
      shippedLesson: result.lesson ?? null,
      baselineMean: result.baseline?.mean ?? null,
      baselineN: result.baseline?.n ?? null,
      monitorStatus: result.baseline ? "pending" : null, // can't monitor without a baseline
    });
    return;
  }
  deps.improverStore.upsert(skill, axis, nowISO, {
    lastOutcome: result.outcome,
    shippedAt: null,
    shippedLesson: null,
    baselineMean: null,
    baselineN: null,
    monitorStatus: null,
  });
}

interface Pair {
  skill: string;
  axis: NoiseAxis;
  records: JudgementRecord[];
  lastAttemptAt: string; // "" = never attempted → sorts first (round-robin)
}

async function tick(deps: ImproveWorkerDeps, opts: ImproveWorkerOpts): Promise<void> {
  const log = deps.log ?? (() => {});
  const skills = deps.store.listJudgedSkills({ provider: opts.provider, promptVersion: JUDGE_PROMPT_VERSION });
  log(`[improve-worker] ${skills.length} skill(s) with a corpus: ${skills.join(", ") || "(none)"}`);

  // Enumerate every (skill, axis) pair the corpus actually scores, loading each
  // skill's judgements once.
  const pairs: Pair[] = [];
  for (const skill of skills) {
    const records = deps.store.listJudgements({ skill, provider: opts.provider, promptVersion: JUDGE_PROMPT_VERSION });
    for (const axis of NOISE_AXES.filter((a) => records.some((r) => r.scores[a] !== null))) {
      pairs.push({ skill, axis, records, lastAttemptAt: deps.improverStore.get(skill, axis)?.lastAttemptAt ?? "" });
    }
  }

  // Monitor every pair first — free (no codex): auto-revert a ship whose live
  // trend fell, settle a held one. "watching"/"reverted" pairs skip authoring.
  const eligible: Pair[] = [];
  for (const p of pairs) {
    const action = await monitorPending(deps, opts, p.skill, p.axis, p.records, new Date().toISOString());
    if (action === "author" || action === "kept") eligible.push(p);
  }

  // Author for at most maxCyclesPerTick pairs — the codex-heavy step — picking
  // the least-recently-attempted first so coverage rotates and no single tick
  // can spike the shared quota.
  eligible.sort((a, b) => a.lastAttemptAt.localeCompare(b.lastAttemptAt));
  const toRun = eligible.slice(0, Math.max(0, opts.maxCyclesPerTick));
  log(`[improve-worker] ${eligible.length} pair(s) eligible to author; running ${toRun.length} this tick (cap ${opts.maxCyclesPerTick})`);

  for (const p of toRun) {
    const nowISO = new Date().toISOString();
    const result = await runImproveCycle(deps, {
      skill: p.skill,
      axis: p.axis,
      provider: opts.provider,
      cluster: opts.cluster,
      holdout: opts.holdout,
      samples: opts.samples,
      absMax: opts.absMax,
      bar: opts.bar,
      holdoutMin: opts.holdoutMin,
      recentDays: opts.recentDays,
      k: opts.k,
      apply: opts.apply,
      maxAttempts: opts.maxAttempts,
      budget: opts.budget,
      guardFaithfulness: opts.guardFaithfulness,
      now: Date.now(),
    });
    log(`[${p.skill}/${p.axis}] cycle → ${result.outcome}${result.mode ? ` (mode: ${result.mode})` : ""}`);
    recordCycle(deps, p.skill, p.axis, result, nowISO);
  }
}

export async function runImproveWorker(deps: ImproveWorkerDeps, opts: ImproveWorkerOpts): Promise<void> {
  const log = deps.log ?? (() => {});
  log(
    `[improve-worker] provider=${opts.provider} apply=${opts.apply} interval=${opts.pollIntervalMs}ms ` +
      `cap=${opts.maxCyclesPerTick}/tick maxAttempts=${opts.maxAttempts} guardFaith=${opts.guardFaithfulness} ` +
      `recent=${opts.recentDays}d minMonitorN=${opts.minMonitorN} budget=${opts.budget} once=${opts.once}`,
  );
  if (!opts.apply) log("[improve-worker] SHADOW MODE (IMPROVE_APPLY not set) — proposes + gates but never ships.");

  do {
    try {
      await tick(deps, opts);
    } catch (err) {
      log(`[improve-worker] tick failed: ${err instanceof Error ? err.message : String(err)}`);
    }
    if (opts.once) break;
    await sleep(opts.pollIntervalMs);
  } while (true);
}
