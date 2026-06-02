import type { RetrievedItem } from "./types";

export function recallAtK(
  gold: number[],
  topK: RetrievedItem[],
  k: number,
): number {
  if (gold.length === 0) return Number.NaN;
  const goldSet = new Set(gold);
  const slice = topK.slice(0, k);
  let hit = 0;
  for (const item of slice) {
    if (goldSet.has(item.id)) hit++;
  }
  return hit / gold.length;
}

// Reciprocal rank of the first gold hit in topK. 0 if none of topK is gold.
export function reciprocalRank(gold: number[], topK: RetrievedItem[]): number {
  const rank = firstGoldRank(gold, topK);
  return rank === null ? 0 : 1 / rank;
}

// 1-indexed rank of the first gold in topK, or null if none present.
export function firstGoldRank(
  gold: number[],
  topK: RetrievedItem[],
): number | null {
  const goldSet = new Set(gold);
  for (let i = 0; i < topK.length; i++) {
    const item = topK[i];
    if (item && goldSet.has(item.id)) return i + 1;
  }
  return null;
}

export function distanceToFirstGold(
  gold: number[],
  topK: RetrievedItem[],
): number | null {
  const goldSet = new Set(gold);
  for (const item of topK) {
    if (goldSet.has(item.id)) return item.distance;
  }
  return null;
}

export function mean(values: number[]): number {
  if (values.length === 0) return Number.NaN;
  let sum = 0;
  for (const v of values) sum += v;
  return sum / values.length;
}
