import { describe, expect, it } from "vitest";
import {
  ROOT_SKIP,
  assembleNodeMaterials,
  budgetExceeded,
  classify,
  collectNodeNoise,
  countLessons,
  decideRevert,
  decideShip,
  dominantMode,
  extractAxisScores,
  gradeAxis,
  judgeNode,
  patchMessages,
  percentile,
  removeLesson,
  sampleStdDev,
  selectCandidates,
  splitLessons,
  summarizeNoise,
  type AxisGrade,
  type ChatMessage,
  type JudgeBackend,
  type JudgeNodeInput,
  type NodeGateResult,
  type NodeJudgement,
  type NodeNoise,
  type NoiseAxis,
  type Taxonomy,
  type TraceSource,
} from "./judging";
import { appendPatch, PATCH_MARKER } from "./agent-loop";
import type { JudgementRecord } from "./db";
import {
  JUDGE_NODE_META,
  type Observation,
  type TraceRecord,
} from "./tracing";

// ─── gate ───────────────────────────────────────────

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

  it("calls a clear lift above k·σ·√(1+1/S) an improvement", () => {
    // before = single stored score; after = S=2 fresh judgements.
    const g = gradeAxis("composition", 0.6, [0.9, 0.9], 0.02, K);
    expect(g.delta).toBeCloseTo(0.3, 4);
    // 2·0.02·√(1.5) ≈ 0.049
    expect(g.threshold).toBeCloseTo(0.049, 3);
    expect(g.beforeN).toBe(1);
    expect(g.verdict).toBe("improve");
  });

  it("calls a drop below -threshold a regression", () => {
    const g = gradeAxis("composition", 0.9, [0.6, 0.6], 0.02, K);
    expect(g.verdict).toBe("regress");
  });

  it("treats a Δ within the threshold as noise", () => {
    const g = gradeAxis("process", 0.71, [0.73, 0.71], 0.05, K);
    // |Δ| = 0.01 < 0.12 threshold
    expect(g.verdict).toBe("noise");
  });

  it("abstains when no σ baseline is available", () => {
    const g = gradeAxis("composition", 0.6, [0.9], null, K);
    expect(g.verdict).toBe("no-baseline");
    expect(g.delta).toBeCloseTo(0.3, 4);
    expect(g.threshold).toBeNull();
  });

  it("is n/a when the before score or the after samples are missing", () => {
    expect(gradeAxis("coverage", null, [0.5], 0.05, K).verdict).toBe("n/a");
    expect(gradeAxis("coverage", 0.5, [], 0.05, K).verdict).toBe("n/a");
  });

  it("widens the threshold as S shrinks (single-sample holdout)", () => {
    const s1 = gradeAxis("coverage", 0.5, [0.6], 0.05, K).threshold!; // S=1 → √2
    const s2 = gradeAxis("coverage", 0.5, [0.6, 0.6], 0.05, K).threshold!; // S=2 → √1.5
    expect(s1).toBeGreaterThan(s2);
    expect(s1).toBeCloseTo(2 * 0.05 * Math.sqrt(2), 3);
  });
});

// ─── improver ───────────────────────────────────────────

function rec(id: string, coverage: number | null, startedAt = "2026-06-18T00:00:00.000Z"): JudgementRecord {
  return {
    traceId: `t-${id}`,
    observationId: `o-${id}`,
    nodeKind: "compose",
    skill: "news-digest",
    scores: {
      query_formulation: null,
      process: null,
      coverage,
      composition: null,
      faithfulness: null,
    },
    detail: {},
    startedAt,
  };
}

// Defaults mirroring the locked design: candidate iff score < 0.6 AND
// score < 0.75 − k·σ; holdout = all-time score ≥ 0.85.
const OPTS = { holdoutSize: 5, absMax: 0.6, bar: 0.75, k: 2, holdoutMin: 0.85, recentSince: null as string | null };

