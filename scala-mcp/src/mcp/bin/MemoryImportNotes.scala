package mcp.bin

// Copies knowledge_base_notes into memory facts. Idempotent: provenance on
// each fact makes a re-run skip what is already there. The source table is
// left untouched.

import mcp.*
import zio.*

object MemoryImportNotes extends CliApp:
  def program(args: Chunk[String]) =
    for
      pool <- Cli.openPool
      embedder <- Cli.embedder
      notes <- KnowledgeRepository(pool, embedder).allNotes
      _ <- Console.printLine(s"[memory-import] ${notes.size} note(s) in knowledge_base_notes")
      memory = MemoryService.make(PgMemoryStore(pool), embedder)
      result <- MemoryService.importLegacyNotes(notes.map(n => MemoryService.LegacyNote(n.id, n.body, n.tags)), memory)
      (imported, skipped) = result
      _ <- Console.printLine(s"[memory-import] done: imported=$imported, skipped=$skipped")
      _ <- ZIO.when(imported > 0)(
        Console.printLine("[memory-import] run `embed-backfill` if the embedder was down during the import")
      )
    yield ()
