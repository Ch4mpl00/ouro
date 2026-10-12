import { describe, expect, it } from "vitest";
import OpenAI from "openai";
import { z } from "zod";
import {
  choice,
  createDecisionClient,
  DecisionError,
  filter,
  FILTER_MAX_BATCH_ITEMS,
  noul,
  OPENROUTER_DECISIONS_URL,
  score,
  type DecisionClient,
} from "./decisions";
import { nullTracer } from "./tracing";

// A real OpenAI SDK client over a scripted fetch: the request goes through the
// same SDK path (headers, JSON, APIError on non-2xx) as against OpenRouter.
function scriptedClient(replies: Array<{ status: number; body: unknown }>) {
  const requests: Array<{ url: string; body: unknown }> = [];
  const client = new OpenAI({
    apiKey: "test-key",
    baseURL: "https://openrouter.test/api/v1",
    fetch: async (url, init) => {
      requests.push({ url: String(url), body: JSON.parse(String(init?.body)) });
      const reply = replies.shift();
      if (!reply) throw new Error("Unexpected decisions call");
      return new Response(JSON.stringify(reply.body), {
        status: reply.status,
        headers: { "content-type": "application/json" },
      });
    },
  });
  return { client, requests };
}

const scope = nullTracer.trace({ id: "t", name: "test" });

const questions = {
  intent: choice("What does the user want?", {
    digest: "A news digest",
    reminder: "A reminder",
    chit_chat: "Small talk",
  }),
  urgency: score("How urgent is it?", ["Low", "Normal", "High"]),
  is_question: noul("The message is a question.", { true: "Yes", false: "No" }),
};

// Captured from the live API (typesafe/jev-1.13, 2026-10-12).
const LIVE_REPLY = {
  model: "typesafe/jev-1.13-20260917",
  answers: {
    intent: {
      type: "choice",
      choice: "digest",
      probabilities: { digest: 1, reminder: 0, chit_chat: 0 },
      confidence: 1,
    },
    urgency: {
      type: "score",
      score: 0.37,
      legend: { "0": "Low", "1": "Normal", "2": "High" },
      probabilities: { "0": 0.64, "1": 0.35, "2": 0.01 },
      confidence: 0.44,
    },
    is_question: { type: "noul", noul: 0.25 },
  },
  usage: { input_tokens: 544, output_tokens: 82, cost: 2.2848e-5 },
  id: "gen-dec-1",
  provider: "TypeSafe",
};

describe("decision client", () => {
  it("sends the native questions schema to the decisions endpoint", async () => {
    const { client, requests } = scriptedClient([{ status: 200, body: LIVE_REPLY }]);
    await createDecisionClient({ client, model: "typesafe/jev-1.13" }).decide({
      state: { message: "скинь дайджест" },
      questions,
      scope,
    });
    expect(requests[0]!.url).toBe(OPENROUTER_DECISIONS_URL);
    expect(requests[0]!.body).toEqual({
      model: "typesafe/jev-1.13",
      state: { message: "скинь дайджест" },
      questions: {
        intent: {
          type: "choice",
          instructions: "What does the user want?",
          criteria: { digest: "A news digest", reminder: "A reminder", chit_chat: "Small talk" },
        },
        urgency: { type: "score", instructions: "How urgent is it?", criteria: ["Low", "Normal", "High"] },
        is_question: {
          type: "noul",
          instructions: "The message is a question.",
          criteria: { true: "Yes", false: "No" },
        },
      },
    });
  });

  it("normalises answers: score levels by label, noul as a probability", async () => {
    const { client } = scriptedClient([{ status: 200, body: LIVE_REPLY }]);
    const decision = await createDecisionClient({ client, model: "typesafe/jev-1.13" }).decide({
      state: "скинь дайджест",
      questions,
      scope,
    });

    const intent = decision.answer("intent");
    expect(intent.choice).toBe("digest");
    expect(intent.confidence).toBe(1);
    expect(intent.probabilities.get("reminder")).toBe(0);

    expect(decision.answer("urgency")).toEqual({
      level: "Low",
      score: 0.37,
      confidence: 0.64,
      probabilities: [0.64, 0.35, 0.01],
    });
    expect(decision.answer("is_question")).toEqual({ probability: 0.25 });
    expect(decision.model).toBe("typesafe/jev-1.13-20260917");
    expect(decision.usage).toEqual({ input: 544, output: 82, total: 626 });
    expect(decision.costUsd).toBe(2.2848e-5);
  });

  it("rejects a choice outside the declared options instead of passing it on", async () => {
    const reply = structuredClone(LIVE_REPLY);
    reply.answers.intent.choice = "weather";
    const { client } = scriptedClient([{ status: 200, body: reply }]);
    const decide = createDecisionClient({ client, model: "typesafe/jev-1.13" }).decide({ state: "x", questions, scope });
    await expect(decide).rejects.toThrow(/answer "intent": choice "weather" is not one of the declared options/);
  });

  it("rejects a reply that is missing an answer", async () => {
    const { client } = scriptedClient([
      { status: 200, body: { ...LIVE_REPLY, answers: { intent: LIVE_REPLY.answers.intent } } },
    ]);
    const decide = createDecisionClient({ client, model: "typesafe/jev-1.13" }).decide({ state: "x", questions, scope });
    await expect(decide).rejects.toThrow(DecisionError);
  });

  it("retries a transient failure and gives up at once on a 4xx", async () => {
    const transient = scriptedClient([
      { status: 503, body: { error: { message: "overloaded" } } },
      { status: 200, body: LIVE_REPLY },
    ]);
    const decision = await createDecisionClient({ client: transient.client, model: "m" }).decide({
      state: "x",
      questions,
      scope,
    });
    expect(transient.requests).toHaveLength(2);
    expect(decision.answer("intent").choice).toBe("digest");

    const permanent = scriptedClient([{ status: 400, body: { error: { message: "bad schema" } } }]);
    await expect(
      createDecisionClient({ client: permanent.client, model: "m" }).decide({ state: "x", questions, scope }),
    ).rejects.toThrow(/400/);
    expect(permanent.requests).toHaveLength(1);
  });

  it("refuses a choice question the API would reject", () => {
    expect(() => choice("one option", { only: "x" })).toThrow(/2–255 options/);
  });
});

