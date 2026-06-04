import { describe, expect, it } from "vitest";
import { DEFAULT_DEDUP_THRESHOLD } from "../../retrieval";
import { mergeRankedPools, type RankablePoolRow } from "./repository";

// Unit-test the pure merge/rank/dedup contract of the batch search path
// (searchMany). The DB query + embed are I/O and live in searchCore;
// filters are applied SQL-side, so they're out of scope here — what
// matters is how per-query pools are merged, ranked and de-duplicated.

const unit = (...xs: number[]): number[] => {
  const norm = Math.hypot(...xs);
  return xs.map((x) => x / norm);
};

const row = (
  id: number,
  distance: number,
  embedding: number[],
  extra: Partial<RankablePoolRow> = {},
): RankablePoolRow => ({
  id,
  source: "hackernews",
  title: `t${id}`,
  url: null,
  body: "body",
  metadata: {},
  postedAt: null,
  distance,
  embedding,
  ...extra,
});

// Far-apart unit vectors (pairwise distance ~1) so dedup never collapses
// them; the only collapsing in these fixtures is opt-in via `near`.
const eX = unit(1, 0, 0);
const eY = unit(0, 1, 0);
const eZ = unit(0, 0, 1);

const DEF = { k: 10, threshold: DEFAULT_DEDUP_THRESHOLD, annotateMatches: true };

describe("mergeRankedPools", () => {
  it("keeps an item retrieved by several queries once, at its min distance", () => {
    const result = mergeRankedPools(
      [
        [row(1, 0.2, eX), row(2, 0.5, eY)],
        [row(1, 0.1, eX), row(3, 0.4, eZ)],
      ],
      DEF,
    );
    expect(result.map((r) => r.id)).toEqual([1, 3, 2]);
    const one = result.find((r) => r.id === 1);
    expect(one?.distance).toBeCloseTo(0.1);
    expect(one?.matchedQueries).toEqual([0, 1]);
  });

  it("collapses cross-query near-duplicates, keeping the closer row", () => {
    const near1 = unit(1, 0, 0);
    const near2 = unit(1, 0.1, 0); // cosine distance to near1 ≈ 0.005 < threshold
    const result = mergeRankedPools(
      [
        [row(1, 0.2, near1)],
        [row(2, 0.15, near2)],
      ],
      DEF,
    );
    expect(result.map((r) => r.id)).toEqual([2]);
  });

  it("applies k as the post-dedup cap", () => {
    const result = mergeRankedPools(
      [
        [row(1, 0.5, eX), row(2, 0.1, eY)],
        [row(3, 0.3, eZ), row(4, 0.2, unit(1, 1, 0)), row(5, 0.9, unit(0, 1, 1))],
      ],
      { ...DEF, k: 2 },
    );
    expect(result.map((r) => r.id)).toEqual([2, 4]);
  });

  it("disables dedup when threshold is 0 (near-duplicates both survive)", () => {
    const near1 = unit(1, 0, 0);
    const near2 = unit(1, 0.1, 0);
    const result = mergeRankedPools(
      [[row(1, 0.2, near1)], [row(2, 0.15, near2)]],
      { ...DEF, threshold: 0 },
    );
    expect(result.map((r) => r.id)).toEqual([2, 1]);
  });

  it("handles the single-query (N=1) case and omits matchedQueries when not annotating", () => {
    const result = mergeRankedPools(
      [[row(1, 0.3, eX), row(2, 0.1, eY)]],
      { ...DEF, annotateMatches: false },
    );
    expect(result.map((r) => r.id)).toEqual([2, 1]);
    expect(result.every((r) => !("matchedQueries" in r))).toBe(true);
  });
});
