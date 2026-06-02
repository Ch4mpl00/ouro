import { readFile, writeFile, mkdir } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

export interface CachedVector {
  id: number;
  embedding: number[];
}

export async function readCorpusCache(
  cacheDir: string,
  hash: string,
): Promise<Map<number, number[]> | null> {
  const path = join(cacheDir, `${hash}.jsonl`);
  if (!existsSync(path)) return null;
  const raw = await readFile(path, "utf-8");
  const map = new Map<number, number[]>();
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    const { id, embedding } = JSON.parse(line) as CachedVector;
    map.set(id, embedding);
  }
  return map;
}

export async function writeCorpusCache(
  cacheDir: string,
  hash: string,
  vectors: Map<number, number[]>,
): Promise<void> {
  const path = join(cacheDir, `${hash}.jsonl`);
  await mkdir(dirname(path), { recursive: true });
  const lines: string[] = [];
  for (const [id, embedding] of vectors) {
    lines.push(JSON.stringify({ id, embedding }));
  }
  await writeFile(path, lines.join("\n") + "\n", "utf-8");
}
