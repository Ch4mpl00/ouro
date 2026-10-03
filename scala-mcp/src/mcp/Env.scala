package mcp

// Process configuration: the real environment, plus `.env`-style files from
// the working directory for local runs (the container gets its env injected).
// Like dotenv, a file never overrides a variable the process already has, and
// only the named files in the current directory are read — never a parent's,
// which on a dev machine can hold some other checkout's real credentials.

import zio.*

import java.nio.file.Files
import java.nio.file.Path
import scala.jdk.CollectionConverters.*

object Env:
  @volatile private var fromFiles: Map[String, String] = Map.empty

  def get(name: String): Option[String] = sys.env.get(name).orElse(fromFiles.get(name)).filter(_.trim.nonEmpty)

  // Once, at the top of a `main`, before anything reads configuration.
  def loadFiles(names: String*): UIO[Unit] =
    ZIO.succeed {
      val loaded = names.toList.flatMap { name =>
        val path = Path.of(name)
        if Files.isRegularFile(path) then parse(Files.readAllLines(path).asScala.toList) else Nil
      }
      // The first file to name a variable wins, like successive dotenv loads.
      fromFiles = loaded.reverse.toMap
    }

  def parse(lines: List[String]): List[(String, String)] =
    lines.map(_.trim).filter(l => l.nonEmpty && !l.startsWith("#")).flatMap { line =>
      val body = line.stripPrefix("export ").trim
      body.indexOf('=') match
        case -1 => None
        case i  =>
          val key = body.take(i).trim
          val raw = body.drop(i + 1).trim
          val value =
            if raw.length >= 2 && (raw.startsWith("\"") && raw.endsWith("\"") || raw
                .startsWith("'") && raw.endsWith("'"))
            then raw.drop(1).dropRight(1)
            else raw.takeWhile(_ != '#').trim
          Option.when(key.nonEmpty)(key -> value)
    }
