import { describe, expect, it } from "vitest";
import { budgetExceeded, countLessons, decideRevert, removeLesson, splitLessons } from "./monitor";

describe("decideRevert", () => {
  const baseline = { mean: 0.7, n: 10 };
  const K = 2;
  const MIN = 5;

  it("is insufficient until enough post-ship traces accumulate", () => {
    const v = decideRevert(baseline, [0.1, 0.1, 0.1], 0.05, K, MIN); // n=3 < 5
    expect(v.decision).toBe("insufficient");
    expect(v.postN).toBe(3);
  });

  it("keeps a ship whose live trend holds at/above baseline", () => {
    const v = decideRevert(baseline, [0.72, 0.71, 0.73, 0.7, 0.74], 0.05, K, MIN);
    expect(v.decision).toBe("keep");
  });

  it("keeps a flat trend (a small dip within the noise band is not a regression)", () => {
    // post mean ≈ 0.69; drop 0.01 well under 2·0.05·√(1/5+1/10) ≈ 0.055
    const v = decideRevert(baseline, [0.69, 0.69, 0.69, 0.69, 0.69], 0.05, K, MIN);
    expect(v.decision).toBe("keep");
    expect(v.margin).toBeCloseTo(2 * 0.05 * Math.sqrt(1 / 5 + 1 / 10), 3);
  });

  it("reverts a ship that confidently fell below baseline", () => {
    const v = decideRevert(baseline, [0.4, 0.45, 0.42, 0.38, 0.41], 0.05, K, MIN);
    expect(v.decision).toBe("revert");
  });

  it("with no σ baseline reverts on any below-baseline mean (margin 0)", () => {
    expect(decideRevert(baseline, [0.69, 0.69, 0.69, 0.69, 0.69], null, K, MIN).decision).toBe("revert");
    expect(decideRevert(baseline, [0.7, 0.7, 0.7, 0.7, 0.7], null, K, MIN).decision).toBe("keep");
  });
});

describe("patch lessons", () => {
  const two = "- lesson one\n  with a second bullet\n\n- lesson two";

  it("splits append-only blocks on the blank line, ignoring empties", () => {
    expect(splitLessons("")).toEqual([]);
    expect(splitLessons("   \n  ")).toEqual([]);
    expect(countLessons(two)).toBe(2);
    expect(splitLessons(two)[0]).toBe("- lesson one\n  with a second bullet");
  });

  it("budget is exceeded once the count reaches the cap", () => {
    expect(budgetExceeded(two, 3)).toBe(false);
    expect(budgetExceeded(two, 2)).toBe(true);
  });

  it("removeLesson drops one block and keeps the rest", () => {
    expect(removeLesson(two, "- lesson one\n  with a second bullet")).toBe("- lesson two\n");
  });

  it("removeLesson returns empty when the removed lesson was the only one", () => {
    expect(removeLesson("- only lesson\n", "- only lesson")).toBe("");
  });

  it("removeLesson is a no-op when the lesson isn't present", () => {
    expect(removeLesson(two, "- not here")).toBe("- lesson one\n  with a second bullet\n\n- lesson two\n");
  });
});
