export type BuildTextMode = "title+body" | "body-only";

export interface EvalConfig {
  name: string;
  retrieval: {
    embed: { model: string; dimensions: number };
    buildText: BuildTextMode;
    topK: number;
    rerank: null;
  };
  query: { field: "query" | "reformulation" };
  scoring: { mode: "binary" };
}

export interface CorpusRow {
  id: number;
  source: string;
  title: string | null;
  body: string;
  postedAt: string | null;
}

export interface QueryRow {
  id: string;
  query: string;
  reformulation: string;
  gold: number[];
  acceptable: number[];
}

export interface RetrievedItem {
  id: number;
  distance: number;
}

export interface PerQueryResult {
  qid: string;
  query: string;
  goldCount: number;
  acceptableCount: number;
  topK: RetrievedItem[];
  hitAt5: number;
  hitAt10: number;
  precisionAt5: number;
  precisionAt10: number;
  firstGoldRank: number | null;
  distToFirstGold: number | null;
}

export interface NegativeTestResult {
  qid: string;
  query: string;
  minDistance: number;
  top1Id: number;
}

export interface AggregateMetrics {
  scoredQueries: number;
  recallAt5: number;
  recallAt10: number;
  precisionAt5: number;
  precisionAt10: number;
  mrr: number;
  meanDistToFirstGold: number | null;
}

export interface EvalResult {
  config: EvalConfig;
  configHash: string;
  perQuery: PerQueryResult[];
  negativeTests: NegativeTestResult[];
  aggregate: AggregateMetrics;
  cacheHit: boolean;
}
