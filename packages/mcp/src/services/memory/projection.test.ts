import { describe, expect, it } from "vitest";
import { buildIndexText, chunkMarkdown, DEFAULT_RANK, rankHits } from "./projection";
import type { IndexHit } from "./types";

describe("chunkMarkdown", () => {
  it("starts a new chunk at every heading and records the breadcrumb", () => {
    const body = "# Project\n\nIntro line.\n\n## Progress\n\nDay 1\n\n### Notes\n\nWatch out\n";

    expect(chunkMarkdown(body)).toEqual([
      { text: "Intro line.", headingPath: "Project" },
      { text: "Day 1", headingPath: "Project > Progress" },
      { text: "Watch out", headingPath: "Project > Progress > Notes" },
    ]);
  });

  it("pops back out of a subsection when a sibling heading arrives", () => {
    const body = "## A\n\na\n\n### A1\n\na1\n\n## B\n\nb\n";

    expect(chunkMarkdown(body).map((c) => c.headingPath)).toEqual(["A", "A > A1", "B"]);
  });

  it("packs several small paragraphs into one chunk", () => {
    const body = "## Log\n\none\n\ntwo\n\nthree\n";

    expect(chunkMarkdown(body, 100)).toEqual([{ text: "one\n\ntwo\n\nthree", headingPath: "Log" }]);
  });

  // A month of daily entries must not collapse into a single vector.
  it("splits once the packed paragraphs exceed the budget", () => {
    const body = ["## Log", "a".repeat(60), "b".repeat(60), "c".repeat(60)].join("\n\n");

    const chunks = chunkMarkdown(body, 100);

    expect(chunks).toHaveLength(3);
    expect(chunks.every((c) => c.text.length <= 100)).toBe(true);
    expect(chunks.every((c) => c.headingPath === "Log")).toBe(true);
  });

  it("hard-splits a single oversized paragraph so it still fits the embedder", () => {
    const chunks = chunkMarkdown("x".repeat(250), 100);

    expect(chunks.map((c) => c.text.length)).toEqual([100, 100, 50]);
  });

  it("keeps text that appears before any heading", () => {
    expect(chunkMarkdown("loose note\n")).toEqual([{ text: "loose note", headingPath: "" }]);
  });

  it("produces nothing for an empty or heading-only document", () => {
    expect(chunkMarkdown("")).toEqual([]);
    expect(chunkMarkdown("# Title\n")).toEqual([]);
  });
});

describe("buildIndexText", () => {
  // The chunk has to name its own subject or recall can never match it.
  it("prefixes the project and document, plus the heading path when there is one", () => {
    expect(
      buildIndexText({
        projectTitle: "Графы для интервью",
        docName: "progress.md",
        headingPath: "Прогресс",
        text: "застрял на Dijkstra",
      }),
    ).toBe("Графы для интервью — progress.md · Прогресс\n\nзастрял на Dijkstra");
  });

  it("omits the separator when the chunk sits under no heading", () => {
    expect(
      buildIndexText({ projectTitle: "P", docName: "d.md", headingPath: "", text: "body" }),
    ).toBe("P — d.md\n\nbody");
  });
});

describe("rankHits", () => {
  const now = new Date("2026-08-23T12:00:00Z");
  const hit = (ref: string, distance: number, daysAgo: number): IndexHit => ({
    id: Number(ref.replace(/\D/g, "")) || 1,
    ref,
    text: ref,
    tags: [],
    actor: null,
    state: "active",
    ts: new Date(now.getTime() - daysAgo * 86_400_000).toISOString(),
    distance,
  });

  it("prefers the fresher row when relevance ties", () => {
    const ranked = rankHits([hit("fact:1", 0.3, 400), hit("fact:2", 0.3, 0)], { now, ...DEFAULT_RANK });

    expect(ranked.map((r) => r.ref)).toEqual(["fact:2", "fact:1"]);
  });

  // Recency breaks ties; it must not float an irrelevant row over a relevant
  // one, or "what do we have on X" starts answering with today's shopping list.
  it("does not let recency outweigh a clearly better match", () => {
    const ranked = rankHits([hit("fact:1", 0.10, 400), hit("fact:2", 0.40, 0)], { now, ...DEFAULT_RANK });

    expect(ranked.map((r) => r.ref)).toEqual(["fact:1", "fact:2"]);
  });

  it("caps the boost at the configured weight for a row written just now", () => {
    const [ranked] = rankHits([hit("fact:1", 0.5, 0)], { now, ...DEFAULT_RANK });

    expect(ranked?.score).toBeCloseTo(0.5 - DEFAULT_RANK.recencyWeight, 10);
  });

  it("halves the boost after one half-life", () => {
    const [ranked] = rankHits([hit("fact:1", 0.5, DEFAULT_RANK.halfLifeDays)], { now, ...DEFAULT_RANK });

    expect(ranked?.score).toBeCloseTo(0.5 - DEFAULT_RANK.recencyWeight / 2, 10);
  });

  it("treats a future timestamp as brand new rather than boosting it further", () => {
    const [ranked] = rankHits([hit("fact:1", 0.5, -10)], { now, ...DEFAULT_RANK });

    expect(ranked?.score).toBeCloseTo(0.5 - DEFAULT_RANK.recencyWeight, 10);
  });
});
