// The improver's patch overlay on the workflow path, wired the way the
// supervisor wires it: a real skill store on temp directories, patches written
// and removed through the store, and the planner and a compose step run by
// the real runner. The instructions in force are body + patch, so whatever
// the store holds must be exactly what each model call is told — the
// improver's measurements and `read_skill` both assume it.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import fc from "fast-check";
import type { ChatCompletionTool } from "openai/resources/chat/completions";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { createSkillStore } from "../src/skills";
import { nullTracer } from "../src/tracing";
import { createWorkflowRunner } from "../src/workflow";

const TOOL: ChatCompletionTool = {
  type: "function",
  function: { name: "t", description: "probe", parameters: { type: "object", properties: {}, required: [] } },
};

const PLAN = JSON.stringify({
  version: 1,
  steps: [
    { kind: "llm_compose", preset: "smartest", skill: "writer", input: {}, bind: "d" },
    { kind: "terminal" },
  ],
});

// What a patch overlay holds, or doesn't.
type Overlay = { kind: "none" } | { kind: "written"; text: string } | { kind: "removed"; text: string };
const overlay: fc.Arbitrary<Overlay> = fc.oneof(
  fc.constant<Overlay>({ kind: "none" }),
  fc.constantFrom("Prefer short items.", "  \n", "- lesson one\n- lesson two\n\n", "Никогда не выдумывай ссылки.").map((text): Overlay => ({ kind: "written", text })),
  fc.constantFrom("Prefer short items.").map((text): Overlay => ({ kind: "removed", text })),
);

let root: string;
beforeAll(() => {
  root = mkdtempSync(path.join(tmpdir(), "workflow-patch-"));
});
afterAll(() => rmSync(root, { recursive: true, force: true }));

describe("improver patches on the workflow path", () => {
  it("each model call is told its skill body followed by exactly the patch in force", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(overlay, overlay, async (plannerPatch, writerPatch) => {
        const dir = mkdtempSync(path.join(root, "run-"));
        const defaultsDir = path.join(dir, "defaults");
        mkdirSync(defaultsDir);
        writeFileSync(path.join(defaultsDir, "planner.md"), "---\ntools: []\n---\n# Planner body\n\nPlan well.\n");
        writeFileSync(path.join(defaultsDir, "writer.md"), "---\ntools: []\n---\n# Writer body\n\nWrite well.\n");
        const store = createSkillStore({ liveDir: path.join(dir, "live"), defaultsDir });
        for (const [name, o] of [["planner", plannerPatch], ["writer", writerPatch]] as const) {
          if (o.kind === "none") continue;
          await store.savePatch(name, o.text);
          if (o.kind === "removed") await store.deletePatch(name);
        }

        const systems: { call: "planner" | "writer"; system: string }[] = [];
        const provider: ChatProvider = {
          kind: "openai",
          async complete(params) {
            const first = params.messages[0];
            const system = first?.role === "system" && typeof first.content === "string" ? first.content : "";
            const call = system.includes("# Planner body") ? "planner" : "writer";
            systems.push({ call, system });
            return { message: { role: "assistant", content: call === "planner" ? PLAN : "composed", refusal: null }, finishReason: "stop" };
          },
        };
        const runner = createWorkflowRunner({
          engine: {
            presets: DEFAULT_PRESETS,
            resolveProvider: () => provider,
            mcp: { callTool: async () => "ok" },
            startAgentLoop: async () => {
              throw new Error("no agents here");
            },
            endAgentLoop: () => {},
          },
          mcpTools: [TOOL],
          knownSkills: ["writer"],
          // As supervisor/main.ts wires the store.
          readSkill: async (name) => (await store.readSkill(name))?.body ?? null,
          readPatch: (name) => store.readPatch(name),
          setMemory: () => {},
        });
        const result = await runner.runForSignal(
          { id: 1, source: "scheduler", content: "tick", envContext: null },
          createSessionContext({ id: "s", env: { now: new Date("2026-10-13T10:00:00Z"), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } }),
          nullTracer.trace({ id: "s", name: "s", kind: "agent" }),
        );
        expect(result.ok, JSON.stringify(result)).toBe(true);
        expect(systems.map((s) => s.call)).toEqual(["planner", "writer"]);

        for (const { call, system } of systems) {
          const o = call === "planner" ? plannerPatch : writerPatch;
          const body = call === "planner" ? "Plan well." : "Write well.";
          const inForce = o.kind === "written" ? o.text.trim() : "";
          const where = `${call}: ${JSON.stringify(o)}`;
          expect(system, `${where}: the body is there`).toContain(body);
          if (inForce === "") {
            if (o.kind === "removed") witnessed.add("a removed patch");
            expect(system, `${where}: no patch, no patch section`).not.toContain("improver-patch");
            expect(system, `${where}: a removed patch is gone`).not.toContain("Prefer short items.");
            continue;
          }
          witnessed.add(`a ${call} patch in force`);
          // The patch comes last — after the body and, for the planner, after
          // the static tool/skill reference — and only once.
          expect(system.trimEnd().endsWith(inForce), `${where}: the patch ends the instructions`).toBe(true);
          expect(system.indexOf(body), `${where}: body before patch`).toBeLessThan(system.lastIndexOf(inForce));
          expect(system.split(inForce).length - 1, `${where}: the patch once`).toBe(1);
        }
      }),
      { numRuns: 40 },
    );
    expect([...witnessed].sort()).toEqual(["a planner patch in force", "a removed patch", "a writer patch in force"]);
  });
});
