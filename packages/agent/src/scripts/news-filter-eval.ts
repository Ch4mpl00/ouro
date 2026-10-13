import "dotenv/config";
import "../openai-native-fetch";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Effect } from "effect";
import { config as loadEnv } from "dotenv";
import { z } from "zod";
import {
  choice,
  createDecisionClient,
  filter,
  noul,
  type ChoiceAnswer,
  type DecisionClient,
  type NoulAnswer,
  type Question,
} from "../decisions";
import { DEFAULT_DECISION_MODEL } from "../models";
import { createOpenRouterClient } from "../providers";
import { nullTracer } from "../tracing";

// Golden-set eval of the news-digest significance filter: a typed decision
// (Jev, one call per post) against a reference labelling (opus.json, written
// by an Opus agent from the same interests rubric). See
// .claude/tasks/jev-system-one.md, candidate 1.
//
//   tsx news-filter-eval.ts jev [--batch N] [--concurrency 8] [--model typesafe/jev-1.13]
//   tsx news-filter-eval.ts compare [--run jev.json]
//   tsx news-filter-eval.ts filter      # the shipped path: filter preset +
//                                       # the planner anchor's prompt over
//                                       # list_news-shaped items → jev-filter.json
//
// --batch N packs N posts into one request: the rubric goes into `state` once
// and each post gets its own short `noul` question, so the rubric is paid for
// once per batch instead of once per post. Writes jev-batchN.json.
//
// Fixture dir: packages/agent/eval-fixtures/news-filter/
//   items.json     the posts {id, channel, posted_at, text}
//   interests.md   the rubric both labellers get
//   opus.json      reference labels  (from the Opus agent)
//   jev.json       Jev's answers     (written by `jev`)
//   report.md      the comparison    (written by `compare`)

loadEnv({ path: ".env.agent" });

const DIR = join(import.meta.dirname, "../../eval-fixtures/news-filter");

const Item = z.object({ id: z.number(), channel: z.string().nullable(), posted_at: z.string(), text: z.string() });
const OpusLabel = z.object({
  id: z.number(),
  keep: z.boolean(),
  confidence: z.enum(["high", "low"]),
  category: z.string(),
  reason: z.string(),
});
const JevRun = z.object({
  model: z.string(),
  concurrency: z.number(),
  batch: z.number().default(1),
  wallMs: z.number(),
  costUsd: z.number(),
  inputTokens: z.number(),
  results: z.array(
    z.object({ id: z.number(), probability: z.number(), latencyMs: z.number(), costUsd: z.number() }),
  ),
});
type JevRun = z.input<typeof JevRun>;

function readJson<S extends z.ZodTypeAny>(name: string, schema: S): z.output<S> {
  return schema.parse(JSON.parse(readFileSync(join(DIR, name), "utf8")));
}

function flag(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i === -1 ? undefined : process.argv[i + 1];
}

// ─── jev ─────────────────────────────────────────────────────────────