describe("selectCandidates", () => {
  it("selects absolute+σ failures (asc) and an all-time high holdout (desc), no overlap", () => {
    const records = [rec("a", 0.2), rec("b", 0.5), rec("c", 0.9), rec("d", 0.95), rec("e", 0.4)];
    const { candidates, holdout } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    // sigmaFloor = 0.75 − 0.1 = 0.65; absMax 0.6 dominates → s < 0.6
    expect(candidates.map((r) => r.observationId)).toEqual(["o-a", "o-e", "o-b"]); // 0.2,0.4,0.5
    expect(holdout.map((r) => r.observationId)).toEqual(["o-d", "o-c"]); // 0.95, 0.9
  });

  it("the σ term excludes near-bar lows on a noisy axis (not just judge wobble)", () => {
    const records = [rec("a", 0.4), rec("b", 0.5)];
    // sigma 0.15 → sigmaFloor = 0.75 − 0.3 = 0.45; 0.5 fails absMax but not the σ floor.
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.15 });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-a"]);
  });

  it("draws the cluster from the recent window but the holdout from all time", () => {
    const records = [
      rec("old-low", 0.3, "2026-01-01T00:00:00.000Z"),
      rec("new-low", 0.3, "2026-06-17T00:00:00.000Z"),
      rec("old-high", 0.9, "2026-01-01T00:00:00.000Z"),
    ];
    const { candidates, holdout } = selectCandidates(records, "coverage", {
      ...OPTS,
      sigma: 0.05,
      recentSince: "2026-06-01T00:00:00.000Z",
    });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-new-low"]); // old low dropped
    expect(holdout.map((r) => r.observationId)).toEqual(["o-old-high"]); // height, not recency
  });

  it("ignores nodes where the axis is null (rubric didn't emit it / n/a)", () => {
    const records = [rec("a", null), rec("b", 0.3), rec("c", null)];
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-b"]);
  });

  it("returns no candidates when the floor has reached the ceiling (all good)", () => {
    const records = [rec("a", 0.8), rec("b", 0.9)];
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    expect(candidates).toEqual([]);
  });
});

describe("dominantMode", () => {
  const taxonomy: Taxonomy = {
    modes: [
      { name: "omits-item", description: "omits a concrete item", nodeIds: ["t-a:o-a", "t-b:o-b"] },
      { name: "off-contract", description: "includes off-contract opinion", nodeIds: ["t-c:o-c"] },
    ],
  };

  it("picks the Pareto-dominant mode (most valid members)", () => {
    const valid = new Set(["t-a:o-a", "t-b:o-b", "t-c:o-c"]);
    expect(dominantMode(taxonomy, valid)?.name).toBe("omits-item");
  });

  it("drops hallucinated ids and re-ranks on the cleaned counts", () => {
    // only one of omits-item's ids is real → off-contract now wins
    const valid = new Set(["t-a:o-a", "t-c:o-c", "t-x:o-x"]);
    const tax: Taxonomy = {
      modes: [
        { name: "omits-item", description: "x", nodeIds: ["t-a:o-a", "t-ghost:o-ghost"] },
        { name: "off-contract", description: "y", nodeIds: ["t-c:o-c", "t-x:o-x"] },
      ],
    };
    expect(dominantMode(tax, valid)?.name).toBe("off-contract");
  });

  it("returns null when no candidate mapped", () => {
    expect(dominantMode({ modes: [{ name: "m", description: "d", nodeIds: ["ghost"] }] }, new Set())).toBeNull();
  });
});

// Build a gate result for one node with the given per-axis verdicts.
function gateResult(label: string, verdicts: Partial<Record<NoiseAxis, AxisGrade["verdict"]>>): NodeGateResult {
  const grades: AxisGrade[] = Object.entries(verdicts).map(([axis, verdict]) => ({
    axis: axis as NoiseAxis,
    beforeN: 3,
    beforeMean: 0.5,
    afterN: 3,
    afterMean: 0.7,
    delta: 0.2,
    sigma: 0.05,
    threshold: 0.1,
    verdict: verdict!,
  }));
  return { node: { observationId: label, kind: "compose", skill: "news-digest", label }, patchedOutputs: [], grades };
}

describe("decideShip", () => {
  it("accepts when the target axis improves on the cluster and nothing regresses", () => {
    const cluster = [gateResult("c1", { coverage: "improve" }), gateResult("c2", { coverage: "improve" })];
    const holdout = [gateResult("h1", { coverage: "noise" })];
    const d = decideShip("coverage", cluster, holdout);
    expect(d.accept).toBe(true);
    expect(d.clusterImprove).toBe(2);
  });

  it("rejects when the target axis does not net-improve on the cluster", () => {
    const cluster = [gateResult("c1", { coverage: "noise" }), gateResult("c2", { coverage: "noise" })];
    const d = decideShip("coverage", cluster, []);
    expect(d.accept).toBe(false);
  });

  it("rejects on any regression — even a collateral axis on the cluster", () => {
    const cluster = [gateResult("c1", { coverage: "improve", composition: "regress" })];
    const d = decideShip("coverage", cluster, []);
    expect(d.accept).toBe(false);
  });

  it("rejects on a holdout regression (don't break what works)", () => {
    const cluster = [gateResult("c1", { coverage: "improve" })];
    const holdout = [gateResult("h1", { coverage: "regress" })];
    const d = decideShip("coverage", cluster, holdout);
    expect(d.accept).toBe(false);
  });
});

