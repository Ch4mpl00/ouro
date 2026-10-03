package mcp.bin

// Score the RAG fixtures under one config; writes a markdown report.
//   eval-rag --config crates/mcp/eval/configs/baseline-dedup-003.json

import mcp.*
import zio.*

import java.nio.file.Files
import java.nio.file.Path
import java.time.LocalDateTime
import java.time.format.DateTimeFormatter

object EvalRag extends CliApp:
  def program(args: Chunk[String]) =
    val dir = Path.of(Cli.EvalDir)
    val configPath = Cli.arg(args, "config").fold(dir.resolve("configs/baseline.json"))(Path.of(_))
    for
      key <- ZIO
        .fromOption(Env.get("OPENAI_API_KEY"))
        .orElseFail(RuntimeException("OPENAI_API_KEY is required (set in .env.mcp)"))
      config <- Eval.loadConfig(configPath)
      _ <- Console.printLine(s"[eval-rag] config: ${config.name} ($configPath)")
      http <- Cli.http
      embedder = OpenAiEmbedder(
        http,
        key,
        config.retrieval.embed.model,
        config.retrieval.embed.dimensions,
        Eval.MaxChars
      )
      started <- Clock.nanoTime
      result <- Eval.run(config, EvalPaths.under(dir), embedder)
      elapsed <- Clock.nanoTime.map(t => (t - started) / 1_000_000)
      stamp = LocalDateTime.now.format(DateTimeFormatter.ofPattern("yyyyMMdd-HHmm"))
      path = dir.resolve("reports").resolve(s"${result.config.name}-$stamp.md")
      _ <- ZIO.attemptBlocking {
        Files.createDirectories(path.getParent); Files.writeString(path, Eval.renderMarkdown(result))
      }
      a = result.aggregate
      _ <- Console.printLine(
        List(
          s"\n[eval-rag] ${if result.cacheHit then "cache hit" else "fresh embed"} · ${elapsed}ms",
          s"[eval-rag] scored queries: ${a.scoredQueries}",
          f"[eval-rag] P@5   ${a.precisionAt5}%.3f   R@5   ${a.recallAt5}%.3f",
          f"[eval-rag] P@10  ${a.precisionAt10}%.3f   R@10  ${a.recallAt10}%.3f   R@30  ${a.recallAt30}%.3f",
          f"[eval-rag] MRR   ${a.mrr}%.3f",
          f"[eval-rag] uniq sources @5/@10  ${a.meanUniqueSourcesAt5}%.2f / ${a.meanUniqueSourcesAt10}%.2f",
          s"[eval-rag] report → $path"
        ).mkString("\n")
      )
    yield ()