async function runJev(): Promise<void> {
  const apiKey = process.env.OPEN_ROUTER_API_KEY;
  if (!apiKey) throw new Error("OPEN_ROUTER_API_KEY is not set");
  const model = flag("--model") ?? DEFAULT_DECISION_MODEL;
  const concurrency = Number(flag("--concurrency") ?? 8);
  const items = readJson("items.json", z.array(Item));
  const rubric = readFileSync(join(DIR, "interests.md"), "utf8");

  const decisions = createDecisionClient({ client: createOpenRouterClient(apiKey), model });
  const scope = nullTracer.trace({ id: "news-filter-eval", name: "news-filter-eval" });
  const batch = Number(flag("--batch") ?? 1);
  const batches: Array<typeof items> = [];
  for (let i = 0; i < items.length; i += batch) batches.push(items.slice(i, i + batch));

  // One post per call: the rubric rides in the question. Several posts per
  // call: the rubric and the posts ride in `state`, one short question each.
  const decideBatch = async (posts: typeof items) => {
    if (batch === 1) {
      const [item] = posts;
      if (!item) return [];
      const decision = await decisions.decide({
        state: { channel: item.channel, text: item.text },
        questions: {
          keep: noul(`Этот пост стоит включить в дайджест читателя согласно правилам отбора.\n\n${rubric}`, {
            true: "Включить",
            false: "Пропустить",
          }),
        },
        scope,
      });
      return [{ id: item.id, probability: decision.answer("keep").probability, decision }];
    }
    const questions: Record<string, Question<NoulAnswer>> = {};
    posts.forEach((_, i) => {
      questions[`post_${i + 1}`] = noul(
        `Пост №${i + 1} из posts стоит включить в дайджест читателя согласно правилам отбора из rules.`,
        { true: "Включить", false: "Пропустить" },
      );
    });
    const decision = await decisions.decide({
      state: {
        rules: rubric,
        posts: posts.map((item, i) => ({ n: i + 1, channel: item.channel, text: item.text })),
      },
      questions,
      scope,
    });
    return posts.map((item, i) => ({
      id: item.id,
      probability: decision.answer(`post_${i + 1}`).probability,
      decision,
    }));
  };

  const started = Date.now();
  const perBatch = await Effect.runPromise(
    Effect.forEach(
      batches,
      (posts) =>
        Effect.promise(async () => {
          const t0 = Date.now();
          const answers = await decideBatch(posts);
          const latencyMs = Date.now() - t0;
          const decision = answers[0]?.decision;
          console.log(`${posts.length} post(s) from ${posts[0]?.id}  ${latencyMs}ms  ${decision?.usage?.input ?? 0} tok`);
          return {
            latencyMs,
            costUsd: decision?.costUsd ?? 0,
            inputTokens: decision?.usage?.input ?? 0,
            answers: answers.map(({ id, probability }) => ({ id, probability })),
          };
        }),
      { concurrency },
    ),
  );
  const run: JevRun = {
    model,
    concurrency,
    batch,
    wallMs: Date.now() - started,
    costUsd: perBatch.reduce((s, b) => s + b.costUsd, 0),
    inputTokens: perBatch.reduce((s, b) => s + b.inputTokens, 0),
    // Per-post cost and latency are the batch's, split evenly.
    results: perBatch.flatMap((b) =>
      b.answers.map(({ id, probability }) => ({
        id,
        probability,
        latencyMs: b.latencyMs,
        costUsd: b.costUsd / b.answers.length,
      })),
    ),
  };
  const out = batch === 1 ? "jev.json" : `jev-batch${batch}.json`;
  writeFileSync(join(DIR, out), JSON.stringify(run, null, 1) + "\n");
  console.log(`\n${items.length} posts in ${batches.length} call(s), ${run.wallMs}ms, $${run.costUsd.toFixed(6)}, ${run.inputTokens} input tokens → ${out}`);
}

// ─── compare ─────────────────────────────────────────────────────────

interface Scores {
  threshold: number;
  tp: number;
  fp: number;
  fn: number;
  tn: number;
  accuracy: number;
  precision: number;
  recall: number;
  f1: number;
}

function score(pairs: Array<{ gold: boolean; p: number }>, threshold: number): Scores {
  let tp = 0, fp = 0, fn = 0, tn = 0;
  for (const { gold, p } of pairs) {
    const keep = p >= threshold;
    if (keep && gold) tp++;
    else if (keep) fp++;
    else if (gold) fn++;
    else tn++;
  }
  const precision = tp + fp === 0 ? 0 : tp / (tp + fp);
  const recall = tp + fn === 0 ? 0 : tp / (tp + fn);
  const f1 = precision + recall === 0 ? 0 : (2 * precision * recall) / (precision + recall);
  return { threshold, tp, fp, fn, tn, accuracy: (tp + tn) / pairs.length, precision, recall, f1 };
}

// Area under the ROC curve: P(a random keep post outranks a random skip post).
function auc(pairs: Array<{ gold: boolean; p: number }>): number {
  const pos = pairs.filter((x) => x.gold).map((x) => x.p);
  const neg = pairs.filter((x) => !x.gold).map((x) => x.p);
  let wins = 0;
  for (const a of pos) for (const b of neg) wins += a > b ? 1 : a === b ? 0.5 : 0;
  return pos.length * neg.length === 0 ? NaN : wins / (pos.length * neg.length);
}

const pct = (x: number) => `${(x * 100).toFixed(0)}%`;
const row = (s: Scores) =>
  `| ${s.threshold.toFixed(2)} | ${s.tp} | ${s.fp} | ${s.fn} | ${s.tn} | ${pct(s.accuracy)} | ${pct(s.precision)} | ${pct(s.recall)} | ${s.f1.toFixed(2)} |`;