// ─── materials ───────────────────────────────────────────

// Minimal Observation factory — only the fields classify/render read matter.
function obs(p: Partial<Observation> & Pick<Observation, "id" | "name" | "type">): Observation {
  return {
    parentObservationId: null,
    startTime: "2026-06-14T00:00:00.000Z",
    endTime: "2026-06-14T00:00:01.000Z",
    level: "DEFAULT",
    statusMessage: null,
    input: null,
    output: null,
    metadata: null,
    model: null,
    modelParameters: null,
    usage: null,
    usageDetails: null,
    calculatedTotalCost: null,
    latency: 0,
    ...p,
  };
}

// A realistic news-digest workflow tree: planner attempt → runner → a
// skill compose, a prompt-only compose, an llm_agent step (with an inner
// iteration), plus tool/embedding spans that must be ignored.
function newsDigestTree(plannerGenName = "attempt-1"): { trace: TraceRecord; observations: Observation[] } {
  const observations: Observation[] = [
    // Trace root is an AGENT span — the supervisor / agent-loop start every
    // trace with kind:"agent" (so the root is type AGENT, not CHAIN). The judge
    // must NOT treat this root AGENT as a spawned sub-agent black box, or it
    // skips every node under it.
    obs({ id: "root", name: "news-digest", type: "AGENT", startTime: "2026-06-14T00:00:00.000Z" }),
    obs({ id: "planner", name: "planner", type: "CHAIN", parentObservationId: "root", metadata: { signal_source: "news-digest" }, startTime: "2026-06-14T00:00:00.100Z" }),
    obs({
      id: "gen-planner",
      name: plannerGenName,
      type: "GENERATION",
      parentObservationId: "planner",
      metadata: { [JUDGE_NODE_META]: "planner" },
      input: [
        { role: "system", content: "planner contract body + tools" },
        { role: "user", content: "Source: news-digest\nContent: digest please" },
      ],
      output: '{"version":1,"steps":[]}',
      startTime: "2026-06-14T00:00:00.200Z",
    }),
    obs({ id: "runner", name: "runner", type: "CHAIN", parentObservationId: "root", startTime: "2026-06-14T00:00:01.000Z" }),
    obs({ id: "step-tool", name: "step[0]:tool", type: "TOOL", parentObservationId: "runner", startTime: "2026-06-14T00:00:01.100Z" }),
    obs({ id: "emb", name: "embed", type: "EMBEDDING", parentObservationId: "step-tool", startTime: "2026-06-14T00:00:01.150Z" }),
    obs({ id: "step-compose", name: "step[1]:llm_compose", type: "CHAIN", parentObservationId: "runner", metadata: { preset: "smart", skill: "news-digest", bind: "digest" }, startTime: "2026-06-14T00:00:02.000Z" }),
    obs({
      id: "gen-compose",
      name: "llm_compose:digest",
      type: "GENERATION",
      parentObservationId: "step-compose",
      metadata: { [JUDGE_NODE_META]: "compose", skill: "news-digest" },
      input: [
        { role: "system", content: "news-digest contract" },
        { role: "user", content: "<posts>\nitem-1\n</posts>" },
      ],
      output: "📰 digest text",
      startTime: "2026-06-14T00:00:02.200Z",
    }),
    obs({ id: "step-compose2", name: "step[2]:llm_compose", type: "CHAIN", parentObservationId: "runner", metadata: { preset: "fast", skill: null, bind: "summary" }, startTime: "2026-06-14T00:00:03.000Z" }),
    obs({
      id: "gen-compose-prompt-only",
      name: "llm_compose:summary",
      type: "GENERATION",
      parentObservationId: "step-compose2",
      metadata: { [JUDGE_NODE_META]: "compose", skill: null },
      input: [{ role: "user", content: "Summarize: ${digest}" }],
      output: "short summary",
      startTime: "2026-06-14T00:00:03.200Z",
    }),
    obs({ id: "step-agent", name: "step[3]:llm_agent", type: "AGENT", parentObservationId: "runner", metadata: { skill: "researcher", bind: "answer" }, input: { skill: "researcher", prompt: "research X" }, output: "agent answer", startTime: "2026-06-14T00:00:04.000Z" }),
    obs({ id: "agent-iter", name: "iter-0", type: "GENERATION", parentObservationId: "step-agent", metadata: { [JUDGE_NODE_META]: "compose" }, output: "inner", startTime: "2026-06-14T00:00:04.200Z" }),
  ];
  const trace: TraceRecord = {
    id: "trace-1",
    name: "news-digest",
    sessionId: "scheduler:1",
    timestamp: "2026-06-14T00:00:00.000Z",
    input: null,
    output: null,
    metadata: null,
    observations,
    latency: 0,
    totalCost: 0,
    tags: ["news-digest"],
  };
  return { trace, observations };
}

