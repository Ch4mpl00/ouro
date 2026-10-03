package mcp.bin

// Re-attempts embeddings left NULL by a failed inline embed, in every
// embedded store. Safe to re-run.
//
//   docker compose exec mcp embed-backfill

import mcp.*
import zio.*

object EmbedBackfill extends CliApp:
  private val Batch = 100

  // One batch at a time until empty — or until a whole batch fails, which
  // points at auth/API trouble that retrying won't fix.
  private def drain(label: String, next: Task[EmbedResult]): Task[Unit] =
    def loop(total: EmbedResult): Task[EmbedResult] =
      next.flatMap { r =>
        if r.embedded == 0 && r.failed == 0 then ZIO.succeed(total)
        else
          val sum = total + r
          Console.printLine(
            s"[embed-backfill:$label] batch: embedded=${r.embedded}, failed=${r.failed} (running totals: ${sum.embedded}/${sum.failed})"
          ) *> (if r.failed > 0 then Console.printLineError(s"[embed-backfill:$label] giving up after batch failure").as(sum)
                else loop(sum))
      }
    loop(EmbedResult()).flatMap(t => Console.printLine(s"[embed-backfill:$label] done: embedded=${t.embedded}, failed=${t.failed}"))

  def program(args: Chunk[String]) =
    for
      pool <- Cli.openPool
      embedder <- Cli.embedder
      memory = MemoryService.make(PgMemoryStore(pool), embedder)
      _ <- drain("news", NewsRepository(pool, embedder).embedMissingBatch(Batch))
      _ <- drain("knowledge", KnowledgeRepository(pool, embedder).embedMissingBatch(Batch))
      _ <- drain("memory", memory.indexer.embedMissingBatch(Batch))
    yield ()
