// write_skill through the real AgentLoop and skill store on temp
// directories. A skill is the system prompt of every later signal, so two
// things must hold whatever a model writes: only the dreaming flow can
// write one at all, and no write leaves a skill the agent can't load — the
// supervisor validates every skill at startup, and a broken one keeps it
// from starting.

import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import fc from "fast-check";
import type { ChatCompletionMessage, ChatCompletionTool } from "openai/resources/chat/completions";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createEngine } from "../src/engine";
import { DEFAULT_PRESETS } from "../src/models";
import type { ChatProvider } from "../src/providers";
import { createSessionContext } from "../src/session-context";
import { createSkillStore } from "../src/skills";
import { nullTracer } from "../src/tracing";

const MCP_TOOLS = ["send_telegram_message", "recall", "list_news"];
const tool = (name: string): ChatCompletionTool => ({ type: "function", function: { name, description: name, parameters: { type: "object", properties: {} } } });

let root: string;
beforeAll(() => {
  root = mkdtempSync(path.join(tmpdir(), "write-skill-"));
});
afterAll(() => rmSync(root, { recursive: true, force: true }));

const frontmatter = fc.constantFrom(
  "---\ntools: *\n---\n",
  "---\ntools: [send_telegram_message, recall]\n---\n",
  "---\ntools: []\n---\n",
  "", // none
  "---\ntools: [no_such_tool]\n---\n", // a tool MCP doesn't have
  "---\ntools: [Bad-Name]\n---\n",
  "---\ntitle: only\n---\n", // no tools line
);
const write = fc.record({
  name: fc.constantFrom("telegram", "news-digest", "orchestrator", "new-skill", "../escape", "x.patch"),
  content: frontmatter.chain((fm) => fc.string({ maxLength: 40 }).map((body) => `${fm}# Revised\n\n${body}\n`)),
});

describe("write_skill", () => {
  it("is offered only to the dreaming flow, and never leaves a skill the agent can't start with", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(write, { minLength: 1, maxLength: 5 }), fc.constantFrom<string[]>(["dreaming"], ["telegram"], ["worker"]), async (writes, skills) => {
        const dir = mkdtempSync(path.join(root, "run-"));
        const liveDir = path.join(dir, "live");
        const defaultsDir = path.join(dir, "defaults");
        mkdirSync(defaultsDir, { recursive: true });
        for (const name of ["dreaming", "telegram", "worker", "news-digest", "orchestrator"]) {
          writeFileSync(path.join(defaultsDir, `${name}.md`), `---\ntools: *\n---\n# ${name}\n`);
        }
        const store = createSkillStore({ liveDir, defaultsDir });

        let turn = 0;
        const offered: string[][] = [];
        const provider: ChatProvider = {
          kind: "openai",
          async complete(params) {
            offered.push((params.tools ?? []).map((t) => (t.type === "function" ? t.function.name : "")));
            const w = writes[turn++];
            const message: ChatCompletionMessage = w
              ? { role: "assistant", content: null, refusal: null, tool_calls: [{ id: `c${turn}`, type: "function", function: { name: "write_skill", arguments: JSON.stringify(w) } }] }
              : { role: "assistant", content: "done", refusal: null };
            return { message, finishReason: "stop" };
          },
        };
        const engine = createEngine({
          providers: { openai: provider, gemini: provider, deepseek: provider },
          mcp: { tools: MCP_TOOLS.map(tool), callTool: async () => "ok", close: async () => {} },
          presets: DEFAULT_PRESETS,
          tracer: nullTracer,
          memory: { get: () => null, set: () => {} },
          codex: { run: async () => { throw new Error("no code_agent"); } },
          skillStore: store,
        });
        engine.log = () => {};
        const loop = await engine.startAgentLoop({
          id: "loop",
          sessionContext: createSessionContext({ id: "loop", env: { now: new Date(), timezone: "UTC", userEmail: null, newsLastReadAt: null, techLastReadAt: null } }),
          skills,
          includeEngineSkills: false,
          maxIterations: writes.length + 1,
        });
        await loop.send("revise the skills");
        await engine.endAgentLoop("loop");

        const dreaming = skills.includes("dreaming");
        expect(offered[0]?.includes("write_skill"), `write_skill offered to a ${skills[0]} loop`).toBe(dreaming);
        const live = (() => {
          try {
            return readdirSync(liveDir);
          } catch {
            return [];
          }
        })();
        if (!dreaming) {
          witnessed.add("refused outside dreaming");
          expect(live, "nothing written outside the dreaming flow").toEqual([]);
        }
        for (const f of live) {
          expect(f.endsWith(".md") && !f.includes("/") && !f.startsWith(".."), `only skill files in the live dir: ${f}`).toBe(true);
          const content = readFileSync(path.join(liveDir, f), "utf-8");
          expect(writes.some((w) => w.content === content), `${f} holds something that was written`).toBe(true);
        }
        if (live.length > 0) witnessed.add("a valid revision lands");
        if (dreaming && writes.some((w) => !w.content.startsWith("---\ntools: *") && !w.content.startsWith("---\ntools: [send") && !w.content.startsWith("---\ntools: []"))) {
          witnessed.add("a broken revision is offered");
        }
        // Whatever was written, the agent still starts.
        await expect(store.validateAll(MCP_TOOLS), `after ${JSON.stringify(writes)}`).resolves.toBeUndefined();
      }),
      { numRuns: 120 },
    );
    expect([...witnessed].sort()).toEqual(["a broken revision is offered", "a valid revision lands", "refused outside dreaming"]);
  });
});