function sourceFor(fixture: { trace: TraceRecord; observations: Observation[] }): TraceSource {
  return {
    async getTrace() {
      return fixture;
    },
    async recentTraces() {
      return [];
    },
  };
}

describe("classify — node identity is by metadata, not name", () => {
  const { trace, observations } = newsDigestTree();
  const byId = new Map(observations.map((o) => [o.id, o]));
  const find = (id: string) => observations.find((o) => o.id === id)!;

  it("skips the trace root", () => {
    expect(classify(find("root"), trace, byId)).toBe(ROOT_SKIP);
  });

  it("classifies the planner generation as a planner node owned by planner", () => {
    expect(classify(find("gen-planner"), trace, byId)).toEqual({ kind: "planner", skill: "planner" });
  });

  it("classifies a skill compose as a compose node owned by that skill", () => {
    expect(classify(find("gen-compose"), trace, byId)).toEqual({ kind: "compose", skill: "news-digest" });
  });

  it("attributes a prompt-only compose (skill null) to the planner", () => {
    expect(classify(find("gen-compose-prompt-only"), trace, byId)).toEqual({ kind: "compose", skill: "planner" });
  });

  it("classifies an llm_agent step as an agent node, black-box", () => {
    expect(classify(find("step-agent"), trace, byId)).toEqual({ kind: "agent", skill: "researcher" });
  });

  it("does NOT treat the trace-root AGENT as a sub-agent — nodes under it stay judgeable", () => {
    // Regression: the root is an AGENT span (kind:"agent"); a buggy ancestor
    // walk that counts it would skip every node, yielding 0 judgeable nodes on
    // every real trace. The planner sits under root→planner→gen, all under the
    // root AGENT, and must still classify.
    expect(byId.get("root")!.type).toBe("AGENT");
    expect(classify(find("gen-planner"), trace, byId)).toEqual({ kind: "planner", skill: "planner" });
    expect(classify(find("gen-compose"), trace, byId)).toEqual({ kind: "compose", skill: "news-digest" });
  });

  it("skips generations INSIDE an agent span even if tagged", () => {
    expect(classify(find("agent-iter"), trace, byId)).toBeNull();
  });

  it("skips tool spans, embeddings, and container spans", () => {
    expect(classify(find("step-tool"), trace, byId)).toBeNull();
    expect(classify(find("emb"), trace, byId)).toBeNull();
    expect(classify(find("planner"), trace, byId)).toBeNull();
    expect(classify(find("runner"), trace, byId)).toBeNull();
    expect(classify(find("step-compose"), trace, byId)).toBeNull();
  });

  it("is independent of observation names — renaming attempt-1 → generation-1 still classifies the planner node", () => {
    const renamed = newsDigestTree("generation-1");
    const renamedById = new Map(renamed.observations.map((o) => [o.id, o]));
    const gen = renamed.observations.find((o) => o.id === "gen-planner")!;
    expect(gen.name).toBe("generation-1");
    expect(classify(gen, renamed.trace, renamedById)).toEqual({ kind: "planner", skill: "planner" });
  });

  it("skips an untagged generation (not a judgeable node)", () => {
    const untagged = obs({ id: "x", name: "llm_compose:ghost", type: "GENERATION", parentObservationId: "runner" });
    const withGhost = new Map(byId);
    withGhost.set("x", untagged);
    expect(classify(untagged, trace, withGhost)).toBeNull();
  });
});

