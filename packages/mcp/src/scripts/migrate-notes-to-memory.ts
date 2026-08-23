import "dotenv/config";
import { createPgClient } from "../db/pg/client";
import { knowledgeBaseNotes } from "../db/pg/schema";
import { createMemoryModule } from "../services/memory";
import { importLegacyNotes, type LegacyNote } from "../services/memory";
import { createPgMemoryStore } from "../services/memory";

// Copies knowledge_base_notes into memory facts (D2 of
// .claude/tasks/unified-memory.md). Idempotent — provenance is recorded on
// each fact, so a re-run skips what is already there. The source table is
// left untouched: find_notes keeps working until its skill references go.
//
//   docker compose exec -w /app mcp pnpm memory:import-notes

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();

  try {
    const rows = await pg.db
      .select({
        id: knowledgeBaseNotes.id,
        body: knowledgeBaseNotes.body,
        tags: knowledgeBaseNotes.tags,
        source: knowledgeBaseNotes.source,
      })
      .from(knowledgeBaseNotes);

    const notes: LegacyNote[] = rows.map((row) => ({
      id: Number(row.id),
      body: row.body,
      tags: row.tags ?? [],
      source: row.source,
    }));
    console.log(`[memory-import] ${notes.length} note(s) in knowledge_base_notes`);

    const { indexer } = createMemoryModule({ db: pg.db });
    const store = createPgMemoryStore({ db: pg.db });
    const result = await importLegacyNotes(notes, { store, indexer });

    console.log(`[memory-import] done: imported=${result.imported}, skipped=${result.skipped}`);
    if (result.imported > 0) {
      // Same contract as every other write: rows land first, vectors may lag.
      console.log("[memory-import] run `pnpm embed:backfill` if the embedder was down during the import");
    }
  } finally {
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[memory-import] crashed:", err);
  process.exitCode = 1;
});
