// The skills contract between the two packages. The agent loads skills
// (packages/agent/src/skills.ts) and the MCP server exports them to readers
// (packages/mcp/src/services/skills) — two implementations of one layout,
// which the packages may not share code for. This test lives outside both
// so it can run them side by side on the same generated skill directories
// and demand they agree: which skills exist and in which layer, what tools
// each declares, and the instructions in force (body + improver patch).

import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import fc from "fast-check";
import { describe, expect, it } from "vitest";
import { appendPatch, createSkillStore } from "../packages/agent/src/skills";
import { createSkillsModule } from "../packages/mcp/src/services/skills";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 150);

const NAMES = ["telegram", "news-digest", "a_b", "planner", "x1"];
const frontmatter = fc.constantFrom(
  "---\ntools: *\n---\n",
  "---\r\ntools: [send_telegram_message, recall]\r\n---\r\n",
  "---\ntools: []\n---\n",
  "---\ntools:*\n---\n",
  "---\ntools:   *  \n---\n",
  "---\ntools: [ a ,b ]\n---\n",
  "---\ntitle: X\ntools: [a, b]\n---\n",
  "", // no frontmatter: the agent refuses the skill
  "---\ntools: [Bad-Name]\n---\n", // the agent refuses an invalid tool name
);
const body = fc.constantFrom(
  "# Title\n\nDo the thing.\n",
  "# Title\r\n\r\nWindows lines.\r\n",
  "body with --- inside\n---\nmore\n",
  "trailing space   \n\n\n",
  "",
);
const patch = fc.constantFrom(undefined, "", "   \n", "Also do X.\n", "\n\nCRLF fix\r\n");

interface SkillFiles {
  live: { frontmatter: string; body: string } | undefined;
  def: { frontmatter: string; body: string } | undefined;
  patch: string | undefined;
}

const file = fc.record({ frontmatter, body });
const files: fc.Arbitrary<SkillFiles> = fc.record({
  live: fc.option(file, { nil: undefined }),
  def: fc.option(file, { nil: undefined }),
  patch,
});
const layout: fc.Arbitrary<Record<string, SkillFiles>> = fc.dictionary(fc.constantFrom(...NAMES), files, { minKeys: 1 });

describe("skills: the agent's loader and the MCP export agree", () => {
  it("on which skills exist, their layer, tools and the instructions in force", async () => {
    await fc.assert(
      fc.asyncProperty(layout, async (skills) => {
        const root = mkdtempSync(path.join(tmpdir(), "skills-contract-"));
        const liveDir = path.join(root, "live");
        const defaultsDir = path.join(root, "defaults");
        mkdirSync(liveDir);
        mkdirSync(defaultsDir);
        try {
          for (const [name, s] of Object.entries(skills)) {
            if (s.live) writeFileSync(path.join(liveDir, `${name}.md`), s.live.frontmatter + s.live.body);
            if (s.def) writeFileSync(path.join(defaultsDir, `${name}.md`), s.def.frontmatter + s.def.body);
            if (s.patch !== undefined) writeFileSync(path.join(liveDir, `${name}.patch.md`), s.patch);
          }
          const agent = createSkillStore({ liveDir, defaultsDir });
          const mcp = createSkillsModule({ liveDir, defaultsDir }).catalog;
          const where = JSON.stringify(skills).slice(0, 500);

          const agentList = (await agent.listSkills()).map((e) => [e.name, e.source]);
          const mcpList = (await mcp.listSkills()).map((e) => [e.name, e.source]);
          expect(agentList, `${where}: the same skills, from the same layer`).toEqual(mcpList);
          const exists = Object.entries(skills).filter(([, s]) => s.live || s.def).map(([n]) => n).sort();
          expect(agentList.map(([n]) => n), `${where}: exactly the skill files, never a patch`).toEqual(exists);

          for (const name of exists) {
            const exported = await mcp.readSkill(`${name}.md`);
            if (!exported) throw new Error(`${name}: listed but not readable through MCP`);
            const loaded = await agent.readSkill(name).then(
              (s) => s,
              () => null, // the agent refuses broken frontmatter; nothing runs
            );
            // A skill the export reads as well-formed (tools parsed, names
            // valid) is one the agent must load: otherwise the export shows
            // a skill that never runs.
            const wellFormed =
              exported.tools === "*" || (Array.isArray(exported.tools) && exported.tools.every((t) => /^[a-z_][a-z0-9_]*$/.test(t)));
            expect(loaded !== null, `${where}: ${name} loads iff the export reads it as well-formed`).toBe(wellFormed);
            if (loaded === null) continue;
            expect(exported.source, `${where}: ${name} layer`).toBe(loaded.source);
            expect(exported.tools, `${where}: ${name} tools`).toEqual(loaded.tools);
            const inForce = appendPatch(loaded.body, (await agent.readPatch(name)) ?? "");
            expect(exported.effectiveInstructions, `${where}: ${name} instructions in force`).toBe(inForce);
            expect(exported.patched, `${where}: ${name} patched flag`).toBe(inForce !== loaded.body);
          }
        } finally {
          rmSync(root, { recursive: true, force: true });
        }
      }),
      { numRuns: RUNS },
    );
  });
});
