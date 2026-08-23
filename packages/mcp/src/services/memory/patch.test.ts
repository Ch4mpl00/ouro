import { describe, expect, it } from "vitest";
import { appendToBody, applyEdits, findNearMatches, invertEdits, listHeadings } from "./patch";

const ROADMAP = `# Roadmap

- [ ] BFS
- [ ] Dijkstra
- [ ] A*
`;

describe("applyEdits", () => {
  it("replaces a unique literal and leaves the rest byte-identical", () => {
    const result = applyEdits(ROADMAP, [{ old: "- [ ] Dijkstra", new: "- [x] Dijkstra" }]);

    expect(result).toEqual({ ok: true, body: "# Roadmap\n\n- [ ] BFS\n- [x] Dijkstra\n- [ ] A*\n" });
  });

  it("treats an empty `new` as a deletion rather than needing a separate op", () => {
    const result = applyEdits(ROADMAP, [{ old: "- [ ] A*\n", new: "" }]);

    expect(result).toEqual({ ok: true, body: "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n" });
  });

  // "Take the first match" is how an agent edits the wrong line and reports
  // success. The count tells it how much more context to quote.
  it("refuses an ambiguous literal and reports the occurrence count", () => {
    const body = "- [ ] review\n- [ ] review\n- [ ] review\n";

    const result = applyEdits(body, [{ old: "- [ ] review", new: "- [x] review" }]);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("expected failure");
    expect(result.failures).toEqual([
      { index: 0, old: "- [ ] review", reason: "ambiguous", occurrences: 3, suggestions: [] },
    ]);
  });

  it("applies nothing at all when a later edit fails", () => {
    const result = applyEdits(ROADMAP, [
      { old: "- [ ] BFS", new: "- [x] BFS" },
      { old: "- [ ] Floyd", new: "- [x] Floyd" },
    ]);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("expected failure");
    // The first edit succeeded in the working copy but must not be returned:
    // atomicity is the whole reason a failed patch is safe to retry.
    expect(result.failures.map((f) => f.index)).toEqual([1]);
  });

  it("lets a later edit target text an earlier one produced", () => {
    const result = applyEdits(ROADMAP, [
      { old: "- [ ] A*", new: "- [ ] A*\n- [ ] Bellman-Ford" },
      { old: "- [ ] Bellman-Ford", new: "- [x] Bellman-Ford" },
    ]);

    expect(result).toEqual({
      ok: true,
      body: "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n- [x] Bellman-Ford\n",
    });
  });

  it("rejects an empty `old` instead of matching everywhere", () => {
    const result = applyEdits(ROADMAP, [{ old: "", new: "anything" }]);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("expected failure");
    expect(result.failures[0]).toMatchObject({ reason: "empty" });
  });

  it("rejects an empty edit list", () => {
    expect(applyEdits(ROADMAP, []).ok).toBe(false);
  });

  it("reports every failing edit at once, not just the first", () => {
    const result = applyEdits(ROADMAP, [
      { old: "nope one", new: "x" },
      { old: "nope two", new: "y" },
    ]);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("expected failure");
    expect(result.failures.map((f) => f.index)).toEqual([0, 1]);
  });
});

// The realistic miss: the model retyped the text it had just read, and the
// retyping normalised something. The error must hand back the literal that
// would have worked.
describe("findNearMatches", () => {
  it("recovers the original when the model straightened an em-dash", () => {
    const body = "Цель — пройти графы за месяц.\n";

    expect(findNearMatches(body, "Цель - пройти графы за месяц.")).toEqual([
      "Цель — пройти графы за месяц.",
    ]);
  });

  it("recovers the original when the model wrote е for ё", () => {
    const body = "- [ ] Обойдём граф в ширину\n";

    expect(findNearMatches(body, "Обойдем граф в ширину")).toEqual(["Обойдём граф в ширину"]);
  });

  it("recovers the original across reflowed whitespace", () => {
    const body = "Плана\n    нет\n";

    expect(findNearMatches(body, "Плана нет")).toEqual(["Плана\n    нет"]);
  });

  it("recovers the original when the model straightened «quotes»", () => {
    const body = 'Проект «Графы» стартовал.\n';

    expect(findNearMatches(body, 'Проект "Графы" стартовал.')).toEqual([
      "Проект «Графы» стартовал.",
    ]);
  });

  it("falls back to the most similar line when nothing matches even loosely", () => {
    const body = "# Roadmap\n\n- [ ] Dijkstra shortest path\n- [ ] Unrelated topic\n";

    expect(findNearMatches(body, "- [ ] Dijkstra shortest paths")).toEqual([
      "- [ ] Dijkstra shortest path",
    ]);
  });

  it("returns nothing rather than a wild guess", () => {
    expect(findNearMatches("# Roadmap\n\n- [ ] BFS\n", "completely unrelated sentence")).toEqual([]);
  });

  it("surfaces near-matches through applyEdits so the agent can retry", () => {
    const result = applyEdits("Цель — пройти графы.\n", [
      { old: "Цель - пройти графы.", new: "Цель — пройти графы за месяц." },
    ]);

    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("expected failure");
    expect(result.failures[0]).toMatchObject({
      reason: "not_found",
      suggestions: ["Цель — пройти графы."],
    });
  });
});

