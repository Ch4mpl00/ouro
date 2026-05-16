import path from "node:path";
import fs from "node:fs/promises";

// Agent-side skills resolver. Two-layer overlay:
//
//   skills/<name>.md          — live (gitignored). Mutable; `dreaming`
//                               writes here when it revises an instruction.
//   skills.default/<name>.md  — defaults (git-tracked). The shipped baseline.
//
// `readSkill(name)` returns the live version if present, else the default,
// else null. `saveSkill(name, content)` always writes to the live overlay —
// defaults are never touched at runtime, which preserves a clean reset
// point (delete the live file → fall back to default).
//
// Both dirs are anchored at the repo root; the supervisor runs from the
// agent package but the docker / dev layouts both place the repo root one
// level up from `packages/`. We resolve relative to this source file so
// the lookup works regardless of cwd.

const REPO_ROOT = path.resolve(import.meta.dirname, "../../..");
const LIVE_DIR = path.resolve(REPO_ROOT, "skills");
const DEFAULTS_DIR = path.resolve(REPO_ROOT, "skills.default");
const NAME_PATTERN = /^[a-z0-9][a-z0-9_-]*$/i;

function validateName(name: string): void {
  if (!NAME_PATTERN.test(name)) {
    throw new Error(`Invalid skill name "${name}". Use [a-z0-9][a-z0-9_-]* only.`);
  }
}

async function readIfExists(file: string): Promise<string | null> {
  try {
    return await fs.readFile(file, "utf-8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw err;
  }
}

export async function readSkill(name: string): Promise<string | null> {
  validateName(name);
  const live = await readIfExists(path.join(LIVE_DIR, `${name}.md`));
  if (live !== null) return live;
  return readIfExists(path.join(DEFAULTS_DIR, `${name}.md`));
}

export async function saveSkill(
  name: string,
  content: string,
): Promise<{ path: string; sizeBytes: number }> {
  validateName(name);
  await fs.mkdir(LIVE_DIR, { recursive: true });
  const target = path.join(LIVE_DIR, `${name}.md`);
  await fs.writeFile(target, content, "utf-8");
  return { path: target, sizeBytes: Buffer.byteLength(content, "utf-8") };
}

export interface SkillEntry {
  name: string;
  source: "live" | "default";
  sizeBytes: number;
  modifiedAt: string;
}

// Union of live + defaults, with `source` showing which layer is active
// for each name. If both exist, the live one wins (matches readSkill).
export async function listSkills(): Promise<SkillEntry[]> {
  const [liveEntries, defaultEntries] = await Promise.all([
    readDir(LIVE_DIR, "live"),
    readDir(DEFAULTS_DIR, "default"),
  ]);
  const byName = new Map<string, SkillEntry>();
  for (const e of defaultEntries) byName.set(e.name, e);
  for (const e of liveEntries) byName.set(e.name, e); // overwrites default
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

async function readDir(dir: string, source: "live" | "default"): Promise<SkillEntry[]> {
  let files: string[];
  try {
    files = await fs.readdir(dir);
  } catch {
    return [];
  }
  const out = await Promise.all(
    files
      .filter((f) => f.endsWith(".md"))
      .map(async (f) => {
        const stat = await fs.stat(path.join(dir, f));
        if (!stat.isFile()) return null;
        return {
          name: f.replace(/\.md$/, ""),
          source,
          sizeBytes: stat.size,
          modifiedAt: stat.mtime.toISOString(),
        } satisfies SkillEntry;
      }),
  );
  return out.filter((x): x is SkillEntry => x !== null);
}
