package mcp.bin

// One-time Gmail OAuth: consent URL → paste the `code` → tokens stored.

import mcp.*
import zio.*

object GmailAuth extends CliApp:
  def program(args: Chunk[String]) =
    for
      gmail <- Cli.openDb.zip(Cli.http).map(GmailModule(_, _))
      url <- gmail.authUrl
      _ <- Console.printLine(s"\n1) Open this URL in a browser and grant access:\n\n$url")
      _ <- Console.printLine(
        "\n2) After consent, Google will redirect to your GOOGLE_REDIRECT_URI with a `code` query param."
      )
      _ <- Console.printLine("   Copy that `code` value and paste it below.\n")
      code <- Cli.prompt("code: ")
      _ <- ZIO.when(code.isEmpty)(ZIO.fail(RuntimeException("No code provided")))
      account <- gmail.exchangeCodeAndPersist(code)
      _ <- Console.printLine(s"\nGmail authorized for $account")
    yield ()
