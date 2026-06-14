import { describe, expect, it } from "vitest";
import type { Observation, Trace } from "../trace-model";
import { JUDGE_NODE_META } from "../trace-model";
import { assembleNodeMaterials, classify, ROOT_SKIP } from "./materials";
import type { TraceSource } from "./trace-source";

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
function newsDigestTree(plannerGenName = "attempt-1"): { trace: Trace; observations: Observation[] } {
  const observations: Observation[] = [
    obs({ id: "root", name: "news-digest", type: "CHAIN", startTime: "2026-06-14T00:00:00.000Z" }),
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
  const trace: Trace = {
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

function sourceFor(fixture: { trace: Trace; observations: Observation[] }): TraceSource {
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
