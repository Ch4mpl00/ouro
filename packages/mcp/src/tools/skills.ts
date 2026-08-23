import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { SkillCatalog } from "../services/skills";
import { jsonResult } from "../result";

export function registerSkillsTools(server: McpServer, skills: SkillCatalog): void {
  server.registerTool(
    "list_skills",
    {
      title: "List available skills",
      description:
        "Return the catalog of all available skills. Each entry includes the " +
        "exact fileName, human-readable title, short description, declared " +
        "tool access, active source layer, size, modification time, and " +
        "`patched` — whether an improver patch is currently appended to that " +
        "skill's instructions. Use " +
        "this first to choose a skill, then pass its exact fileName to read_skill.",
      inputSchema: {},
    },
    async () => {
      const catalog = await skills.listSkills();
      return jsonResult({ count: catalog.length, skills: catalog });
    },
  );

  server.registerTool(
    "read_skill",
    {
      title: "Read complete skill instructions",
      description:
        "Return the complete UTF-8 Markdown contents of one active skill file " +
        "selected from list_skills, including its frontmatter. Pass the exact " +
        "fileName from the catalog, including the .md extension. Three views " +
        "come back: `content` is the editable source file; `patch` is the " +
        "improver's append-only overlay (null when there is none); and " +
        "`effectiveInstructions` is what the agent actually runs — the body " +
        "with the frontmatter stripped and the patch appended. When `patch` " +
        "is non-null, judge the skill's behaviour by effectiveInstructions, " +
        "not by content alone.",
      inputSchema: {
        fileName: z
          .string()
          .min(1)
          .describe("Exact skill fileName returned by list_skills, for example telegram.md."),
      },
    },
    async ({ fileName }) => {
      const skill = await skills.readSkill(fileName);
      if (skill === null) {
        return jsonResult({ found: false, fileName, content: null });
      }
      const { content, patch, effectiveInstructions, ...metadata } = skill;
      return jsonResult({ found: true, fileName, content, patch, effectiveInstructions, metadata });
    },
  );
}
