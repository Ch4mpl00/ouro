package mcp.bin

// Dump the most recent news_items into the eval corpus fixture (no
// embeddings — each config re-embeds from text). Re-ordered by id so re-runs
// diff cleanly; body-less rows dropped.
//   eval-snapshot [--limit 2000] [--out crates/mcp/eval/fixtures/corpus.jsonl]

import mcp.*
import mcp.Rows.*
import zio.*
import zio.json.*
import zio.json.ast.Json

import java.nio.file.Files
import java.nio.file.Path

object EvalSnapshot extends CliApp:
  def program(args: Chunk[String]) =
    val out = Cli.arg(args, "out").fold(Path.of(Cli.EvalDir).resolve("fixtures/corpus.jsonl"))(Path.of(_))
    for
      limit <- Cli.intArg(args, "limit", 2000)
      _ <- ZIO.when(limit <= 0)(ZIO.fail(RuntimeException(s"--limit must be a positive number, got $limit")))
      pool <- Cli.openPool
      rows <- pool.query(
        """SELECT id, source, external_id, title, url, body, metadata, posted_at FROM (
             SELECT *, COALESCE(posted_at, fetched_at) AS sort_at FROM news_items
              WHERE length(body) > 0 ORDER BY sort_at DESC LIMIT ?
           ) sub ORDER BY id ASC""",
        limit
      ) { rs =>
        rs.getString("source") -> Json.Obj(
          "id" -> Json.Num(rs.getLong("id")),
          "source" -> Json.Str(rs.getString("source")),
          "externalId" -> Json.Str(rs.getString("external_id")),
          "title" -> rs.optString("title").fold(Json.Null)(Json.Str(_)),
          "url" -> rs.optString("url").fold(Json.Null)(Json.Str(_)),
          "body" -> Json.Str(rs.getString("body")),
          "metadata" -> NewsRepository.metadataOf(rs),
          "postedAt" -> rs.optInstant("posted_at").fold(Json.Null)(t => Json.Str(Time.iso(t)))
        )
      }
      _ <- ZIO.attemptBlocking {
        Option(out.getParent).foreach(Files.createDirectories(_))
        Files.writeString(out, rows.map(_._2.toJson + "\n").mkString)
      }
      bySource = rows.groupMapReduce(_._1)(_ => 1)(_ + _).toList.sorted
      _ <- Console.printLine(s"[eval-snapshot] wrote ${rows.size} rows (limit=$limit) → $out")
      _ <- Console.printLine(
        s"[eval-snapshot] by source: ${bySource.map((s, n) => s"\"$s\": $n").mkString("{", ", ", "}")}"
      )
    yield ()