function compare(): void {
  const items = readJson("items.json", z.array(Item));
  const opus = new Map(readJson("opus.json", z.array(OpusLabel)).map((l) => [l.id, l]));
  const runFile = flag("--run") ?? "jev.json";
  const jev = readJson(runFile, JevRun);
  const jevById = new Map(jev.results.map((r) => [r.id, r]));

  const joined = items.map((item) => {
    const gold = opus.get(item.id);
    const answer = jevById.get(item.id);
    if (!gold || !answer) throw new Error(`post ${item.id} is missing from opus.json or jev.json`);
    return { item, gold, p: answer.probability };
  });
  const pairs = joined.map((j) => ({ gold: j.gold.keep, p: j.p }));
  const sure = joined.filter((j) => j.gold.confidence === "high").map((j) => ({ gold: j.gold.keep, p: j.p }));

  const thresholds = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
  const sweep = thresholds.map((t) => score(pairs, t));
  const best = sweep.reduce((a, b) => (b.f1 > a.f1 ? b : a));
  const at = score(pairs, 0.5);
  const latencies = jev.results.map((r) => r.latencyMs).sort((a, b) => a - b);
  const q = (f: number) => latencies[Math.min(latencies.length - 1, Math.floor(f * latencies.length))] ?? 0;

  const misses = joined
    .filter((j) => j.gold.keep !== j.p >= best.threshold)
    .sort((a, b) => Math.abs(b.p - best.threshold) - Math.abs(a.p - best.threshold));
  const snippet = (text: string) => text.replace(/\s+/g, " ").slice(0, 140);

  const lines = [
    `# News filter: Opus (reference) vs ${jev.model}`,
    "",
    `${items.length} latest channel posts. Reference: Opus, ${pairs.filter((p) => p.gold).length} keep / ${pairs.filter((p) => !p.gold).length} skip, ${joined.length - sure.length} marked low-confidence.`,
    "",
    "## Jev cost and speed",
    "",
    `- ${Math.ceil(jev.results.length / jev.batch)} calls, ${jev.batch} post(s) each, concurrency ${jev.concurrency}`,
    `- wall time ${(jev.wallMs / 1000).toFixed(1)} s; per call p50 ${q(0.5)} ms, p90 ${q(0.9)} ms, max ${q(1)} ms`,
    `- cost $${jev.costUsd.toFixed(5)} total, ${jev.inputTokens} input tokens (≈ $${((jev.costUsd / jev.results.length) * 1000).toFixed(3)} per 1000 posts)`,
    "",
    "## Agreement",
    "",
    `- ROC AUC ${auc(pairs).toFixed(3)} (all), ${auc(sure).toFixed(3)} (Opus high-confidence only)`,
    `- at p ≥ 0.5: accuracy ${pct(at.accuracy)}, precision ${pct(at.precision)}, recall ${pct(at.recall)}`,
    `- best F1 threshold ${best.threshold.toFixed(2)}: accuracy ${pct(best.accuracy)}, precision ${pct(best.precision)}, recall ${pct(best.recall)}`,
    "",
    "| threshold | TP | FP | FN | TN | accuracy | precision | recall | F1 |",
    "|---|---|---|---|---|---|---|---|---|",
    ...sweep.map(row),
    "",
    `## Disagreements at threshold ${best.threshold.toFixed(2)} (${misses.length})`,
    "",
    "| id | Opus | Jev p | conf | category | post | Opus reason |",
    "|---|---|---|---|---|---|---|",
    ...misses.map(
      (m) =>
        `| ${m.item.id} | ${m.gold.keep ? "keep" : "skip"} | ${m.p.toFixed(2)} | ${m.gold.confidence} | ${m.gold.category} | ${snippet(m.item.text).replace(/\|/g, "/")} | ${m.gold.reason.replace(/\|/g, "/")} |`,
    ),
    "",
  ];
  writeFileSync(join(DIR, runFile.replace(/^jev/, "report").replace(/\.json$/, ".md")), lines.join("\n"));
  console.log(lines.slice(0, 28).join("\n"));
}

// ─── pipeline: filter + dedup in one pass ────────────────────────────
//
// One Jev request per batch answers three questions per post:
//   keep_n     noul    passes the rubric
//   covered_n  noul    already in the previous digest, nothing new
//   dup_n      choice  repeats which other post that has at least as many facts
// Every post's text, the rubric and the previous digest are sent once per
// batch. The embeddings (already stored per post) are free: they pick each
// post's dup candidates and pack candidates into the same batch, so a post
// with no near neighbour gets no dup question at all.

