// The LLM call path: the transient-failure retry policy over generated
// failure sequences, and the AgentLoop's conversation protocol over
// generated model turns (tool calls that succeed, fail, throw, carry broken
// arguments or name tools the loop may not use).

import fc from "fast-check";
import OpenAI from "openai";
import type { ChatCompletionMessage, ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import { describe, expect, it } from "vitest";
import { createEngine } from "../src/engine";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { retryOnTransient, type RetryInfo } from "../src/providers/retry";
import { createSessionContext } from "../src/session-context";
import { nullTracer } from "../src/tracing";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 150);

// ── retry ────────────────────────────────────────────────────────────────────

type Outcome = { kind: "ok" } | { kind: "transient"; status: number | "connection" } | { kind: "permanent"; status: number } | { kind: "plain" };

const outcome: fc.Arbitrary<Outcome> = fc.oneof(
  { weight: 2, arbitrary: fc.constant<Outcome>({ kind: "ok" }) },
  { weight: 4, arbitrary: fc.constantFrom<number | "connection">(408, 429, 500, 502, 503, 529, "connection").map((status): Outcome => ({ kind: "transient", status })) },
  { weight: 1, arbitrary: fc.constantFrom(400, 401, 403, 404, 422).map((status): Outcome => ({ kind: "permanent", status })) },
  { weight: 1, arbitrary: fc.constant<Outcome>({ kind: "plain" }) },
);

function raise(o: Exclude<Outcome, { kind: "ok" }>): Error {
  const api = (status: number) => new OpenAI.APIError(status, undefined, `status ${status}`, undefined);
  switch (o.kind) {
    case "plain":
      return new Error("not an API error");
    case "permanent":
      return api(o.status);
    case "transient":
      return o.status === "connection" ? new OpenAI.APIConnectionError({ message: "offline" }) : api(o.status);
  }
}

describe("retry on transient failures", () => {
  it("retries exactly the transient failures, within the budget, with capped exponential backoff", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 0, max: 4 }), fc.array(outcome, { minLength: 6, maxLength: 6 }), async (maxRetries, script) => {
        const errors = script.map((o) => (o.kind === "ok" ? null : raise(o)));
        let calls = 0;
        const retries: RetryInfo[] = [];
        const settled = await retryOnTransient(
          async () => {
            const i = calls++;
            const error = errors[i];
            if (error) throw error;
            return `answer ${i}`;
          },
          { maxRetries, baseDelayMs: 1, maxDelayMs: 4, jitter: false, onRetry: (info) => void retries.push(info) },
        ).then(
          (value) => ({ ok: true as const, value }),
          (error: unknown) => ({ ok: false as const, error }),
        );

        // From the script: stop at the first success or non-transient
        // failure, or when the retry budget is spent.
        const stop = script.findIndex((o) => o.kind !== "transient");
        const want = Math.min(stop < 0 ? Infinity : stop + 1, maxRetries + 1);
        const where = `maxRetries=${maxRetries} script=${JSON.stringify(script)}`;
        expect(calls, `${where}: attempts`).toBe(want);
        const last = script[want - 1]!;
        if (last.kind === "ok") expect(settled, where).toEqual({ ok: true, value: `answer ${want - 1}` });
        else expect(settled.ok ? null : settled.error, `${where}: the last failure itself is thrown`).toBe(errors[want - 1]);
        expect(retries.map((r) => r.attempt), `${where}: one retry note per retried attempt`).toEqual(Array.from({ length: want - 1 }, (_, i) => i + 1));
        expect(retries.map((r) => r.delayMs), `${where}: backoff`).toEqual(retries.map((_, i) => Math.min(2 ** i, 4)));
        for (const [i, r] of retries.entries()) {
          const failed = script[i]!;
          expect(r.status, `${where}: status of attempt ${i + 1}`).toBe(failed.kind === "transient" && failed.status !== "connection" ? failed.status : undefined);
        }
      }),
      { numRuns: RUNS },
    );
  });
});

