package mcp.bin

// One-time MTProto login for the userbot: phone → code → (2FA) → the session
// saved in integration_account, in the gramjs format every server reads.

import mcp.*
import zio.*

object UserbotAuth extends CliApp:
  def program(args: Chunk[String]) =
    for
      userbot <- Cli.openDb.flatMap(Userbot.make)
      _ <- Console.printLine("Starting Telegram userbot login (MTProto)...")
      prompts = MtLogin.Prompts(
        Cli.prompt("Phone number (e.g. +380501234567): "),
        Cli.prompt("Code from Telegram: "),
        hint => Cli.prompt(s"2FA password (hint: ${hint.getOrElse("none")}): ")
      )
      (account, username) <- UserbotLogin.run(userbot, prompts)
      _ <- Console.printLine(s"\n✓ Saved session for account $account${username.fold("")(u => s" (@$u)")}")
    yield ()
