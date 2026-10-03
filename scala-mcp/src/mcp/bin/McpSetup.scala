package mcp.bin

// Idempotent state-database setup: connecting creates `mcp_state` if it is
// missing, applies its migrations and seeds the system tasks. The server does
// the same on boot; this does it without starting anything.

import mcp.*
import zio.*

object McpSetup extends CliApp:
  def program(args: Chunk[String]) = Cli.openDb *> Console.printLine("[setup:mcp] state database ready")