const NEIGHBOUR_MIN_COSINE = 0.55;
const MAX_NEIGHBOURS = 5;
const MAX_BATCH = 25;

const Embeddings = z.record(z.array(z.number()));
const LastDigest = z.object({ id: z.number(), sent_at: z.string(), text: z.string() });
const PipelineRun = z.object({
  model: z.string(),
  wallMs: z.number(),
  costUsd: z.number(),
  inputTokens: z.number(),
  batches: z.array(z.array(z.number())),
  results: z.array(
    z.object({
      id: z.number(),
      keep: z.number(),
      covered: z.number(),
      neighbours: z.array(z.object({ id: z.number(), cosine: z.number() })),
      // P(each candidate) from the dup choice; absent when the post had no
      // in-batch candidate. "none" is the no-duplicate option.
      dup: z.record(z.number()).nullable(),
    }),
  ),
});
type PipelineRun = z.infer<typeof PipelineRun>;

function unit(v: number[]): number[] {
  const n = Math.hypot(...v);
  return v.map((x) => x / n);
}

function dot(a: number[], b: number[]): number {
  return a.reduce((s, x, i) => s + x * (b[i] ?? 0), 0);
}

function neighbourGraph(ids: number[], vectors: Map<number, number[]>): Map<number, Array<{ id: number; cosine: number }>> {
  const graph = new Map<number, Array<{ id: number; cosine: number }>>();
  for (const a of ids) {
    const va = vectors.get(a) ?? [];
    graph.set(
      a,
      ids
        .filter((b) => b !== a)
        .map((b) => ({ id: b, cosine: dot(va, vectors.get(b) ?? []) }))
        .filter((n) => n.cosine >= NEIGHBOUR_MIN_COSINE)
        .sort((x, y) => y.cosine - x.cosine)
        .slice(0, MAX_NEIGHBOURS),
    );
  }
  return graph;
}

// Union-find over undirected edges → groups, singletons included.
function components(ids: number[], edges: Array<[number, number]>): number[][] {
  const parent = new Map(ids.map((id) => [id, id]));
  const find = (x: number): number => {
    const p = parent.get(x) ?? x;
    if (p === x) return x;
    const root = find(p);
    parent.set(x, root);
    return root;
  };
  for (const [a, b] of edges) parent.set(find(a), find(b));
  const groups = new Map<number, number[]>();
  for (const id of ids) groups.set(find(id), [...(groups.get(find(id)) ?? []), id]);
  return [...groups.values()];
}

// First-fit decreasing: neighbour groups stay inside one batch.
function packBatches(groups: number[][]): number[][] {
  const bins: number[][] = [];
  const chunks = groups
    .flatMap((g) => Array.from({ length: Math.ceil(g.length / MAX_BATCH) }, (_, i) => g.slice(i * MAX_BATCH, (i + 1) * MAX_BATCH)))
    .sort((a, b) => b.length - a.length);
  for (const chunk of chunks) {
    const bin = bins.find((b) => b.length + chunk.length <= MAX_BATCH);
    if (bin) bin.push(...chunk);
    else bins.push([...chunk]);
  }
  return bins;
}

