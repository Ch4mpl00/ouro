import { describe, expect, it } from "vitest";
import { appendPatch, patchMessages, PATCH_MARKER, type ChatMessage } from "./patch";
import { gradeAxis } from "./gate";

describe("appendPatch", () => {
  it("appends the patch after the body with a marker", () => {
    const out = appendPatch("BODY", "extra rule");
    expect(out).toBe(`BODY\n\n${PATCH_MARKER}\nextra rule\n`);
  });

  it("is a no-op for an empty / whitespace patch", () => {
    expect(appendPatch("BODY", "")).toBe("BODY");
    expect(appendPatch("BODY", "   \n ")).toBe("BODY");
  });

  it("trims trailing whitespace on the body so the marker lands cleanly", () => {
    expect(appendPatch("BODY\n\n", "p")).toBe(`BODY\n\n${PATCH_MARKER}\np\n`);
  });
});

describe("patchMessages", () => {
  it("appends to the FIRST system message only, leaving the rest untouched", () => {
    const msgs: ChatMessage[] = [
      { role: "system", content: "CONTRACT" },
      { role: "user", content: "candidates" },
    ];
    const out = patchMessages(msgs, "be terse");
    expect(out[0]!.content).toBe(`CONTRACT\n\n${PATCH_MARKER}\nbe terse\n`);
    expect(out[1]).toEqual({ role: "user", content: "candidates" });
    // original array is not mutated
    expect(msgs[0]!.content).toBe("CONTRACT");
  });

  it("stringifies non-string system content before appending", () => {
    const msgs: ChatMessage[] = [{ role: "system", content: { a: 1 } }];
    const out = patchMessages(msgs, "p");
    expect(out[0]!.content).toBe(`{"a":1}\n\n${PATCH_MARKER}\np\n`);
  });

  it("throws when there is no system message", () => {
    expect(() => patchMessages([{ role: "user", content: "x" }], "p")).toThrow(/no system message/);
  });
});

describe("gradeAxis", () => {
  const K = 2;

  it("calls a clear lift above k·σ an improvement", () => {
    const g = gradeAxis("composition", [0.6, 0.6], [0.9, 0.9], 0.02, K);
    expect(g.delta).toBeCloseTo(0.3, 4);
    expect(g.threshold).toBeCloseTo(0.04, 4);
    expect(g.verdict).toBe("improve");
  });

  it("calls a drop below -k·σ a regression", () => {
    const g = gradeAxis("composition", [0.9, 0.9], [0.6, 0.6], 0.02, K);
    expect(g.verdict).toBe("regress");
  });

  it("treats a Δ within k·σ as noise", () => {
    const g = gradeAxis("process", [0.70, 0.72], [0.73, 0.71], 0.05, K);
    // |Δ| = 0.01 < 0.10 threshold
    expect(g.verdict).toBe("noise");
  });

  it("abstains when no σ baseline is available", () => {
    const g = gradeAxis("composition", [0.6], [0.9], null, K);
    expect(g.verdict).toBe("no-baseline");
    expect(g.delta).toBeCloseTo(0.3, 4);
    expect(g.threshold).toBeNull();
  });

  it("is n/a when either side has no numeric samples", () => {
    expect(gradeAxis("coverage", [], [0.5], 0.05, K).verdict).toBe("n/a");
    expect(gradeAxis("coverage", [0.5], [], 0.05, K).verdict).toBe("n/a");
  });
});
