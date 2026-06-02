import { cosineDistance } from "./cosine";

// Walk a pool of retrieval results in input order (caller is responsible
// for sorting by ascending distance to query) and keep a candidate only
// if its pairwise distance to every already-kept item is >= threshold.
// Items whose vector accessor returns null/undefined are skipped.
//
// Generic over the item type so it works on both eval rows (RetrievedItem
// + vectors-by-id map) and prod search rows (which carry their own
// embedding column).
export function dedupByPairwiseCosine<T>(
  items: T[],
  getVector: (item: T) => number[] | null | undefined,
  threshold: number,
): T[] {
  if (threshold <= 0) return items.slice();
  const kept: T[] = [];
  const keptVecs: number[][] = [];
  for (const cand of items) {
    const candVec = getVector(cand);
    if (!candVec) continue;
    let tooClose = false;
    for (const kVec of keptVecs) {
      if (cosineDistance(candVec, kVec) < threshold) {
        tooClose = true;
        break;
      }
    }
    if (!tooClose) {
      kept.push(cand);
      keptVecs.push(candVec);
    }
  }
  return kept;
}

// Sensible default for near-duplicate filtering: catches exact and
// near-exact copies (same WSJ headline reposted to multiple feeds,
// "ОТБОЙ" channel-noise repeated by the same source) without merging
// genuinely-distinct posts on the same topic. Tuned on the RAG eval
// golden set — see packages/mcp/src/eval/configs/baseline-dedup-003.json.
export const DEFAULT_DEDUP_THRESHOLD = 0.03;
