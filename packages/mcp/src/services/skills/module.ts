import path from "node:path";
import fs from "node:fs/promises";
import type { Dirent } from "node:fs";

const SKILL_NAME_RE = /^[a-z0-9][a-z0-9_-]*$/i;
const FRONTMATTER_RE = /^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n/;
const TOOLS_WILDCARD_RE = /^tools:\s*\*\s*$/m;
const TOOLS_ARRAY_RE = /^tools:\s*\[(.*?)\]\s*$/m;

export type SkillSource = "live" | "default";

export interface SkillSummary {
  id: string;
  name: string;
  fileName: string;
  title: string;
  description: string;
  tools: string[] | "*" | null;
  source: SkillSource;
  sizeBytes: number;
  modifiedAt: string;
}

export interface SkillDocument extends SkillSummary {
  content: string;
}

export interface SkillCatalog {
  listSkills(): Promise<SkillSummary[]>;
  readSkill(fileName: string): Promise<SkillDocument | null>;
}

export interface SkillsModule {
  catalog: SkillCatalog;
}

export interface SkillsModuleDeps {
  liveDir: string;
  defaultsDir: string;
}

interface SkillFileRef {
  name: string;
  fileName: string;
  file: string;
  source: SkillSource;
  modifiedAtMs: number;
}

function isMissingFile(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

function validateFileName(fileName: string): string {
  if (!fileName.endsWith(".md")) {
    throw new Error(
      `Invalid skill filename "${fileName}". Use an exact fileName returned by list_skills.`,
    );
  }
  const name = fileName.slice(0, -3);
  if (!SKILL_NAME_RE.test(name)) {
    throw new Error(
      `Invalid skill filename "${fileName}". Use an exact fileName returned by list_skills.`,
    );
  }
  return fileName;
}

function bodyWithoutFrontmatter(raw: string): string {
  const match = FRONTMATTER_RE.exec(raw);
  return match ? raw.slice(match[0].length) : raw;
}

function fallbackTitle(name: string): string {
  return name
    .split(/[-_]+/)
    .filter((part) => part.length > 0)
    .map((part) => `${part[0]?.toUpperCase() ?? ""}${part.slice(1)}`)
    .join(" ");
}

function extractTitle(name: string, raw: string): string {
  const heading = /^#\s+(.+?)\s*$/m.exec(bodyWithoutFrontmatter(raw));
  return heading?.[1]?.trim() || fallbackTitle(name);
}

function cleanMarkdown(text: string): string {
  return text
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\[([^\]]+)]\([^)]+\)/g, "$1")
    .replace(/[*_~]/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

function extractDescription(raw: string, title: string): string {
  const body = bodyWithoutFrontmatter(raw);
  const heading = /^#\s+.+?\s*$/m.exec(body);
  const afterTitle = heading?.index === undefined
    ? body
    : body.slice(heading.index + heading[0].length);
  const paragraph = afterTitle
    .split(/\r?\n\s*\r?\n/)
    .map((block) => block.trim())
    .find((block) =>
      block.length > 0 &&
      !block.startsWith("#") &&
      !block.startsWith("```") &&
      !block.startsWith("|") &&
      !/^[-*]\s/.test(block),
    );
  const description = cleanMarkdown(paragraph ?? `Instructions for ${title}.`);
  return description.length <= 280 ? description : `${description.slice(0, 277).trimEnd()}...`;
}

function extractTools(raw: string): string[] | "*" | null {
  const frontmatter = FRONTMATTER_RE.exec(raw)?.[1];
  if (frontmatter === undefined) return null;
  if (TOOLS_WILDCARD_RE.test(frontmatter)) return "*";
  const array = TOOLS_ARRAY_RE.exec(frontmatter)?.[1];
  if (array === undefined) return null;
  if (array.trim() === "") return [];
  return array
    .split(",")
    .map((tool) => tool.trim())
    .filter((tool) => tool.length > 0);
}

async function listSkillFiles(dir: string, source: SkillSource): Promise<SkillFileRef[]> {
  let entries: Dirent[];
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch (error) {
    if (isMissingFile(error)) return [];
    throw error;
  }

  const refs = await Promise.all(
    entries
      .filter((entry) =>
        entry.isFile() && entry.name.endsWith(".md") && !entry.name.endsWith(".patch.md"),
      )
      .map(async (entry): Promise<SkillFileRef | null> => {
        const name = entry.name.slice(0, -3);
        if (!SKILL_NAME_RE.test(name)) return null;
        const file = path.join(dir, entry.name);
        const stat = await fs.stat(file);
        return { name, fileName: entry.name, file, source, modifiedAtMs: stat.mtimeMs };
      }),
  );
  return refs.filter((ref): ref is SkillFileRef => ref !== null);
}

export function createSkillsModule(deps: SkillsModuleDeps): SkillsModule {
  const { liveDir, defaultsDir } = deps;

  async function activeFiles(): Promise<Map<string, SkillFileRef>> {
    const [defaults, live] = await Promise.all([
      listSkillFiles(defaultsDir, "default"),
      listSkillFiles(liveDir, "live"),
    ]);
    const files = new Map<string, SkillFileRef>();
    for (const ref of defaults) files.set(ref.fileName, ref);
    for (const ref of live) files.set(ref.fileName, ref);
    return files;
  }

  async function readFromRef(ref: SkillFileRef): Promise<SkillDocument> {
    const content = await fs.readFile(ref.file, "utf-8");
    const title = extractTitle(ref.name, content);
    return {
      id: ref.name,
      name: ref.name,
      fileName: ref.fileName,
      title,
      description: extractDescription(content, title),
      tools: extractTools(content),
      source: ref.source,
      sizeBytes: Buffer.byteLength(content, "utf-8"),
      modifiedAt: new Date(ref.modifiedAtMs).toISOString(),
      content,
    };
  }

  const catalog: SkillCatalog = {
    async listSkills() {
      const files = await activeFiles();
      const documents = await Promise.all([...files.values()].map(readFromRef));
      return documents
        .map(({ content: _content, ...summary }) => summary)
        .sort((a, b) => a.name.localeCompare(b.name));
    },

    async readSkill(fileName) {
      const validated = validateFileName(fileName);
      const ref = (await activeFiles()).get(validated);
      return ref ? readFromRef(ref) : null;
    },
  };

  return { catalog };
}
