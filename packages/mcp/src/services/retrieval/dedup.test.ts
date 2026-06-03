import { describe, expect, it } from "vitest";
import { dedupByPairwiseCosine } from "./dedup";

interface Item {
  id: number;
  embedding: number[] | null;
}

const byEmbedding = (item: Item) => item.embedding;

describe("dedupByPairwiseCosine", () => {
  it("returns empty for empty input", () => {
    expect(dedupByPairwiseCosine<Item>([], byEmbedding, 0.05)).toEqual([]);
  });

  it("threshold <= 0 → passthrough (no dedup)", () => {
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: [1, 0, 0] },
    ];
    expect(dedupByPairwiseCosine(items, byEmbedding, 0)).toEqual(items);
  });

  it("drops exact-duplicate vectors", () => {
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: [1, 0, 0] },
      { id: 3, embedding: [0, 1, 0] },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05);
    expect(out.map((x) => x.id)).toEqual([1, 3]);
  });

  it("keeps first-by-input-order when several are near-duplicates", () => {
    const items: Item[] = [
      { id: 10, embedding: [1, 0, 0] },
      { id: 11, embedding: [1, 0.001, 0] },
      { id: 12, embedding: [1, 0.002, 0] },
      { id: 99, embedding: [0, 1, 0] },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05);
    expect(out.map((x) => x.id)).toEqual([10, 99]);
  });

  it("transitive chain: keeps endpoints when ε hits middle", () => {
    // Vectors at 0°, 15°, 30° on unit circle:
    // d(1,2) = d(2,3) = 1 - cos(15°) ≈ 0.034 (close)
    // d(1,3) = 1 - cos(30°) ≈ 0.134 (far)
    const theta = Math.PI / 12;
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: [Math.cos(theta), Math.sin(theta), 0] },
      { id: 3, embedding: [Math.cos(2 * theta), Math.sin(2 * theta), 0] },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05);
    expect(out.map((x) => x.id)).toEqual([1, 3]);
  });

  it("skips items whose vector accessor returns null", () => {
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: null },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05);
    expect(out.map((x) => x.id)).toEqual([1]);
  });

  it("keepNullVectors=true preserves null-vector items at their original position", () => {
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: null },
      { id: 3, embedding: [1, 0, 0] },
      { id: 4, embedding: [0, 1, 0] },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05, {
      keepNullVectors: true,
    });
    expect(out.map((x) => x.id)).toEqual([1, 2, 4]);
  });

  it("preserves input order for surviving items", () => {
    const items: Item[] = [
      { id: 1, embedding: [1, 0, 0] },
      { id: 2, embedding: [0, 1, 0] },
      { id: 3, embedding: [0, 0, 1] },
    ];
    const out = dedupByPairwiseCosine(items, byEmbedding, 0.05);
    expect(out.map((x) => x.id)).toEqual([1, 2, 3]);
  });

  it("accessor can resolve vectors from external source", () => {
    const vecs = new Map<number, number[]>([
      [1, [1, 0, 0]],
      [2, [1, 0, 0]],
      [3, [0, 1, 0]],
    ]);
    const items = [{ id: 1 }, { id: 2 }, { id: 3 }];
    const out = dedupByPairwiseCosine(
      items,
      (x) => vecs.get(x.id) ?? null,
      0.05,
    );
    expect(out.map((x) => x.id)).toEqual([1, 3]);
  });
});
