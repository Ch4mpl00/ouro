package mcp.bin

// Per-query top-k with gold/acceptable marks, for debugging labels.
//   eval-inspect --qids q-014,q-017 [--k 15] [--config <path>]

import mcp.*
import zio.*

import java.nio.file.Path

object EvalInspect extends CliApp:
  def program(args: Chunk[String]) =
    val dir = Path.of(Cli.EvalDir)
    val configPath = Cli.arg(args, "config").fold(dir.resolve("configs/baseline.json"))(Path.of(_))
    for
      raw <- ZIO.fromOption(Cli.arg(args, "qids")).orElseFail(RuntimeException("--qids q-014,q-017,... required"))
      qids = raw.split(',').map(_.trim).filter(_.nonEmpty).toList
      k <- Cli.intArg(args, "k", 15)
      _ <- ZIO.when(k <= 0)(ZIO.fail(RuntimeException("--k must be a positive integer")))
      key <- ZIO
        .fromOption(Env.get("OPENAI_API_KEY"))
        .orElseFail(RuntimeException("OPENAI_API_KEY is required (set in .env.mcp)"))
      config <- Eval.loadConfig(configPath)
      _ <- Console.printLine(s"[inspect] config: ${config.name} ($configPath)")
      _ <- Console.printLine(s"[inspect] qids: ${qids.mkString(", ")} · k=$k\n")
      http <- Cli.http
      embedder = OpenAiEmbedder(
        http,
        key,
        config.retrieval.embed.model,
        config.retrieval.embed.dimensions,
        Eval.MaxChars
      )
      out <- Eval.inspect(config, EvalPaths.under(dir), embedder, qids, k)
      _ <- Console.printLine(out)
    yield ()
