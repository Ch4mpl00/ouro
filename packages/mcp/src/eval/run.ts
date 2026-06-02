import { readFile } from "node:fs/promises";
import type { EmbeddingService } from "../services/embeddings/service";
import { hashCorpusInputs } from "./config";
import { readCorpusCache, writeCorpusCache } from "./corpus-cache";
import { buildText } from "./embed";
import { cosineTopK } from "./search";
import {
  distanceToFirstGold,
  firstGoldRank,
  mean,
  recallAtK,
  reciprocalRank,
} from "./metrics";
import {
  sourceBucket,
  type AggregateMetrics,
  type CorpusRow,
  type EvalConfig,
  type EvalResult,
  type NegativeTestResult,
  type PerQueryResult,
  type QueryRow,
  type RetrievedItem,
} from "./types";

// Always retrieve this many results so R@30 and the wider window
// metrics are computable regardless of config.retrieval.topK (which
// stays as a hint of what a downstream skill would actually show).
const RETRIEVAL_SIZE = 30;

export interface EvalRunDeps {
  embedFactory: (model: string, dimensions: number) => EmbeddingService;
  corpusPath: string;
  queriesPath: string;
  cacheDir: string;
}

export interface EvalRunModule {
  run(config: EvalConfig): Promise<EvalResult>;
}

export function createEvalRun(deps: EvalRunDeps): EvalRunModule {
  return {
    async run(config: EvalConfig): Promise<EvalResult> {
      const corpus = await loadCorpus(deps.corpusPath);
      const queries = await loadQueries(deps.queriesPath);
      const provider = deps.embedFactory(
        config.retrieval.embed.model,
        config.retrieval.embed.dimensions,
      );

      const { vectors: corpusVecs, cacheHit, configHash } = await prepareCorpus(
        corpus,
        config,
        provider,
        deps.cacheDir,
      );

      const corpusForSearch = corpus
        .map((row) => {
          const embedding = corpusVecs.get(row.id);
          return embedding ? { id: row.id, embedding } : null;
        })
        .filter((x): x is { id: number; embedding: number[] } => x !== null);

      const queryTexts = queries.map((q) =>
        config.query.field === "reformulation" ? q.reformulation : q.query,
      );
      const queryVecs = await provider.embedBatch(queryTexts);
      const pairs = queries.map((q, i) => {
        const vec = queryVecs[i];
        if (!vec) throw new Error(`missing query vector at index ${i}`);
        return { q, vec };
      });

      const corpusById = new Map(corpus.map((row) => [row.id, row]));

      const perQuery: PerQueryResult[] = [];
      const negativeTests: NegativeTestResult[] = [];
      for (const { q, vec } of pairs) {
        const topK = cosineTopK(vec, corpusForSearch, RETRIEVAL_SIZE);

        if (q.gold.length === 0 && q.acceptable.length === 0) {
          negativeTests.push({
            qid: q.id,
            query: q.query,
            minDistance: topK[0]?.distance ?? Number.NaN,
            top1Id: topK[0]?.id ?? -1,
          });
          continue;
        }

        perQuery.push(scoreQuery(q, topK, corpusById));
      }

      return {
        config,
        configHash,
        perQuery,
        negativeTests,
        aggregate: aggregateMetrics(perQuery),
        cacheHit,
      };
    },
  };
}

async function loadCorpus(path: string): Promise<CorpusRow[]> {
  const raw = await readFile(path, "utf-8");
  const rows: CorpusRow[] = [];
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    rows.push(JSON.parse(line) as CorpusRow);
  }
  return rows;
}

async function loadQueries(path: string): Promise<QueryRow[]> {
  const raw = await readFile(path, "utf-8");
  const rows: QueryRow[] = [];
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    rows.push(JSON.parse(line) as QueryRow);
  }
  return rows;
}

interface PrepareCorpusResult {
  vectors: Map<number, number[]>;
  cacheHit: boolean;
  configHash: string;
}