// ── the AgentLoop's conversation protocol ────────────────────────────────────

const tool = (name: string): ChatCompletionTool => ({
  type: "function",
  function: { name, description: name, parameters: { type: "object", properties: { q: { type: "string" } } } },
});
// MCP has four tools; the loop may use three of them.
const MCP_TOOLS = ["t_ok", "t_err", "t_throw", "t_hidden"];
const ALLOWED = new Set(["t_ok", "t_err", "t_throw"]);

interface Call {
  name: string;
  // Raw `arguments` text, as a model would send it.
  args: string;
}
type Turn = { kind: "answer"; text: string } | { kind: "calls"; calls: Call[] };

const call: fc.Arbitrary<Call> = fc.record({
  name: fc.constantFrom(...MCP_TOOLS, "t_ok", "t_ok", "no_such_tool"),
  args: fc.oneof(
    { weight: 6, arbitrary: fc.record({ q: fc.string({ maxLength: 20 }) }).map((a) => JSON.stringify(a)) },
    { weight: 1, arbitrary: fc.constantFrom("", "{}", "{broken", "null", "[1,2]", '"text"') },
  ),
});
const turn: fc.Arbitrary<Turn> = fc.oneof(
  { weight: 3, arbitrary: fc.array(call, { minLength: 1, maxLength: 4 }).map((calls): Turn => ({ kind: "calls", calls })) },
  { weight: 1, arbitrary: fc.string({ maxLength: 20 }).map((text): Turn => ({ kind: "answer", text })) },
);

// Arguments the loop must hand to a tool: a JSON object; empty means {}.
function parsedArgs(raw: string): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(raw || "{}");
    return value !== null && typeof value === "object" && !Array.isArray(value) ? Object.fromEntries(Object.entries(value)) : null;
  } catch {
    return null;
  }
}

