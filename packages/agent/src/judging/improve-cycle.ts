import { z } from "zod";
import type { JudgementRecord, TraceStore } from "../db/trace-store";
import type { Observation } from "../trace-model";
import type { SkillStore } from "../skills";
import { assembleNodeMaterials, type NodeMaterial } from "./materials";
import { runNodeGate, type NodeGateResult } from "./gate";
import { buildGateTarget, runModel as defaultRunModel } from "./gate-runtime";
import type { JudgeBackend } from "./judge-backend";
import type { JudgeProvider } from "./judge-backend";
import { type NoiseAxis } from "./noise";
import { JUDGE_PROMPT_VERSION } from "./schema";
import type { TraceSource } from "./trace-source";
import {
  authorPatch,
  decideShip,
  dominantMode,
  induceTaxonomy,
  selectCandidates,
  type PatchExample,
} from "./improver";
import { budgetExceeded, mean } from "./monitor";

// ONE improvement cycle for a single (skill, axis): select recent failures →
// induce a failure-mode taxonomy → author an append-only lesson for the dominant
// mode (with ≤1 informed retry on a failed gate) → cheap-gate it (stored before,
// cluster S× + holdout S=1) → decide → ship. Pure orchestration over injected
// deps so BOTH the CLI (scripts/improve.ts) and the cron worker
// (scripts/improve-worker.ts) drive the exact same logic. The live-trend monitor
// and auto-revert live in the worker (they span runs); this is one cycle.

// Pull the judge's rationale for one axis out of the stored detail JSON.
const DetailSchema = z.object({
  scorecard: z
    .object({
      axes: z.array(
        z.object({ axis: z.string(), rationale: z.string().optional(), evidence: z.string().optional() }),
      ),
    })
    .optional(),
  faithfulness: z.object({ note: z.string().optional() }).nullable().optional(),
});

export function judgeRationale(detail: unknown, axis: NoiseAxis): string {
  const p = DetailSchema.safeParse(detail);
  if (!p.success) return "";
  if (axis === "faithfulness") return p.data.faithfulness?.note ?? "";
  const a = p.data.scorecard?.axes.find((x) => x.axis === axis);
  if (!a) return "";
  return `${a.rationale ?? ""}${a.evidence ? ` (evidence: ${a.evidence})` : ""}`.trim();
}

export type CycleOutcome =
  | "no-corpus" // nothing judged for this (skill, axis) yet
  | "no-candidates" // floor reached the ceiling — no confident recent failures
  | "no-mode" // taxonomy/material yielded nothing patchable
  | "no-fix" // author found no generalizable lesson
  | "rejected" // candidate(s) failed the gate
  | "budget" // accepted, but the per-skill patch budget is full — not shipped
  | "accepted" // passed the gate, but apply=false (propose-only)
  | "shipped"; // applied to skills/<skill>.patch.md

export interface CycleResult {
  outcome: CycleOutcome;
  mode?: string;
  lesson?: string;
  reasons?: string[];
  // Pre-ship recent axis mean + count — the baseline the worker's live monitor
  // compares post-ship prod traces against. Set on "shipped".
  baseline?: { mean: number; n: number };
  patchPath?: string;
}

export interface ImproveCycleOpts {
  skill: string;
  axis: NoiseAxis;
  provider: JudgeProvider;
  cluster: number;
  holdout: number;
  samples: number; // cluster samples (S); holdout is always S=1
  absMax: number;
  bar: number;
  holdoutMin: number;
  recentDays: number;
  k: number;
  apply: boolean;
  maxAttempts: number; // ≤2 → one informed retry after a failed gate
  budget: number; // per-skill patch lesson budget (block new ships when full)
  now: number; // Date.now(), injected for the recent window + testability
}

export interface ImproveCycleDeps {
  store: Pick<TraceStore, "listJudgements">;
  source: TraceSource;
  skillStore: Pick<SkillStore, "readPatch" | "savePatch">;
  backend: JudgeBackend;
  sigma: Partial<Record<NoiseAxis, number>>;
  runModel?: (messages: Parameters<typeof defaultRunModel>[0], model: string, jsonMode: boolean) => Promise<string>;
  log?: (msg: string) => void;
}

