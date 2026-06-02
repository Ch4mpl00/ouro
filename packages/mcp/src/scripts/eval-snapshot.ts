import "dotenv/config";
import { writeFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { sql } from "drizzle-orm";
import { createPgClient } from "../db/pg/client";

// Dumps a point-in-time snapshot of news_items into a JSONL fixture
// for the RAG eval harness. Body + metadata + postedAt go in; embedding
// does NOT — each eval config re-embeds the corpus from text so we can
// compare embedding models / chunker strategies / buildText variants.
//
//   pnpm eval:snapshot                       # most recent 2000 rows
//   pnpm eval:snapshot -- --limit 5000       # bigger window
//   pnpm eval:snapshot -- --out <path>       # default: packages/mcp/src/eval/fixtures/corpus.jsonl

const DEFAULT_LIMIT = 2000;

interface SnapshotRow {
  id: number;
  source: string;
  externalId: string;
  title: string | null;
  url: string | null;
  body: string;
  metadata: Record<string, unknown>;
  postedAt: string | null;
}

function parseArg(name: string): string | undefined {
  const idx = process.argv.indexOf(name);
  return idx !== -1 ? process.argv[idx + 1] : undefined;
}

function parseOutPath(): string {
  const raw = parseArg("--out");
  if (raw) return resolve(raw);
  const here = dirname(fileURLToPath(import.meta.url));
  return resolve(here, "../eval/fixtures/corpus.jsonl");
}

function parseLimit(): number {
  const raw = parseArg("--limit");
  if (!raw) return DEFAULT_LIMIT;
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) {
    throw new Error(`--limit must be a positive number, got ${raw}`);
  }
  return Math.floor(n);
}

async function main(): Promise<void> {
  const outPath = parseOutPath();
  const limit = parseLimit();
  const pg = createPgClient();
  await pg.ensureReady();

  try {
    // Pick the most recent `limit` rows (by posted_at, falling back to
    // fetched_at for rows without a parsed publication date), then
    // re-order by id for a stable file diff across re-runs. Drop
    // body-less rows — useless for retrieval comparison.
    const rows = (await pg.db.execute(sql`
      SELECT id, source, "externalId", title, url, body, metadata, "postedAt"
        FROM (
          SELECT id::bigint AS id,
                 source,
                 external_id AS "externalId",
                 title,
                 url,
                 body,
                 metadata,
                 posted_at AS "postedAt",
                 COALESCE(posted_at, fetched_at) AS sort_at
            FROM news_items
           WHERE length(body) > 0
           ORDER BY sort_at DESC
           LIMIT ${limit}
        ) sub
       ORDER BY id ASC
    `)) as unknown as { rows: SnapshotRow[] } | SnapshotRow[];

    const list: SnapshotRow[] = Array.isArray(rows) ? rows : rows.rows;

    await mkdir(dirname(outPath), { recursive: true });
    const lines = list.map((r) => {
      const out: SnapshotRow = {
        id: Number(r.id),
        source: r.source,
        externalId: r.externalId,
        title: r.title,
        url: r.url,
        body: r.body,
        metadata: r.metadata ?? {},
        postedAt: r.postedAt ? new Date(r.postedAt).toISOString() : null,
      };
      return JSON.stringify(out);
    });
    await writeFile(outPath, lines.join("\n") + "\n", "utf-8");

    const bySource = list.reduce<Record<string, number>>((acc, r) => {
      acc[r.source] = (acc[r.source] ?? 0) + 1;
      return acc;
    }, {});
    console.log(
      `[eval-snapshot] wrote ${list.length} rows (limit=${limit}) → ${outPath}`,
    );
    console.log(`[eval-snapshot] by source:`, bySource);
  } finally {
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[eval-snapshot] crashed:", err);
  process.exitCode = 1;
});
