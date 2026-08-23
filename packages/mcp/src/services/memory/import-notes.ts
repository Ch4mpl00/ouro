// One-shot import of knowledge_base_notes into memory facts (D2).
//
// Notes carry their text verbatim, so the import is a copy, not a
// transformation: the original phrasing is exactly what a later structuring
// pass will need. Provenance goes into `source`, which also makes the import
// re-runnable — a second run finds the fact already there and skips it.

import type { Indexer } from "./indexer";
import type { MemoryStore } from "./store";

export interface LegacyNote {
  id: number;
  body: string;
  tags: string[];
  source: string | null;
}

export interface ImportNotesResult {
  imported: number;
  skipped: number;
}

export function legacyNoteSource(noteId: number): string {
  return `knowledge_base_notes:${noteId}`;
}

export interface ImportNotesDeps {
  store: MemoryStore;
  indexer: Indexer;
}

export async function importLegacyNotes(
  notes: LegacyNote[],
  deps: ImportNotesDeps,
): Promise<ImportNotesResult> {
  let imported = 0;
  let skipped = 0;

  for (const note of notes) {
    const provenance = legacyNoteSource(note.id);
    if (await deps.store.getFactBySource(provenance)) {
      skipped++;
      continue;
    }
    const body = note.body.trim();
    if (body.length === 0) {
      skipped++;
      continue;
    }
    const fact = await deps.store.createFact({ body, tags: note.tags, source: provenance });
    try {
      await deps.indexer.indexFact(fact);
    } catch (err) {
      // Same contract as every other write: the row lands, searchability
      // catches up on backfill.
      console.error(`[memory-import] indexing fact ${fact.id} failed:`, err);
    }
    imported++;
  }

  return { imported, skipped };
}