describe("assembleNodeMaterials — node list + rendered IO", () => {
  it("returns one node per judgeable observation, in execution order", async () => {
    const fixture = newsDigestTree();
    const { nodes } = await assembleNodeMaterials(sourceFor(fixture), "trace-1");

    expect(nodes.map((n) => ({ kind: n.kind, skill: n.skill, label: n.label }))).toEqual([
      { kind: "planner", skill: "planner", label: "attempt-1" },
      { kind: "compose", skill: "news-digest", label: "llm_compose:digest" },
      { kind: "compose", skill: "planner", label: "llm_compose:summary" },
      { kind: "agent", skill: "researcher", label: "step[3]:llm_agent" },
    ]);
  });

  it("renders a generation's chat-messages input as role-labelled blocks", async () => {
    const fixture = newsDigestTree();
    const { nodes } = await assembleNodeMaterials(sourceFor(fixture), "trace-1");
    const planner = nodes.find((n) => n.kind === "planner")!;
    expect(planner.inputText).toContain("[system]");
    expect(planner.inputText).toContain("[user]");
    expect(planner.inputText).toContain("digest please");
    expect(planner.outputText).toContain('"version":1');
  });

  it("renders an agent span's object input as JSON and carries the result as output", async () => {
    const fixture = newsDigestTree();
    const { nodes } = await assembleNodeMaterials(sourceFor(fixture), "trace-1");
    const agent = nodes.find((n) => n.kind === "agent")!;
    expect(agent.inputText).toContain("research X");
    expect(agent.outputText).toBe("agent answer");
  });
});

// ─── monitor ───────────────────────────────────────────

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

// ─── node-judge ───────────────────────────────────────────

// A backend that records which completions it's asked for and returns a minimal
// valid payload for each (scorecard / faithfulness). Lets us assert the gate's
// cost knob (skipFaithfulness) without any network.
function recordingBackend(): { backend: JudgeBackend; names: string[] } {
  const names: string[] = [];
  const backend: JudgeBackend = {
    async complete(req) {
      names.push(req.name);
      if (req.name === "faithfulness") {
        return { applicable: true, claims: [], score: 1, note: "" };
      }
      return {
        axes: [{ axis: "coverage", applicable: true, score: 0.4, label: "weak", rationale: "r", evidence: "e" }],
        overall_note: "",
      };
    },
  };
  return { backend, names };
}

const composeNode: JudgeNodeInput = {
  kind: "compose",
  skill: "news-digest",
  contract: "C",
  inputText: "I",
  outputText: "O",
};

describe("judgeNode skipFaithfulness", () => {
  it("runs both passes by default (scorecard + faithfulness)", async () => {
    const { backend, names } = recordingBackend();
    const v = await judgeNode(backend, composeNode);
    expect(names.sort()).toEqual(["faithfulness", "scorecard"]);
    expect(v.faithfulness).not.toBeNull();
  });

  it("skips the faithfulness pass when asked — halving codex for a compose node", async () => {
    const { backend, names } = recordingBackend();
    const v = await judgeNode(backend, composeNode, { skipFaithfulness: true });
    expect(names).toEqual(["scorecard"]);
    expect(v.faithfulness).toBeNull();
  });
});

// ─── noise ───────────────────────────────────────────

// Build a planner verdict with given process / query_formulation values
// (null = the axis was n/a or absent).
function plannerVerdict(opts: { process?: number | null; qf?: number | null }): NodeJudgement {
  const axes: NodeJudgement["scorecard"]["axes"] = [];
  const push = (axis: "process" | "query_formulation", v: number | null | undefined) => {
    if (v === undefined) return;
    axes.push({
      axis,
      applicable: v !== null,
      score: v,
      label: v === null ? "n/a" : "ok",
      rationale: "",
      evidence: "",
    });
  };
  push("process", opts.process);
  push("query_formulation", opts.qf);
  return { scorecard: { axes, overall_note: "" }, faithfulness: null };
}

