import path from "node:path";
import fs from "node:fs/promises";

// Skill files live at `<repo-root>/skills/<source>.md`. MCP launches with
// cwd at the repo root (.mcp.json invokes `pnpm mcp:serve` there), so the
// skills dir is reachable via process.cwd(). All access is sandboxed to
// this dir so the dreaming skill can edit any skill but nothing else on
// the filesystem.

const SKILLS_DIR = path.resolve(process.cwd(), "skills");
const NAME_PATTERN = /^[a-z0-9][a-z0-9_-]*$/i;

function resolveSkillPath(name: string): string {
  if (!NAME_PATTERN.test(name)) {
    throw new Error(`Invalid skill name "${name}". Use [a-z0-9][a-z0-9_-]* only.`);
  }
  const target = path.resolve(SKILLS_DIR, `${name}.md`);
  if (!target.startsWith(`${SKILLS_DIR}${path.sep}`) && target !== `${SKILLS_DIR}.md`) {
    throw new Error(`Resolved skill path escapes skills dir: ${target}`);
  }
  return target;
}

export async function listSkills(): Promise<{ name: string; sizeBytes: number; modifiedAt: string }[]> {
  let entries: string[];
  try {
    entries = await fs.readdir(SKILLS_DIR);
  } catch {
    return [];
  }
  const out = await Promise.all(
    entries
      .filter((f) => f.endsWith(".md"))
      .map(async (f) => {
        const full = path.join(SKILLS_DIR, f);
        const stat = await fs.stat(full);
        if (!stat.isFile()) return null;
        return {
          name: f.replace(/\.md$/, ""),
          sizeBytes: stat.size,
          modifiedAt: stat.mtime.toISOString(),
        };
      }),
  );
  return out
    .filter((x): x is { name: string; sizeBytes: number; modifiedAt: string } => x !== null)
    .sort((a, b) => a.name.localeCompare(b.name));
}

export async function readSkill(name: string): Promise<string> {
  return fs.readFile(resolveSkillPath(name), "utf-8");
}

export async function writeSkill(name: string, content: string): Promise<{ path: string; sizeBytes: number }> {
  const target = resolveSkillPath(name);
  await fs.mkdir(SKILLS_DIR, { recursive: true });
  await fs.writeFile(target, content, "utf-8");
  return { path: target, sizeBytes: Buffer.byteLength(content, "utf-8") };
}