async function prepareCorpus(
  corpus: CorpusRow[],
  config: EvalConfig,
  provider: EmbeddingService,
  cacheDir: string,
): Promise<PrepareCorpusResult> {
  const configHash = hashCorpusInputs(config);
  const cached = await readCorpusCache(cacheDir, configHash);
  if (cached && cached.size === corpus.length) {
    return { vectors: cached, cacheHit: true, configHash };
  }

  const texts = corpus.map((row) => buildText(row, config.retrieval.buildText));
  const embeddings = await provider.embedBatch(texts);
  const vectors = new Map<number, number[]>();
  corpus.forEach((row, i) => {
    const vec = embeddings[i];
    if (!vec) throw new Error(`missing embedding for corpus row ${row.id}`);
    vectors.set(row.id, vec);
  });
  await writeCorpusCache(cacheDir, configHash, vectors);
  return { vectors, cacheHit: false, configHash };
}

function scoreQuery(
  q: QueryRow,
  topK: RetrievedItem[],
  corpusById: Map<number, CorpusRow>,
): PerQueryResult {
  const goldHitsAt5 = countHits(q.gold, topK.slice(0, 5));
  const goldHitsAt10 = countHits(q.gold, topK.slice(0, 10));
  const goldHitsAt30 = countHits(q.gold, topK.slice(0, 30));
  return {
    qid: q.id,
    query: q.query,
    goldCount: q.gold.length,
    acceptableCount: q.acceptable.length,
    topK,
    hitAt5: goldHitsAt5,
    hitAt10: goldHitsAt10,
    hitAt30: goldHitsAt30,
    precisionAt5: goldHitsAt5 / 5,
    precisionAt10: goldHitsAt10 / 10,
    uniqueSourcesAt5: uniqueSources(topK.slice(0, 5), corpusById),
    uniqueSourcesAt10: uniqueSources(topK.slice(0, 10), corpusById),
    firstGoldRank: firstGoldRank(q.gold, topK),
    distToFirstGold: distanceToFirstGold(q.gold, topK),
  };
}

function uniqueSources(
  items: RetrievedItem[],
  corpusById: Map<number, CorpusRow>,
): number {
  const buckets = new Set<string>();
  for (const item of items) {
    const row = corpusById.get(item.id);
    if (row) buckets.add(sourceBucket(row));
  }
  return buckets.size;
}

function countHits(gold: number[], topK: RetrievedItem[]): number {
  const goldSet = new Set(gold);
  let hit = 0;
  for (const item of topK) if (goldSet.has(item.id)) hit++;
  return hit;
}

function aggregateMetrics(perQuery: PerQueryResult[]): AggregateMetrics {
  const scored = perQuery.filter((p) => p.goldCount > 0);
  const recall5 = scored.map((p) => recallAtKFromHits(p.hitAt5, p.goldCount));
  const recall10 = scored.map((p) => recallAtKFromHits(p.hitAt10, p.goldCount));
  const recall30 = scored.map((p) => recallAtKFromHits(p.hitAt30, p.goldCount));
  const precision5 = scored.map((p) => p.precisionAt5);
  const precision10 = scored.map((p) => p.precisionAt10);
  const uniqAt5 = scored.map((p) => p.uniqueSourcesAt5);
  const uniqAt10 = scored.map((p) => p.uniqueSourcesAt10);
  const mrrs = scored.map((p) =>
    p.firstGoldRank === null ? 0 : 1 / p.firstGoldRank,
  );
  const distances = scored
    .map((p) => p.distToFirstGold)
    .filter((d): d is number => d !== null);
  return {
    scoredQueries: scored.length,
    recallAt5: scored.length === 0 ? Number.NaN : mean(recall5),
    recallAt10: scored.length === 0 ? Number.NaN : mean(recall10),
    recallAt30: scored.length === 0 ? Number.NaN : mean(recall30),
    precisionAt5: scored.length === 0 ? Number.NaN : mean(precision5),
    precisionAt10: scored.length === 0 ? Number.NaN : mean(precision10),
    meanUniqueSourcesAt5: scored.length === 0 ? Number.NaN : mean(uniqAt5),
    meanUniqueSourcesAt10: scored.length === 0 ? Number.NaN : mean(uniqAt10),
    mrr: scored.length === 0 ? Number.NaN : mean(mrrs),
    meanDistToFirstGold: distances.length === 0 ? null : mean(distances),
  };
}

function recallAtKFromHits(hits: number, goldCount: number): number {
  if (goldCount === 0) return Number.NaN;
  return hits / goldCount;
}

// re-exports for tests
export { recallAtK, reciprocalRank };