describe("AgentLoop conversation protocol", () => {
  it("sends a valid, append-only conversation and dispatches exactly the allowed calls", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(turn, { minLength: 1, maxLength: 6 }), fc.integer({ min: 1, max: 5 }), async (script, maxIterations) => {
        const requests: ChatCompletionMessageParam[][] = [];
        const replies: ChatCompletionMessage[] = [];
        const provider: ChatProvider = {
          kind: "openai",
          async complete(params) {
            requests.push(structuredClone(params.messages));
            const t = script[requests.length - 1];
            if (!t) throw new Error("the model has nothing more to say");
            const message: ChatCompletionMessage =
              t.kind === "answer"
                ? { role: "assistant", content: t.text, refusal: null }
                : {
                    role: "assistant",
                    content: null,
                    refusal: null,
                    tool_calls: t.calls.map((c, i) => ({ id: `call_${requests.length}_${i}`, type: "function", function: { name: c.name, arguments: c.args } })),
                  };
            replies.push(structuredClone(message));
            return { message, finishReason: "stop" };
          },
        };
        const dispatched: { name: string; args: Record<string, unknown> }[] = [];
        const engine = createEngine({
          providers: { openai: provider, gemini: provider, deepseek: provider },
          mcp: {
            tools: MCP_TOOLS.map(tool),
            callTool: async (name, args) => {
              dispatched.push({ name, args });
              if (name === "t_err") return "[tool error] refused";
              if (name === "t_throw") throw new Error("connection reset");
              return `ok ${JSON.stringify(args)}`;
            },
            close: async () => {},
          },
          presets: DEFAULT_PRESETS,
          tracer: nullTracer,
          memory: { get: () => null, set: () => {} },
          codex: { run: async () => { throw new Error("no code_agent"); } },
          skillStore: {
            readSkill: async () => ({ body: "skill", tools: "*", source: "default" }),
            readSkillRaw: async () => "skill",
            readPatch: async () => null,
            saveSkill: async () => ({ path: "s.md", sizeBytes: 0 }),
            savePatch: async () => ({ path: "s.patch.md", sizeBytes: 0 }),
            deletePatch: async () => false,
            listSkills: async () => [],
            validateAll: async () => {},
          },
        });
        engine.log = () => {};
        const sessionContext = createSessionContext({ id: "loop", env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } });
        const loop = await engine.startAgentLoop({ id: "loop", sessionContext, skills: ["s"], toolWhitelist: ALLOWED, maxIterations });
        const settled = await loop.send("start").then(
          (value) => ({ ok: true as const, value }),
          (error: unknown) => ({ ok: false as const, error: error instanceof Error ? error.message : String(error) }),
        );
        await engine.endAgentLoop("loop");
        const where = `maxIterations=${maxIterations} script=${JSON.stringify(script)}`;

        // Termination, from the script: the first answer within the budget.
        const answerAt = script.findIndex((t) => t.kind === "answer");
        const wantCalls = answerAt >= 0 && answerAt < maxIterations ? answerAt + 1 : Math.min(maxIterations, script.length + 1);
        expect(requests, `${where}: model calls`).toHaveLength(Math.min(wantCalls, script.length + 1));
        if (answerAt >= 0 && answerAt < maxIterations) {
          const answer = script[answerAt];
          expect(settled, where).toEqual({ ok: true, value: answer?.kind === "answer" ? answer.text : "" });
          witnessed.add("answers");
        } else {
          expect(settled.ok, `${where}: no answer in budget → the loop fails`).toBe(false);
          witnessed.add(maxIterations <= script.length ? "runs out of iterations" : "model fails");
        }

        // Every request is a valid conversation: each assistant tool_calls
        // message is followed by one tool reply per call, same ids, same order.
        for (const [k, msgs] of requests.entries()) {
          for (let i = 0; i < msgs.length; i++) {
            const m = msgs[i]!;
            if (m.role === "tool") {
              const owner = msgs.slice(0, i).reverse().find((x) => x.role !== "tool");
              expect(owner?.role, `${where}: request ${k}: a tool reply follows an assistant call`).toBe("assistant");
            }
            if (m.role !== "assistant" || !m.tool_calls?.length) continue;
            const answered = msgs.slice(i + 1, i + 1 + m.tool_calls.length);
            expect(answered.map((x) => (x.role === "tool" ? x.tool_call_id : `<${x.role}>`)), `${where}: request ${k}: replies to message ${i}`).toEqual(m.tool_calls.map((c) => c.id));
          }
          // Append-only: the next request is this one, the model's reply,
          // and the tool replies — nothing earlier rewritten.
          const next = requests[k + 1];
          if (next) {
            expect(next.slice(0, msgs.length), `${where}: request ${k + 1} rewrote history`).toEqual(msgs);
            expect(next[msgs.length], `${where}: the model's own message comes back as sent`).toEqual(replies[k]);
          }
        }

        // MCP sees exactly the allowed calls with object arguments, in order,
        // with the arguments the model sent.
        // Every reply's calls ran: the loop only stops after replying to them.
        const sent = replies.flatMap((r) => r.tool_calls ?? []);
        const wantDispatch = sent.flatMap((c) => {
          if (c.type !== "function") return [];
          const args = parsedArgs(c.function.arguments);
          return ALLOWED.has(c.function.name) && args ? [{ name: c.function.name, args }] : [];
        });
        expect(dispatched, `${where}: dispatched calls`).toEqual(wantDispatch);
        if (sent.some((c) => c.type === "function" && !ALLOWED.has(c.function.name))) witnessed.add("refuses a tool outside the loop");
        if (sent.some((c) => c.type === "function" && parsedArgs(c.function.arguments) === null)) witnessed.add("refuses broken arguments");
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(["answers", "model fails", "refuses a tool outside the loop", "refuses broken arguments", "runs out of iterations"]);
  });
});
