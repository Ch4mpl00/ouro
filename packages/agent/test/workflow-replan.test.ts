// The plan → act → replan loop (runForSignal) with the real compiler and
// executor, a scripted planner model and a scripted tool. Each pass of a
// generated script either acts, fails while acting, gathers and asks to
// replan, or can't produce a valid plan; the rules say from the script alone
// how the signal ends, how many planner calls it takes, and what each pass
// is told and given.

import fc from "fast-check";
import type { ChatCompletionTool } from "openai/resources/chat/completions";
import { describe, expect, it } from "vitest";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { nullTracer } from "../src/tracing";
import { createWorkflowRunner } from "../src/workflow";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 150);
const MAX_ATTEMPTS = 3;

type Pass = "act" | "fail" | "replan" | "invalid";

const TOOL: ChatCompletionTool = {
  type: "function",
  function: { name: "t", description: "probe", parameters: { type: "object", properties: { q: { type: "string" } }, required: ["q"] } },
};

// The plan the planner emits on pass `p`.
function planFor(p: number, kind: Pass): string {
  const carried = p > 0 ? ` with \${context.v${p - 1}}` : "";
  switch (kind) {
    case "invalid":
      return "{ not a plan";
    case "act":
      return JSON.stringify({ version: 1, steps: [{ kind: "tool", tool: "t", args: { q: `act ${p}${carried}` } }, { kind: "terminal" }] });
    case "fail":
      return JSON.stringify({ version: 1, steps: [{ kind: "tool", tool: "t", args: { q: `FAIL ${p}${carried}` } }] });
    case "replan":
      return JSON.stringify({
        version: 1,
        steps: [
          { kind: "tool", tool: "t", args: { q: `gather ${p}${carried}` }, bind: `v${p}` },
          { kind: "replan", context: [`v${p}`, "ghost"], note: `note from ${p}` },
        ],
      });
  }
}

describe("runForSignal: plan, act, replan", () => {
  it("ends the way the script says, carrying exactly the gathered context forward", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 1, max: 4 }), fc.array(fc.constantFrom<Pass>("act", "fail", "replan", "replan", "invalid"), { minLength: 4, maxLength: 4 }), async (maxPasses, script) => {
        const prompts: { pass: number; user: string }[] = [];
        const toolCalls: string[] = [];
        let pass = 0;
        let attemptsThisPass = 0;
        const provider: ChatProvider = {
          kind: "openai",
          async complete(params) {
            const user = params.messages.find((m) => m.role === "user");
            prompts.push({ pass, user: typeof user?.content === "string" ? user.content : "" });
            const kind = script[pass]!;
            attemptsThisPass++;
            const content = planFor(pass, kind);
            // The next planner call belongs to the next pass once this pass
            // produced a plan, or ran out of attempts.
            if (kind !== "invalid" || attemptsThisPass === MAX_ATTEMPTS) {
              pass++;
              attemptsThisPass = 0;
            }
            return { message: { role: "assistant", content, refusal: null }, finishReason: "stop" };
          },
        };
        const runner = createWorkflowRunner({
          engine: {
            presets: DEFAULT_PRESETS,
            resolveProvider: () => provider,
            mcp: {
              callTool: async (_name, args) => {
                const q = String(args.q);
                toolCalls.push(q);
                if (q.startsWith("FAIL")) return "[tool error] boom";
                return JSON.stringify({ gathered: q.split(" ").slice(0, 2).join(" ") });
              },
            },
            startAgentLoop: async () => {
              throw new Error("no agents here");
            },
            endAgentLoop: () => {},
          },
          mcpTools: [TOOL],
          knownSkills: ["s"],
          readSkill: async (name) => (name === "planner" ? "plan well" : "skill"),
          setMemory: () => {},
          maxAttempts: MAX_ATTEMPTS,
          maxPasses,
        });
        const context = createSessionContext({ id: "s", env: { now: new Date("2026-10-12T10:00:00Z"), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } });
        const result = await runner.runForSignal(
          { id: 1, source: "scheduler", content: "tick", envContext: null },
          context,
          nullTracer.trace({ id: "s", name: "s", kind: "agent" }),
        );

        // Expected from the script: the first non-replan pass decides; all
        // replans within the budget means exhaustion.
        const reached = Math.min(maxPasses, (script.findIndex((k) => k !== "replan") + 1) || maxPasses);
        const last = script[reached - 1]!;
        const where = `maxPasses=${maxPasses} script=${script.join(",")}`;
        const ending = last === "replan" ? "replan_exhausted" : last === "invalid" ? "compile" : last === "fail" ? "execute" : "ok";
        expect(result.ok ? "ok" : result.stage, `${where}: how the signal ends`).toBe(ending);

        const plannerCalls = script.slice(0, reached).reduce((n, k) => n + (k === "invalid" ? MAX_ATTEMPTS : 1), 0);
        expect(prompts, `${where}: planner calls`).toHaveLength(plannerCalls);

        // Pass p acts with p-1's gathered value seeded as context.v{p-1}.
        const wantCalls = script.slice(0, reached).flatMap((k, p) => {
          const carried = p > 0 ? ` with ${JSON.stringify({ gathered: `gather ${p - 1}` })}` : "";
          if (k === "invalid") return [];
          return [`${k === "act" ? "act" : k === "fail" ? "FAIL" : "gather"} ${p}${carried}`];
        });
        expect(toolCalls, `${where}: each pass runs its own steps, with the previous pass's data`).toEqual(wantCalls);

        for (const { pass: p, user } of prompts) {
          const hasContext = user.includes("<context>");
          expect(hasContext, `${where}: only replan passes are told about a previous pass (pass ${p})`).toBe(p > 0);
          if (p === 0) continue;
          expect(user, `${where}: pass ${p} sees the gathered data`).toContain(`"v${p - 1}"`);
          expect(user, `${where}: a name never bound is not carried`).not.toContain('"ghost"');
          expect(user, `${where}: the note travels`).toContain(`note from ${p - 1}`);
          expect(user.includes("LAST pass"), `${where}: pass ${p} is the last one iff it is`).toBe(p === maxPasses - 1);
        }
      }),
      { numRuns: RUNS },
    );
  });
});