async function runPipeline(): Promise<void> {
  const apiKey = process.env.OPEN_ROUTER_API_KEY;
  if (!apiKey) throw new Error("OPEN_ROUTER_API_KEY is not set");
  const model = flag("--model") ?? DEFAULT_DECISION_MODEL;
  const items = readJson("items.json", z.array(Item));
  const byId = new Map(items.map((i) => [i.id, i]));
  const rubric = readFileSync(join(DIR, "interests.md"), "utf8");
  const previous = readJson("last-digest.json", LastDigest);
  const raw = readJson("embeddings.json", Embeddings);
  const vectors = new Map(items.map((i) => [i.id, unit(raw[String(i.id)] ?? [])]));

  const ids = items.map((i) => i.id);
  const graph = neighbourGraph(ids, vectors);
  const edges = ids.flatMap((a) => (graph.get(a) ?? []).map((n): [number, number] => [a, n.id]));
  const batches = packBatches(components(ids, edges));

  const decisions = createDecisionClient({ client: createOpenRouterClient(apiKey), model });
  const scope = nullTracer.trace({ id: "news-filter-pipeline", name: "news-filter-pipeline" });

  const runBatch = async (batch: number[]) => {
    const n = new Map(batch.map((id, i) => [id, i + 1]));
    const questions: Record<string, Question<NoulAnswer | ChoiceAnswer<string>>> = {};
    for (const [id, k] of n) {
      questions[`keep_${k}`] = noul(`Пост №${k} из posts стоит включить в дайджест читателя согласно правилам отбора из rules.`);
      questions[`covered_${k}`] = noul(
        `Событие поста №${k} уже есть в previous_digest, и пост не добавляет к нему новых фактов.`,
      );
      const candidates = (graph.get(id) ?? []).flatMap((c) => {
        const m = n.get(c.id);
        return m === undefined ? [] : [m];
      });
      if (candidates.length > 0) {
        questions[`dup_${k}`] = choice(
          `Пост №${k} сообщает о том же событии, что и другой пост из posts, в котором фактов не меньше. Какой это пост?`,
          { none: "Такого поста нет", ...Object.fromEntries(candidates.map((m) => [String(m), `Пост №${m}`])) },
        );
      }
    }
    const decision = await decisions.decide({
      state: {
        rules: rubric,
        previous_digest: previous.text,
        posts: batch.map((id) => ({ n: n.get(id), channel: byId.get(id)?.channel, text: byId.get(id)?.text })),
      },
      questions,
      scope,
    });
    const noulP = (key: string): number => {
      const a = decision.answer(key);
      return "probability" in a ? a.probability : NaN;
    };
    const results = batch.map((id) => {
      const k = n.get(id) ?? 0;
      let dup: Record<string, number> | null = null;
      if (`dup_${k}` in questions) {
        const a = decision.answer(`dup_${k}`);
        if ("probabilities" in a) {
          dup = Object.fromEntries(
            [...a.probabilities].map(([option, p]) => [option === "none" ? "none" : String(batch[Number(option) - 1]), p]),
          );
        }
      }
      return { id, keep: noulP(`keep_${k}`), covered: noulP(`covered_${k}`), neighbours: graph.get(id) ?? [], dup };
    });
    console.log(`batch of ${batch.length}: ${Object.keys(questions).length} questions, ${decision.usage?.input ?? 0} tok`);
    return { results, costUsd: decision.costUsd ?? 0, inputTokens: decision.usage?.input ?? 0 };
  };

  const started = Date.now();
  const perBatch = await Effect.runPromise(
    Effect.forEach(batches, (batch) => Effect.promise(() => runBatch(batch)), { concurrency: 8 }),
  );
  const run: PipelineRun = {
    model,
    wallMs: Date.now() - started,
    costUsd: perBatch.reduce((s, b) => s + b.costUsd, 0),
    inputTokens: perBatch.reduce((s, b) => s + b.inputTokens, 0),
    batches,
    results: perBatch.flatMap((b) => b.results),
  };
  writeFileSync(join(DIR, "pipeline.json"), JSON.stringify(run, null, 1) + "\n");
  console.log(
    `\n${items.length} posts in ${batches.length} call(s) [${batches.map((b) => b.length).join(", ")}], ${run.wallMs}ms, $${run.costUsd.toFixed(6)}, ${run.inputTokens} input tokens → pipeline.json`,
  );
}

// ─── compare-dedup ───────────────────────────────────────────────────

const DedupGold = z.object({
  events: z.array(z.object({ event: z.string(), post_ids: z.array(z.number()), fullest: z.number() })),
  covered: z.array(
    z.object({
      id: z.number(),
      covered: z.boolean(),
      bullet: z.number().nullable(),
      confidence: z.enum(["high", "low"]),
      reason: z.string(),
    }),
  ),
});

const KEEP_THRESHOLD = 0.3;
const COVERED_THRESHOLD = Number(flag("--covered") ?? 0.5);

// Pairwise clustering agreement: a pair counts when both posts share a group.
function pairScores(predicted: number[][], gold: number[][]): { precision: number; recall: number; f1: number; pairs: number } {
  const pairsOf = (groups: number[][]) =>
    new Set(groups.flatMap((g) => g.flatMap((a, i) => g.slice(i + 1).map((b) => [a, b].sort((x, y) => x - y).join("-")))));
  const p = pairsOf(predicted);
  const g = pairsOf(gold);
  const hit = [...p].filter((x) => g.has(x)).length;
  const precision = p.size === 0 ? 1 : hit / p.size;
  const recall = g.size === 0 ? 1 : hit / g.size;
  return { precision, recall, f1: precision + recall === 0 ? 0 : (2 * precision * recall) / (precision + recall), pairs: p.size };
}

