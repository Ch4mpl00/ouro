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

// A parsed skill: the body (markdown after frontmatter) plus the declared
// `tools:` allow-list from the frontmatter. Every skill MUST declare a
// `tools:` field. Three forms:
//
//   tools: []           — grants no MCP tools (meta-skills, e.g. routing).
//   tools: [a, b, c]    — explicit allow-list of MCP tool names.
//   tools: *            — wildcard: all MCP tools available.
//
// The wildcard exists for catch-all skills (telegram, scheduler) where
// enumerating 14 tools by hand is brittle and adds nothing.
export interface SkillFile {
  body: string;
  tools: string[] | "*";
  source: "live" | "default";
}

const FRONTMATTER_RE = /^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n/;
const TOOLS_WILDCARD_RE = /^tools:\s*\*\s*$/m;
const TOOLS_ARRAY_RE = /^tools:\s*\[(.*?)\]\s*$/m;
const TOOL_NAME_RE = /^[a-z_][a-z0-9_]*$/;

function parseSkillFile(name: string, raw: string, source: "live" | "default"): SkillFile {
  const m = FRONTMATTER_RE.exec(raw);
  if (!m) {
    throw new Error(
      `skill "${name}" (${source}): missing frontmatter. ` +
        `Every skill must start with a \`---\\ntools: ...\\n---\` block. ` +
        `Use \`tools: *\` for all MCP tools, \`tools: []\` for none, or ` +
        `\`tools: [a, b, c]\` for an explicit allow-list.`,
    );
  }
  const frontmatter = m[1] ?? "";
  const body = raw.slice(m[0].length);

  if (TOOLS_WILDCARD_RE.test(frontmatter)) {
    return { body, tools: "*", source };
  }

  const t = TOOLS_ARRAY_RE.exec(frontmatter);
  if (!t) {
    throw new Error(
      `skill "${name}" (${source}): frontmatter must declare \`tools: *\`, ` +
        `\`tools: []\`, or \`tools: [a, b, c]\`.`,
    );
  }
  const inner = (t[1] ?? "").trim();
  const tools = inner === ""
    ? []
    : inner.split(",").map((s) => s.trim()).filter((s) => s.length > 0);
  for (const tool of tools) {
    if (!TOOL_NAME_RE.test(tool)) {
      throw new Error(
        `skill "${name}" (${source}): tool name "${tool}" is not a valid identifier`,
      );
    }
  }
  return { body, tools, source };
}

export async function readSkill(name: string): Promise<SkillFile | null> {
  validateName(name);
  const live = await readIfExists(path.join(LIVE_DIR, `${name}.md`));
  if (live !== null) return parseSkillFile(name, live, "live");
  const def = await readIfExists(path.join(DEFAULTS_DIR, `${name}.md`));
  if (def !== null) return parseSkillFile(name, def, "default");
  return null;
}

// Walk every skill file on disk (defaults ∪ live) and parse it. Throws on
// the first broken frontmatter / unknown tool. Called once at engine
// startup so misconfiguration crashes the agent up front, not mid-signal.
//
// `knownMcpTools` is the list of MCP tool names the engine sees right
// now. Any declared tool that's not in this set is rejected (catches
// typos like `set_memrory`). Pass an empty array to skip MCP cross-check.
export async function validateAllSkills(knownMcpTools: string[]): Promise<void> {
  const known = new Set(knownMcpTools);
  const entries = await listSkills();
  const errors: string[] = [];
  for (const e of entries) {
    let parsed: SkillFile | null;
    try {
      parsed = await readSkill(e.name);
    } catch (err) {
      errors.push((err as Error).message);
      continue;
    }
    if (!parsed) continue;
    if (known.size === 0) continue;
    if (parsed.tools === "*") continue; // wildcard — nothing to cross-check
    for (const tool of parsed.tools) {
      if (!known.has(tool)) {
        errors.push(
          `skill "${e.name}" (${parsed.source}): declares tool "${tool}" which is not in the MCP registry. ` +
            `Known tools: ${[...known].sort().join(", ")}`,
        );
      }
    }
  }
  if (errors.length > 0) {
    throw new Error(
      `Skill validation failed (${errors.length} issue(s)):\n` +
        errors.map((e) => `  - ${e}`).join("\n"),
    );
  }
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
