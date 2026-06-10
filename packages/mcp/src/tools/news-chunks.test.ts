import { describe, expect, it } from "vitest";
import { splitChunks } from "./news";

describe("splitChunks", () => {
  it("splits contiguously into exactly n parts, sizes differ by at most 1", () => {
    const items = Array.from({ length: 10 }, (_, i) => i);
    const chunks = splitChunks(items, 3);
    expect(chunks).toEqual([
      [0, 1, 2, 3],
      [4, 5, 6],
      [7, 8, 9],
    ]);
  });

  it("always returns n arrays, padding with empty ones when items < n", () => {
    expect(splitChunks([1, 2], 4)).toEqual([[1], [2], [], []]);
    expect(splitChunks([], 3)).toEqual([[], [], []]);
  });

  it("concatenation of chunks reproduces the input order", () => {
    const items = Array.from({ length: 23 }, (_, i) => `p${i}`);
    expect(splitChunks(items, 5).flat()).toEqual(items);
  });
});