// The post a dup answer points at, when Jev thinks it is a duplicate at all.
function dupTarget(dup: Record<string, number> | null): number | null {
  if (!dup || (dup.none ?? 0) >= 0.5) return null;
  const [best] = Object.entries(dup)
    .filter(([k]) => k !== "none")
    .sort((a, b) => b[1] - a[1]);
  return best ? Number(best[0]) : null;
}

function compareDedup(): void {
  const items = readJson("items.json", z.array(Item));
  const length = new Map(items.map((i) => [i.id, i.text.length]));
  const ids = items.map((i) => i.id);
  const opus = new Map(readJson("opus.json", z.array(OpusLabel)).map((l) => [l.id, l]));
  const gold = readJson("dedup-opus.json", DedupGold);
  const goldCovered = new Map(gold.covered.map((c) => [c.id, c]));
  const eventOf = new Map(gold.events.flatMap((e, i) => e.post_ids.map((id) => [id, i])));
  const run = readJson("pipeline.json", PipelineRun);
  const res = new Map(run.results.map((r) => [r.id, r]));
  const goldGroups = gold.events.map((e) => e.post_ids);
  const multi = gold.events.filter((e) => e.post_ids.length > 1);

  // Filter quality with two extra questions per post in the same request.
  const keepPairs = ids.map((id) => ({ gold: opus.get(id)?.keep ?? false, p: res.get(id)?.keep ?? 0 }));
  const keepSure = ids
    .filter((id) => opus.get(id)?.confidence === "high")
    .map((id) => ({ gold: opus.get(id)?.keep ?? false, p: res.get(id)?.keep ?? 0 }));

  // Covered vs the previous digest.
  const covPairs = ids.map((id) => ({ gold: goldCovered.get(id)?.covered ?? false, p: res.get(id)?.covered ?? 0 }));
  const cov = score(covPairs, COVERED_THRESHOLD);

  // Dedup clusters: Jev's edges vs embedding-threshold baselines.
  const jevEdges = ids.flatMap((id): Array<[number, number]> => {
    const t = dupTarget(res.get(id)?.dup ?? null);
    return t === null ? [] : [[id, t]];
  });
  const jevClusters = components(ids, jevEdges);
  const raw = readJson("embeddings.json", Embeddings);
  const vectors = new Map(items.map((i) => [i.id, unit(raw[String(i.id)] ?? [])]));
  const allPairs = ids.flatMap((a, i) => ids.slice(i + 1).map((b) => ({ a, b, cosine: dot(vectors.get(a) ?? [], vectors.get(b) ?? []) })));
  const baseline = (tau: number) =>
    pairScores(components(ids, allPairs.filter((p) => p.cosine >= tau).map((p): [number, number] => [p.a, p.b])), goldGroups);
  const candidateEdges = run.results.flatMap((r) => r.neighbours.map((n): [number, number] => [r.id, n.id]));
  const ceiling = pairScores(components(ids, candidateEdges), goldGroups);

  // Fullest wins: in a cluster the survivor is the post nobody else outranks —
  // a post with an outgoing dup edge is dominated. Ties/cycles → longest.
  const survivorOf = (group: number[], edges: Array<[number, number]>): number => {
    const dominated = new Set(edges.filter(([a, b]) => group.includes(a) && group.includes(b)).map(([a]) => a));
    const free = group.filter((id) => !dominated.has(id));
    const pool = free.length > 0 ? free : group;
    return pool.reduce((a, b) => ((length.get(b) ?? 0) > (length.get(a) ?? 0) ? b : a));
  };
  const longest = (group: number[]) => group.reduce((a, b) => ((length.get(b) ?? 0) > (length.get(a) ?? 0) ? b : a));
  const fullestRows = multi.map((e) => {
    const cluster = jevClusters.find((c) => c.includes(e.fullest)) ?? [e.fullest];
    const merged = e.post_ids.filter((id) => cluster.includes(id));
    return { e, merged: merged.length, jev: survivorOf(merged, jevEdges), longest: longest(e.post_ids) };
  });

  // End to end: what would reach the composer.
  const passes = (id: number) => (res.get(id)?.keep ?? 0) >= KEEP_THRESHOLD && (res.get(id)?.covered ?? 1) < COVERED_THRESHOLD;
  const kept = ids.filter(passes);
  const finalPosts = components(kept, jevEdges.filter(([a, b]) => kept.includes(a) && kept.includes(b))).map((g) =>
    survivorOf(g, jevEdges),
  );
  const filterOnly = ids.filter((id) => (res.get(id)?.keep ?? 0) >= KEEP_THRESHOLD);
  const expected = new Set(
    ids.filter((id) => opus.get(id)?.keep && !goldCovered.get(id)?.covered).map((id) => eventOf.get(id)),
  );
  const endToEnd = (posts: number[]) => {
    const events = posts.map((id) => eventOf.get(id));
    const hit = new Set(events.filter((e) => expected.has(e)));
    return {
      posts: posts.length,
      recall: hit.size / expected.size,
      dupes: events.filter((e) => expected.has(e)).length - hit.size,
      noise: events.filter((e) => !expected.has(e)).length,
    };
  };
  const e2e = [
    ["all posts", endToEnd(ids)],
    [`filter only (keep ≥ ${KEEP_THRESHOLD})`, endToEnd(filterOnly)],
    ["filter + covered + dedup", endToEnd(finalPosts)],
  ] as const;

  const snippet = (id: number) => (items.find((i) => i.id === id)?.text ?? "").replace(/\s+/g, " ").slice(0, 90).replace(/\|/g, "/");
  const f = (x: number) => x.toFixed(2);
  const lines = [
    `# News filter + dedup in one Jev pass: ${run.model}`,
    "",
    `${items.length} posts, previous digest #${readJson("last-digest.json", LastDigest).id}. Reference: Opus (opus.json, dedup-opus.json).`,
    "",
    "## Cost",
    "",
    `- ${run.batches.length} calls [${run.batches.map((b) => b.length).join(", ")}], ${(run.wallMs / 1000).toFixed(1)} s wall, $${run.costUsd.toFixed(5)}, ${run.inputTokens} input tokens`,
    `- dup candidates: cosine ≥ ${NEIGHBOUR_MIN_COSINE}, top ${MAX_NEIGHBOURS}; ${run.results.filter((r) => r.dup).length} posts got a dup question`,
    "",
    "## End to end (events the composer receives)",
    "",
    `Expected: ${expected.size} events (some post is Opus-keep and not covered by the previous digest).`,
    "",
    "| stage | posts | event recall | duplicate posts | off-rubric/covered posts |",
    "|---|---|---|---|---|",
    ...e2e.map(([name, s]) => `| ${name} | ${s.posts} | ${pct(s.recall)} | ${s.dupes} | ${s.noise} |`),
    "",
    "## Filter (keep) — with the extra questions in the same request",
    "",
    `- ROC AUC ${auc(keepPairs).toFixed(3)} (all), ${auc(keepSure).toFixed(3)} (Opus high-confidence); filter-only batch25 was 0.919 / 0.995`,
    "",
    "## Covered by the previous digest",
    "",
    `- gold: ${covPairs.filter((p) => p.gold).length} covered; ROC AUC ${auc(covPairs).toFixed(3)}; at p ≥ ${COVERED_THRESHOLD}: precision ${pct(cov.precision)}, recall ${pct(cov.recall)} (TP ${cov.tp}, FP ${cov.fp}, FN ${cov.fn})`,
    "",
    "## Dedup among the posts (pairwise vs Opus events)",
    "",
    `Gold: ${gold.events.length} events, ${multi.length} with 2+ posts.`,
    "",
    "| method | predicted pairs | precision | recall | F1 |",
    "|---|---|---|---|---|",
    (() => { const s = pairScores(jevClusters, goldGroups); return `| **Jev on candidates** | ${s.pairs} | ${pct(s.precision)} | ${pct(s.recall)} | ${f(s.f1)} |`; })(),
    `| candidates, all merged (recall ceiling) | ${ceiling.pairs} | ${pct(ceiling.precision)} | ${pct(ceiling.recall)} | ${f(ceiling.f1)} |`,
    ...[0.6, 0.65, 0.7, 0.8, 0.9].map((t) => { const s = baseline(t); return `| embeddings only, cosine ≥ ${t} | ${s.pairs} | ${pct(s.precision)} | ${pct(s.recall)} | ${f(s.f1)} |`; }),
    "",
    "## Fullest version wins",
    "",
    `Of ${multi.length} multi-post events: Jev survivor = Opus fullest in ${fullestRows.filter((r) => r.jev === r.e.fullest).length}; longest text = Opus fullest in ${fullestRows.filter((r) => r.longest === r.e.fullest).length}.`,
    "",
    "| event | posts | merged by Jev | Opus fullest | Jev survivor | longest |",
    "|---|---|---|---|---|---|",
    ...fullestRows.map((r) => `| ${r.e.event.replace(/\|/g, "/")} | ${r.e.post_ids.join(", ")} | ${r.merged} | ${r.e.fullest} | ${r.jev} | ${r.longest} |`),
    "",
    "## Covered disagreements",
    "",
    "| id | Opus | Jev p | conf | post | Opus reason |",
    "|---|---|---|---|---|---|",
    ...ids
      .filter((id) => (goldCovered.get(id)?.covered ?? false) !== (res.get(id)?.covered ?? 0) >= COVERED_THRESHOLD)
      .map((id) => {
        const g = goldCovered.get(id);
        return `| ${id} | ${g?.covered ? "covered" : "new"} | ${f(res.get(id)?.covered ?? 0)} | ${g?.confidence} | ${snippet(id)} | ${(g?.reason ?? "").replace(/\|/g, "/")} |`;
      }),
    "",
  ];
  writeFileSync(join(DIR, "report-dedup.md"), lines.join("\n"));
  console.log(lines.join("\n"));
}