describe("extractAxisScores", () => {
  it("returns the numeric score for applicable axes, null otherwise", () => {
    const v = plannerVerdict({ process: 0.7, qf: null });
    const scores = extractAxisScores(v);
    expect(scores.process).toBe(0.7);
    expect(scores.query_formulation).toBeNull(); // n/a → null
    expect(scores.coverage).toBeNull(); // absent → null
    expect(scores.faithfulness).toBeNull(); // planner has no faithfulness
  });

  it("treats applicable=false / score=null as null even if a number is present", () => {
    const v: NodeJudgement = {
      scorecard: {
        axes: [{ axis: "process", applicable: false, score: 0.9, label: "n/a", rationale: "", evidence: "" }],
        overall_note: "",
      },
      faithfulness: null,
    };
    expect(extractAxisScores(v).process).toBeNull();
  });
});

describe("sampleStdDev / percentile", () => {
  it("is zero for fewer than two samples", () => {
    expect(sampleStdDev([])).toBe(0);
    expect(sampleStdDev([0.5])).toBe(0);
  });

  it("computes the n-1 sample stddev", () => {
    // mean 0.5, deviations ±0.1 → var = (0.01+0.01)/1 = 0.02 → σ ≈ 0.1414
    expect(sampleStdDev([0.4, 0.6])).toBeCloseTo(0.141421, 5);
  });

  it("interpolates percentiles", () => {
    expect(percentile([0, 0.1, 0.2, 0.3], 90)).toBeCloseTo(0.27, 5);
    expect(percentile([0.05], 90)).toBe(0.05);
  });
});

describe("collectNodeNoise", () => {
  it("buckets numeric samples and counts n/a flips per axis", () => {
    const node = collectNodeNoise(
      { observationId: "obs1", label: "attempt-1", kind: "planner", skill: "planner" },
      [
        plannerVerdict({ process: 0.6, qf: 0.8 }),
        plannerVerdict({ process: 0.7, qf: null }), // qf flips to n/a
        plannerVerdict({ process: 0.65, qf: 0.8 }),
      ],
    );
    expect(node.runs).toBe(3);
    const proc = node.axes.find((a) => a.axis === "process")!;
    expect(proc.scores).toEqual([0.6, 0.7, 0.65]);
    expect(proc.naCount).toBe(0);
    const qf = node.axes.find((a) => a.axis === "query_formulation")!;
    expect(qf.scores).toEqual([0.8, 0.8]);
    expect(qf.naCount).toBe(1);
    // Axes the node never touched are omitted from the breakdown.
    expect(node.axes.find((a) => a.axis === "coverage")).toBeUndefined();
  });
});

describe("summarizeNoise", () => {
  it("rolls per-node sigmas into a pooled per-axis noise floor", () => {
    const nodes: NodeNoise[] = [
      collectNodeNoise(
        { observationId: "o1", label: "a", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.4 }), plannerVerdict({ process: 0.6 })], // σ ≈ 0.1414
      ),
      collectNodeNoise(
        { observationId: "o2", label: "b", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.5 }), plannerVerdict({ process: 0.5 })], // σ = 0
      ),
    ];
    const report = summarizeNoise(
      { model: "gpt-5.4", provider: "openai", promptVersion: "n3", runs: 2 },
      nodes,
    );
    expect(report.sampleNodes).toBe(2);
    expect(report.totalJudgeCalls).toBe(4);
    const proc = report.axes.find((a) => a.axis === "process")!;
    expect(proc.nodes).toBe(2);
    // pooled = sqrt(mean(0.02, 0)) = sqrt(0.01) = 0.1
    expect(proc.pooledSigma).toBeCloseTo(0.1, 4);
    expect(proc.maxSigma).toBeCloseTo(0.1414, 3);
    expect(proc.meanScore).toBeCloseTo(0.5, 3);
    expect(proc.applicabilityFlips).toBe(0);
  });

  it("flags applicability flips and omits never-scored axes", () => {
    const nodes: NodeNoise[] = [
      collectNodeNoise(
        { observationId: "o1", label: "a", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.6, qf: 0.8 }), plannerVerdict({ process: 0.6, qf: null })],
      ),
    ];
    const report = summarizeNoise(
      { model: "m", provider: "openai", promptVersion: "n3", runs: 2 },
      nodes,
    );
    const qf = report.axes.find((a) => a.axis === "query_formulation")!;
    expect(qf.applicabilityFlips).toBe(1);
    // coverage/composition/faithfulness never scored → omitted
    expect(report.axes.map((a) => a.axis).sort()).toEqual(["process", "query_formulation"]);
  });
});
