import { describe, expect, it } from "vitest";
import { questionScorer } from "./scorer";

describe("questionScorer — float ground truth", () => {
  it("matches bare numbers", () => {
    expect(questionScorer("90", "90")).toBe(true);
    expect(questionScorer("91", "90")).toBe(false);
  });

  it("strips $, %, and thousands commas from the model answer", () => {
    expect(questionScorer("$100", "100")).toBe(true);
    expect(questionScorer("4.6%", "4.6")).toBe(true);
    expect(questionScorer("1,000", "1000")).toBe(true);
  });

  it("handles signs and decimals", () => {
    expect(questionScorer("+4.6", "4.6")).toBe(true);
    expect(questionScorer("-2.5", "-2.5")).toBe(true);
  });

  it("a non-numeric model answer never matches a numeric GT", () => {
    expect(questionScorer("ninety", "90")).toBe(false);
  });
});

describe("questionScorer — string ground truth", () => {
  it("is whitespace-, case-, and punctuation-insensitive", () => {
    expect(questionScorer("Dr. Martin Luther King Jr.", "Dr Martin Luther King Jr")).toBe(true);
    expect(
      questionScorer("D.R M.A.R.T.I.N L.U.T.H.E.R K.I.N.G J.R", "Dr. Martin Luther King Jr."),
    ).toBe(true);
  });

  it("distinguishes genuinely different strings", () => {
    expect(questionScorer("White", "Smith")).toBe(false);
  });
});

describe("questionScorer — list ground truth", () => {
  it("compares comma-separated lists element-wise, order-sensitive", () => {
    expect(questionScorer("apple, banana", "apple,banana")).toBe(true);
    expect(questionScorer("Apple, Banana", "apple, banana")).toBe(true);
    expect(questionScorer("banana, apple", "apple, banana")).toBe(false);
  });

  it("requires equal length", () => {
    expect(questionScorer("a, b, c", "a, b")).toBe(false);
  });

  it("applies numeric normalization per numeric element", () => {
    expect(questionScorer("1, 2, 3", "1, 2, 3")).toBe(true);
    expect(questionScorer("$1, 2, 3", "1, 2, 3")).toBe(true);
  });

  it("a thousands-separated GT is a list, not a number", () => {
    // is_float("1,000") is false → list branch → length mismatch vs "1000".
    expect(questionScorer("1000", "1,000")).toBe(false);
  });
});
