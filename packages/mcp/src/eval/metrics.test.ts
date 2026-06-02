import { describe, expect, it } from "vitest";
import {
  distanceToFirstGold,
  firstGoldRank,
  mean,
  recallAtK,
  reciprocalRank,
} from "./metrics";

const topK = (...ids: number[]) =>
  ids.map((id, i) => ({ id, distance: 0.1 * (i + 1) }));

describe("recallAtK", () => {
  it("returns 1.0 when all gold present in top-K", () => {
    expect(recallAtK([1, 2, 3], topK(1, 2, 3, 99, 100), 5)).toBe(1);
  });

  it("returns partial recall", () => {
    expect(recallAtK([1, 2, 3], topK(1, 99, 2, 99, 99), 5)).toBeCloseTo(2 / 3);
  });

  it("returns 0 when no gold in top-K", () => {
    expect(recallAtK([1, 2, 3], topK(7, 8, 9), 5)).toBe(0);
  });

  it("respects k bound", () => {
    expect(recallAtK([1, 2, 3], topK(99, 99, 99, 1, 2), 3)).toBe(0);
    expect(recallAtK([1, 2, 3], topK(99, 99, 99, 1, 2), 5)).toBeCloseTo(2 / 3);
  });

  it("returns NaN for empty gold", () => {
    expect(recallAtK([], topK(1, 2, 3), 5)).toBeNaN();
  });
});

describe("firstGoldRank / reciprocalRank", () => {
  it("returns 1-indexed rank", () => {
    expect(firstGoldRank([7], topK(1, 2, 7, 4))).toBe(3);
    expect(reciprocalRank([7], topK(1, 2, 7, 4))).toBeCloseTo(1 / 3);
  });

  it("returns null/0 when no gold in topK", () => {
    expect(firstGoldRank([99], topK(1, 2, 3))).toBeNull();
    expect(reciprocalRank([99], topK(1, 2, 3))).toBe(0);
  });

  it("rank=1 → MRR=1", () => {
    expect(reciprocalRank([1], topK(1, 2, 3))).toBe(1);
  });
});

describe("distanceToFirstGold", () => {
  it("returns the distance of the first gold hit", () => {
    expect(distanceToFirstGold([3], topK(1, 2, 3, 4))).toBeCloseTo(0.3);
  });

  it("returns null when no gold present", () => {
    expect(distanceToFirstGold([99], topK(1, 2, 3))).toBeNull();
  });
});

describe("mean", () => {
  it("averages values", () => {
    expect(mean([1, 2, 3, 4])).toBe(2.5);
  });

  it("returns NaN for empty input", () => {
    expect(mean([])).toBeNaN();
  });
});
