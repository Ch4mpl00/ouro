import os from "node:os";
import path from "node:path";
import fs from "node:fs/promises";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { createSkillsModule } from "./module";

describe("skills catalog", () => {
  let root: string;
  let liveDir: string;
  let defaultsDir: string;

  beforeEach(async () => {
    root = await fs.mkdtemp(path.join(os.tmpdir(), "mcp-skills-"));
    liveDir = path.join(root, "skills");
    defaultsDir = path.join(root, "skills.default");
    await Promise.all([
      fs.mkdir(liveDir, { recursive: true }),
      fs.mkdir(defaultsDir, { recursive: true }),
    ]);
  });

  afterEach(async () => {
    await fs.rm(root, { recursive: true, force: true });
  });

  it("lists exact filenames with useful metadata and applies the live overlay", async () => {
    await Promise.all([
      fs.writeFile(
        path.join(defaultsDir, "alpha.md"),
        "---\ntools: [search_news]\n---\n\n# Default Alpha\n\nDefault instructions.\n",
      ),
      fs.writeFile(
        path.join(defaultsDir, "beta.md"),
        "---\ntools: *\n---\n\n# Beta title\n\nBeta summary for selection.\n",
      ),
      fs.writeFile(
        path.join(liveDir, "alpha.md"),
        "---\ntools: []\n---\n\n# Live Alpha\n\nActive overlay instructions.\n",
      ),
      fs.writeFile(path.join(liveDir, "alpha.patch.md"), "not a standalone skill\n"),
    ]);

    const { catalog } = createSkillsModule({ liveDir, defaultsDir });
    const skills = await catalog.listSkills();

    expect(skills.map((skill) => skill.fileName)).toEqual(["alpha.md", "beta.md"]);
    expect(skills[0]).toMatchObject({
      id: "alpha",
      name: "alpha",
      fileName: "alpha.md",
      title: "Live Alpha",
      description: "Active overlay instructions.",
      tools: [],
      source: "live",
    });
    expect(skills[1]).toMatchObject({
      fileName: "beta.md",
      title: "Beta title",
      tools: "*",
      source: "default",
    });
  });

  it("reads the exact selected file contents and rejects stems or paths", async () => {
    const raw = "---\ntools: []\n---\n\n# Exact\n\nKeep every byte.\n";
    await fs.writeFile(path.join(defaultsDir, "exact.md"), raw);
    const { catalog } = createSkillsModule({ liveDir, defaultsDir });

    await expect(catalog.readSkill("exact.md")).resolves.toMatchObject({
      fileName: "exact.md",
      content: raw,
    });
    await expect(catalog.readSkill("missing.md")).resolves.toBeNull();
    await expect(catalog.readSkill("exact")).rejects.toThrow(/exact fileName/);
    await expect(catalog.readSkill("../exact.md")).rejects.toThrow(/exact fileName/);
  });
});
