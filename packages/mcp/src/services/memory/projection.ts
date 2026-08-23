// The search projection (D8): how a document becomes rows in `memory_index`,
// and how hits come back ranked.
//
// Pure on purpose. Chunking granularity and the recency/relevance trade-off
// are the two things most likely to be tuned later, and neither should need a
// database to change or to test.

import type { IndexHit } from "./types";

export const DEFAULT_CHUNK_CHARS = 1200;

export interface MarkdownChunk {
  text: string;
  // Breadcrumb of the headings this chunk sits under ("Progress > Notes").
  // Empty for text before the first heading.
  headingPath: string;
}

// Splits on blank lines and packs paragraphs up to `maxChars`, never breaking
// a paragraph in half unless it is oversized on its own. A month-long
// progress.md must not become one vector — that is the point of chunking a
// document the read model still stores whole (D8, reason 1).
export function chunkMarkdown(body: string, maxChars: number = DEFAULT_CHUNK_CHARS): MarkdownChunk[] {
  const chunks: MarkdownChunk[] = [];
  const stack: { level: number; text: string }[] = [];
  let buffer: string[] = [];
  let bufferPath = "";

  const pathNow = (): string => stack.map((h) => h.text).join(" > ");

  const flush = (): void => {
    const text = buffer.join("\n\n").trim();
    buffer = [];
    if (text.length > 0) chunks.push({ text, headingPath: bufferPath });
  };

  for (const block of body.split(/\n\s*\n/)) {
    const paragraph = block.trim();
    if (paragraph.length === 0) continue;

    const heading = /^(#{1,6})\s+(.+?)\s*$/.exec(paragraph);
    if (heading) {
      // A heading starts a new chunk: it is the strongest topical boundary a
      // markdown document offers.
      flush();
      const level = heading[1]!.length;
      while (stack.length > 0 && stack[stack.length - 1]!.level >= level) stack.pop();
      stack.push({ level, text: heading[2]! });
      bufferPath = pathNow();
      continue;
    }

    if (buffer.length === 0) bufferPath = pathNow();

    const projected = [...buffer, paragraph].join("\n\n").length;
    if (buffer.length > 0 && projected > maxChars) {
      flush();
      bufferPath = pathNow();
    }

    if (paragraph.length > maxChars) {
      // An oversized single paragraph (a pasted log, a long table) still has
      // to fit the embedder, so it is cut on length alone.
      for (let at = 0; at < paragraph.length; at += maxChars) {
        chunks.push({ text: paragraph.slice(at, at + maxChars), headingPath: pathNow() });
      }
      continue;
    }
    buffer.push(paragraph);
  }
  flush();

  return chunks;
}

export interface IndexTextInput {
  projectTitle: string;
  docName: string;
  headingPath: string;
  text: string;
}

// Chunks must be self-contained; documents must not (D8, reason 2). A chunk
// that reads "застрял на Dijkstra" matches nothing useful on its own, so the
// indexer denormalises the subject into the embedded text while the stored
// document stays clean.
export function buildIndexText(input: IndexTextInput): string {
  const where = input.headingPath.length > 0
    ? `${input.projectTitle} — ${input.docName} · ${input.headingPath}`
    : `${input.projectTitle} — ${input.docName}`;
  return `${where}\n\n${input.text}`;
}

export interface RankOpts {
  now: Date;
  // Days for the recency bonus to halve.
  halfLifeDays: number;
  // How much a brand-new row may improve its cosine distance. Small on
  // purpose: recency breaks ties and lifts fresh context, it must not float
  // an irrelevant note above a relevant one (D7).
  recencyWeight: number;
}

export const DEFAULT_RANK: Omit<RankOpts, "now"> = { halfLifeDays: 30, recencyWeight: 0.05 };

export interface RankedHit extends IndexHit {
  score: number;
}

export function rankHits(hits: IndexHit[], opts: RankOpts): RankedHit[] {
  return hits
    .map((hit) => {
      const ageDays = Math.max(0, (opts.now.getTime() - new Date(hit.ts).getTime()) / 86_400_000);
      const boost = opts.recencyWeight * 2 ** (-ageDays / opts.halfLifeDays);
      return { ...hit, score: hit.distance - boost };
    })
    .sort((a, b) => a.score - b.score);
}
