package mcp

// read_file: a UTF-8 text file from disk, path absolute or relative to the
// repo root (MCP runs from it, so the working directory is the anchor).

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import zio.*
import zio.json.*

import java.nio.file.Files
import java.nio.file.Path

object FsTools:
  import Tools.*

  final case class ReadFileParams(@description("Absolute path or path relative to the repo root.") path: String)
      derives JsonDecoder,
        Schema

  final case class FileContent(path: String, content: String) derives JsonEncoder

  val tools: List[ToolDef] = List(
    tool(
      "read_file",
      "Read a text file",
      "Read a UTF-8 text file (markdown, txt, etc) and return its contents. Use this to load skill instructions like " +
        "`skills/telegram.md` when handling a signal. Path may be absolute or relative to the repo root."
    ) { (_, p: ReadFileParams) =>
      ZIO.attemptBlocking {
        val resolved = Path.of(p.path).toAbsolutePath
        FileContent(resolved.toString, Files.readString(resolved))
      }
    }
  )