describe("invertEdits", () => {
  it("reverses order and swaps the sides", () => {
    expect(
      invertEdits([
        { old: "a", new: "b" },
        { old: "c", new: "d" },
      ]),
    ).toEqual([
      { old: "d", new: "c" },
      { old: "b", new: "a" },
    ]);
  });

  // Re-inserting deleted text needs a position, and the anchor it hung on is
  // gone. Callers fall back to a whole-document rollback instead of guessing.
  it("has no inverse for a deletion", () => {
    expect(invertEdits([{ old: "gone", new: "" }])).toBeNull();
  });
});

describe("appendToBody", () => {
  it("appends at the end with exactly one blank line and one trailing newline", () => {
    expect(appendToBody("# Progress\n\nDay 1: BFS\n", "Day 2: Dijkstra")).toEqual({
      ok: true,
      body: "# Progress\n\nDay 1: BFS\n\nDay 2: Dijkstra\n",
    });
  });

  it("normalises sloppy trailing whitespace instead of compounding it", () => {
    expect(appendToBody("# Progress\n\n\n\n", "Day 1")).toEqual({
      ok: true,
      body: "# Progress\n\nDay 1\n",
    });
  });

  it("writes into an empty document", () => {
    expect(appendToBody("", "first note")).toEqual({ ok: true, body: "first note\n" });
  });

  it("appends at the end of the named section, not the end of the file", () => {
    const body = "# Doc\n\n## Progress\n\nDay 1\n\n## Mistakes\n\nForgot visited set\n";

    expect(appendToBody(body, "Day 2", "Progress")).toEqual({
      ok: true,
      body: "# Doc\n\n## Progress\n\nDay 1\n\nDay 2\n\n## Mistakes\n\nForgot visited set\n",
    });
  });

  it("accepts the heading with or without its hashes", () => {
    const body = "## Progress\n\nDay 1\n\n## Later\n\nx\n";

    expect(appendToBody(body, "Day 2", "## Progress")).toEqual(appendToBody(body, "Day 2", "Progress"));
  });

  it("keeps a deeper sub-heading inside the section it belongs to", () => {
    const body = "## Progress\n\nDay 1\n\n### Notes\n\nn\n\n## Mistakes\n\nm\n";

    const result = appendToBody(body, "Day 2", "Progress");

    expect(result).toEqual({
      ok: true,
      body: "## Progress\n\nDay 1\n\n### Notes\n\nn\n\nDay 2\n\n## Mistakes\n\nm\n",
    });
  });

  it("fails with the available headings rather than appending to the wrong place", () => {
    const body = "## Progress\n\nDay 1\n\n## Mistakes\n\nm\n";

    expect(appendToBody(body, "Day 2", "Roadmap")).toEqual({
      ok: false,
      reason: "heading_not_found",
      headings: ["## Progress", "## Mistakes"],
    });
  });
});

describe("listHeadings", () => {
  it("reads level and text, ignoring non-headings", () => {
    expect(listHeadings("# A\n\ntext\n\n### B\n#not a heading\n")).toEqual([
      { level: 1, text: "A", line: 0 },
      { level: 3, text: "B", line: 4 },
    ]);
  });
});