const nid = (r: { traceId: string; observationId: string }): string => `${r.traceId}:${r.observationId}`;

// Compact feedback for the informed retry: why the previous lesson failed the
// gate, per cluster node (before→after on the target axis + verdict).
function gateFeedback(reasons: string[], cluster: NodeGateResult[], axis: NoiseAxis): string {
  const lines = cluster.map((n) => {
    const g = n.grades.find((x) => x.axis === axis);
    if (!g) return `- ${n.node.label}: ${axis} not scored`;
    const f = (x: number | null) => (x === null ? "—" : x.toFixed(2));
    return `- ${n.node.label}: ${axis} ${f(g.beforeMean)}→${f(g.afterMean)} [${g.verdict}]`;
  });
  return [`Gate reasons: ${reasons.join("; ")}`, ...lines].join("\n");
}

export async function runImproveCycle(deps: ImproveCycleDeps, opts: ImproveCycleOpts): Promise<CycleResult> {
  const log = deps.log ?? (() => {});
  const runModel = deps.runModel ?? defaultRunModel;
  const { skill, axis } = opts;
  const axisSigma = deps.sigma[axis] ?? null;

  const records = deps.store.listJudgements({
    skill,
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
  });
  if (records.length === 0) return { outcome: "no-corpus" };

  const recentSince =
    opts.recentDays > 0 ? new Date(opts.now - opts.recentDays * 86_400_000).toISOString() : null;
  const inWindow = (r: JudgementRecord): boolean => recentSince === null || r.startedAt >= recentSince;

  const { candidates, holdout } = selectCandidates(records, axis, {
    holdoutSize: opts.holdout,
    absMax: opts.absMax,
    bar: opts.bar,
    k: opts.k,
    sigma: axisSigma,
    holdoutMin: opts.holdoutMin,
    recentSince,
  });
  log(`corpus=${records.length} · recent candidates=${candidates.length} · holdout=${holdout.length}`);
  if (candidates.length === 0) return { outcome: "no-candidates" };

  // Pre-ship baseline = mean of the axis over ALL recent nodes (not just the
  // failures) — the skill's current live level the monitor will compare against.
  const recentScores = records
    .filter((r) => inWindow(r) && r.scores[axis] !== null)
    .map((r) => r.scores[axis]!);
  const baseMean = mean(recentScores);
  const baseline = baseMean === null ? null : { mean: baseMean, n: recentScores.length };

  // Taxonomy → dominant mode → cluster.
  const rationaleById = new Map(candidates.map((r) => [nid(r), judgeRationale(r.detail, axis)]));
  log(`Inducing failure-mode taxonomy over ${candidates.length} candidate(s)…`);
  const taxonomy = await induceTaxonomy(deps.backend, {
    skill,
    axis,
    items: candidates.map((r) => ({ id: nid(r), rationale: rationaleById.get(nid(r)) ?? "" })),
  });
  for (const m of taxonomy.modes) {
    log(`  · ${m.name} (${m.nodeIds.filter((id) => rationaleById.has(id)).length}): ${m.description}`);
  }
  const mode = dominantMode(taxonomy, new Set(rationaleById.keys()));
  if (!mode) return { outcome: "no-mode" };
  const modeIds = new Set(mode.nodeIds);
  const cluster = candidates.filter((r) => modeIds.has(nid(r))).slice(0, opts.cluster);
  log(`Dominant mode: "${mode.name}" — ${mode.description} · cluster=${cluster.length}`);

  // Resolve cluster + holdout nodes to replayable material (cache per trace).
  const traceCache = new Map<string, { nodes: NodeMaterial[]; byId: Map<string, Observation> }>();
  async function resolve(rec: JudgementRecord): Promise<{ node: NodeMaterial; obs: Observation | undefined } | null> {
    let entry = traceCache.get(rec.traceId);
    if (!entry) {
      const { observations } = await deps.source.getTrace(rec.traceId);
      const { nodes } = await assembleNodeMaterials(deps.source, rec.traceId);
      entry = { nodes, byId: new Map(observations.map((o) => [o.id, o])) };
      traceCache.set(rec.traceId, entry);
    }
    const node = entry.nodes.find((n) => n.observationId === rec.observationId);
    return node ? { node, obs: entry.byId.get(rec.observationId) } : null;
  }

  const examples: PatchExample[] = [];
  let contract: string | null = null;
  for (const rec of cluster) {
    const r = await resolve(rec);
    if (!r) continue;
    contract = r.node.contract;
    examples.push({
      inputExcerpt: r.node.inputText,
      output: r.node.outputText,
      judgeRationale: judgeRationale(rec.detail, axis) || "(no rationale recorded)",
    });
  }
  if (examples.length === 0) return { outcome: "no-mode" };

  const existingPatch = (await deps.skillStore.readPatch(skill)) ?? "";

  // Short-circuit before spending codex on author+gate: if we'd ship but the
  // per-skill budget is full, there's nothing to do until lessons are pruned.
  // (In propose-only mode we still author, to surface the candidate.)
  if (opts.apply && budgetExceeded(existingPatch, opts.budget)) {
    log(`patch budget (${opts.budget}) reached for ${skill} — not authoring; prune lessons or raise the budget.`);
    return { outcome: "budget", mode: mode.description };
  }

  async function gateNodes(recs: JudgementRecord[], samples: number, lesson: string): Promise<NodeGateResult[]> {
    const out: NodeGateResult[] = [];
    for (const rec of recs) {
      const r = await resolve(rec);
      if (!r) continue;
      const target = buildGateTarget(r.node, r.obs);
      if (!target) continue;
      out.push(await runNodeGate({ backend: deps.backend, runModel }, target, lesson, samples, deps.sigma, opts.k, rec.scores));
    }
    return out;
  }

  // Author → gate, with ≤(maxAttempts−1) informed retries on a failed gate.
  let priorFeedback: string | null = null;
  let lastReasons: string[] = [];
  for (let attempt = 1; attempt <= Math.max(1, opts.maxAttempts); attempt++) {
    const authored = await authorPatch(deps.backend, {
      skill,
      axis,
      contract,
      examples,
      failureMode: mode.description,
      existingPatch,
      priorFeedback,
    });
    log(`[attempt ${attempt}] candidate: ${authored.lesson.trim() ? authored.lesson.trim() : "(empty)"}`);
    if (authored.lesson.trim().length === 0) return { outcome: "no-fix", mode: mode.description };

    const clusterResults = await gateNodes(cluster, opts.samples, authored.lesson);
    const holdoutResults = await gateNodes(holdout, 1, authored.lesson);
    const decision = decideShip(axis, clusterResults, holdoutResults);
    lastReasons = decision.reasons;
    for (const r of decision.reasons) log(`  • ${r}`);

    if (!decision.accept) {
      priorFeedback = gateFeedback(decision.reasons, clusterResults, axis);
      continue;
    }

    // Accepted. (Budget was already checked before authoring when apply=true.)
    if (!opts.apply) return { outcome: "accepted", mode: mode.description, lesson: authored.lesson, reasons: decision.reasons };
    const merged =
      existingPatch.trim().length > 0
        ? `${existingPatch.trimEnd()}\n\n${authored.lesson.trim()}\n`
        : `${authored.lesson.trim()}\n`;
    const saved = await deps.skillStore.savePatch(skill, merged);
    log(`SHIPPED → ${saved.path} (${saved.sizeBytes} bytes)`);
    return {
      outcome: "shipped",
      mode: mode.description,
      lesson: authored.lesson.trim(),
      reasons: decision.reasons,
      baseline: baseline ?? undefined,
      patchPath: saved.path,
    };
  }

  return { outcome: "rejected", mode: mode.description, reasons: lastReasons };
}