// ─── filter: the shipped path ────────────────────────────────────────

// The filter prompt exactly as the planner anchor pins it, whitespace folded
// the way the compiler re-emits it.
function anchorFilterPrompt(): string {
  const planner = readFileSync(join(import.meta.dirname, "../../../../skills.default/planner.md"), "utf8");
  const match = planner.match(/"kind":"decide","preset":"filter"[\s\S]*?"prompt":"([^"]*)"/);
  if (!match?.[1]) throw new Error('no {"kind":"decide","preset":"filter"} prompt in planner.md');
  return match[1].replace(/\s+/g, " ").trim();
}

// Sums what each decide call reports, without touching the client.
function metered(client: DecisionClient): DecisionClient & { costUsd: () => number; inputTokens: () => number } {
  let cost = 0;
  let tokens = 0;
  return {
    model: client.model,
    async decide(req) {
      const decision = await client.decide(req);
      cost += decision.costUsd ?? 0;
      tokens += decision.usage?.input ?? 0;
      return decision;
    },
    costUsd: () => cost,
    inputTokens: () => tokens,
  };
}

async function runFilter(): Promise<void> {
  const apiKey = process.env.OPEN_ROUTER_API_KEY;
  if (!apiKey) throw new Error("OPEN_ROUTER_API_KEY is not set");
  const model = flag("--model") ?? DEFAULT_DECISION_MODEL;
  const rows = readJson("list-news-items.json", z.array(z.object({ id: z.number(), item: z.record(z.unknown()) })));
  const decisions = metered(createDecisionClient({ client: createOpenRouterClient(apiKey), model }));
  const scope = nullTracer.trace({ id: "news-filter-eval", name: "news-filter-eval" });

  const started = Date.now();
  const { probabilities } = await filter(decisions, {
    items: rows.map((r) => r.item),
    prompt: anchorFilterPrompt(),
    threshold: 0.01,
    scope,
  });
  const wallMs = Date.now() - started;
  const run: JevRun = {
    model,
    concurrency: 16,
    batch: 25,
    wallMs,
    costUsd: decisions.costUsd(),
    inputTokens: decisions.inputTokens(),
    results: rows.map((r, i) => ({ id: r.id, probability: probabilities[i] ?? 0, latencyMs: wallMs, costUsd: decisions.costUsd() / rows.length })),
  };
  writeFileSync(join(DIR, "jev-filter.json"), JSON.stringify(run, null, 1) + "\n");
  console.log(`${rows.length} items, ${wallMs}ms, $${run.costUsd.toFixed(6)}, ${run.inputTokens} input tokens → jev-filter.json`);
}

const command = process.argv[2];
if (command === "jev") await runJev();
else if (command === "compare") compare();
else if (command === "pipeline") await runPipeline();
else if (command === "compare-dedup") compareDedup();
else if (command === "filter") await runFilter();
else throw new Error("usage: news-filter-eval.ts jev | compare | pipeline | compare-dedup");
