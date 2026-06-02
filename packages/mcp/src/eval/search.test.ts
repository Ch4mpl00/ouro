import { describe, expect, it } from "vitest";
import { cosineDistance, cosineTopK } from "./search";

const unit = (x: number, y: number): number[] => {
  const norm = Math.hypot(x, y);
  return [x / norm, y / norm];
};

describe("cosineDistance", () => {
  it("returns 0 for identical unit vectors", () => {
    const v = unit(1, 1);
    expect(cosineDistance(v, v)).toBeCloseTo(0);
  });

  it("returns 1 for orthogonal unit vectors", () => {
    expect(cosineDistance(unit(1, 0), unit(0, 1))).toBeCloseTo(1);
  });

  it("returns 2 for antipodal unit vectors", () => {
    expect(cosineDistance(unit(1, 0), unit(-1, 0))).toBeCloseTo(2);
  });
});

describe("cosineTopK", () => {
  it("returns top-K sorted by ascending distance", () => {
    const q = unit(1, 0);
    const corpus = [
      { id: 1, embedding: unit(0, 1) },
      { id: 2, embedding: unit(1, 0.01) },
      { id: 3, embedding: unit(-1, 0) },
      { id: 4, embedding: unit(0.9, 0.1) },
    ];
    const result = cosineTopK(q, corpus, 2);
    expect(result.map((r) => r.id)).toEqual([2, 4]);
    expect(result.map((r) => r.distance)).toEqual([...result.map((r) => r.distance)].sort((a, b) => a - b));
  });

  it("returns all items when K exceeds corpus size", () => {
    const q = unit(1, 0);
    const corpus = [
      { id: 1, embedding: unit(1, 0) },
      { id: 2, embedding: unit(0, 1) },
    ];
    expect(cosineTopK(q, corpus, 10)).toHaveLength(2);
  });
});