// A decision client that answers each filter question from the item itself,
// parsing through the question's own parser — the same path a real answer
// takes. Records every request's state.
function fakeFilterClient(pOf: (item: unknown) => number, fail?: Error) {
  const FilterState = z.object({ rules: z.string(), items: z.array(z.object({ n: z.number(), item: z.unknown() })) });
  const states: Array<z.infer<typeof FilterState>> = [];
  const client: DecisionClient = {
    model: "fake",
    async decide(req) {
      if (fail) throw fail;
      const state = FilterState.parse(req.state);
      states.push(state);
      return {
        model: "fake",
        answer: (key) => {
          const item = state.items.find((i) => `item_${i.n}` === key)?.item;
          return req.questions[key].parse({ type: "noul", noul: pOf(item) });
        },
      };
    },
  };
  return { client, states };
}

describe("filter preset", () => {
  it("keeps items at or above the threshold, in input order, with aligned probabilities", async () => {
    const { client, states } = fakeFilterClient((item) => (item === "b" || item === "d" ? 0.3 : 0.29));
    const r = await filter(client, { items: ["a", "b", "c", "d"], prompt: "rules", scope });
    expect(r.kept).toEqual(["b", "d"]);
    expect(r.probabilities).toEqual([0.29, 0.3, 0.29, 0.3]);
    expect(states[0]).toEqual({
      rules: "rules",
      items: [
        { n: 1, item: "a" },
        { n: 2, item: "b" },
        { n: 3, item: "c" },
        { n: 4, item: "d" },
      ],
    });
  });

  it("honours a custom threshold", async () => {
    const { client } = fakeFilterClient((item) => (item === "hi" ? 0.6 : 0.4));
    const r = await filter(client, { items: ["hi", "lo"], prompt: "r", threshold: 0.5, scope });
    expect(r.kept).toEqual(["hi"]);
  });

  it("packs batches by item count and by size, keeping order across batches", async () => {
    const { client, states } = fakeFilterClient(() => 1);
    const items = Array.from({ length: 60 }, (_, i) => i);
    const r = await filter(client, { items, prompt: "r", scope });
    expect(states.map((s) => s.items.length).sort((a, b) => b - a)).toEqual([FILTER_MAX_BATCH_ITEMS, FILTER_MAX_BATCH_ITEMS, 10]);
    expect(r.kept).toEqual(items);

    const big = fakeFilterClient(() => 1);
    await filter(big.client, { items: ["x".repeat(15_000), "y".repeat(15_000), "z"], prompt: "r", scope });
    expect(big.states.map((s) => s.items.length).sort()).toEqual([1, 2]);
  });

  it("asks nothing for an empty list", async () => {
    const { client, states } = fakeFilterClient(() => 1);
    expect(await filter(client, { items: [], prompt: "r", scope })).toEqual({ kept: [], probabilities: [] });
    expect(states).toHaveLength(0);
  });

  it("fails when the decision model fails — no pass-through", async () => {
    const { client } = fakeFilterClient(() => 1, new Error("openrouter 503"));
    await expect(filter(client, { items: ["a"], prompt: "r", scope })).rejects.toThrow(/openrouter 503/);
  });
});
