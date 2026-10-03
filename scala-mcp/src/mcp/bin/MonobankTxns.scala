package mcp.bin

// Print a statement as JSON. --account (0 = default UAH), --days (7, max 31).

import mcp.*
import zio.*
import zio.json.*

object MonobankTxns extends CliApp:
  def program(args: Chunk[String]) =
    for
      days <- Cli.intArg(args, "days", 7)
      _ <- ZIO.when(days < 1 || days > 31)(ZIO.fail(RuntimeException(s"--days must be between 1 and 31 (got $days)")))
      monobank <- Cli.http.map(Monobank.fromEnv)
      statement <- monobank.recent(Cli.arg(args, "account").getOrElse("0"), days)
      _ <- Console.printLine(statement.toJsonPretty)
    yield ()
