package mcp.bin

// Debug helper: list matching mail. --account, --query (is:unread), --limit (10).

import mcp.*
import zio.*

object GmailListUnread extends CliApp:
  def program(args: Chunk[String]) =
    for
      gmail <- Cli.openDb.zip(Cli.http).map(GmailModule(_, _))
      account <- Cli.arg(args, "account") match
        case Some(a) => ZIO.succeed(a)
        case None    =>
          gmail.resolveAccountKey.someOrFail(RuntimeException("No Gmail account in DB. Run `pnpm gmail:auth` first."))
      query = Cli.arg(args, "query").getOrElse("is:unread")
      limit <- Cli.intArg(args, "limit", 10)
      page <- gmail.listMessages(account, query, limit, None)
      _ <- Console.printLine(s"\n$account — query=`$query` — ${page.messages.size} match(es)\n")
      _ <- ZIO.foreachDiscard(page.messages) { m =>
        Console.printLine(
          (List(s"• ${m.subject.getOrElse("(no subject)")}", s"  from: ${m.from.getOrElse("?")}") ++
            m.date.map(d => s"  date: $d") ++ Option.when(m.snippet.nonEmpty)(s"  ${m.snippet}") :+ "").mkString("\n")
        )
      }
    yield ()
