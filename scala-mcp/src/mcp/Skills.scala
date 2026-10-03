package mcp

// The skills export: the agent's skill files (live overlay `skills/` over the
// shipped `skills.default/`), read-only, for clients that want to see what
// the agent runs — the ChatGPT tunnel, mostly.
//
// Sections:
//   1. catalog — active files, metadata, and the patch composition
//   2. tools   — `skills` toolset: list_skills, read_skill
//
// The improver writes an append-only overlay at `skills/<name>.patch.md`,
// and the agent glues it onto the end of the body (agent `appendPatch`). The
// two packages may not share code, so `appendPatch` here re-implements it
// byte for byte; a test pins the composed output as a literal.

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import zio.*
import zio.json.*
import zio.json.ast.Json

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.Files
import java.nio.file.NoSuchFileException
import java.nio.file.Path
import scala.jdk.CollectionConverters.*

// ── 1. catalog ───────────────────────────────────────────────────────────────

enum SkillSource:
  case live, default

// `tools: *` or `tools: [a, b]` from the frontmatter.
enum SkillTools:
  case All
  case Only(names: List[String])

object SkillTools:
  given JsonEncoder[SkillTools] = JsonEncoder[Json].contramap {
    case All         => Json.Str("*")
    case Only(names) => Json.Arr(names.map(Json.Str(_))*)
  }

final case class SkillSummary(
    id: String,
    name: String,
    fileName: String,
    title: String,
    description: String,
    tools: Option[SkillTools],
    source: SkillSource,
    sizeBytes: Int,
    modifiedAt: String,
    // Whether an improver patch is in force.
    patched: Boolean
)

object SkillSummary:
  given JsonEncoder[SkillSource] = JsonEncoder[String].contramap(_.toString)
  given JsonEncoder[SkillSummary] = DeriveJsonEncoder.gen[SkillSummary]

final case class SkillDocument(
    summary: SkillSummary,
    // The exact bytes of the active file — the editable source.
    content: String,
    // The exact bytes of the overlay, or None.
    patch: Option[String],
    // What the agent runs: body without frontmatter + patch.
    effectiveInstructions: String
)

final class SkillCatalog(liveDir: Path, defaultsDir: Path):
  import SkillCatalog.*

  private final case class FileRef(name: String, fileName: String, path: Path, source: SkillSource)

  private def listFiles(dir: Path, source: SkillSource): Task[List[FileRef]] =
    ZIO.attemptBlocking {
      if !Files.isDirectory(dir) then Nil
      else
        val stream = Files.list(dir)
        try
          stream.iterator.asScala.toList.flatMap { path =>
            val fileName = path.getFileName.toString
            val name = fileName.stripSuffix(".md")
            Option.when(
              Files.isRegularFile(path) && fileName.endsWith(".md") && !fileName.endsWith(".patch.md") && nameOk(name)
            )(FileRef(name, fileName, path, source))
          }
        finally stream.close()
    }

  // Live overrides default, file by file.
  private def activeFiles: Task[Map[String, FileRef]] =
    for
      defaults <- listFiles(defaultsDir, SkillSource.default)
      live <- listFiles(liveDir, SkillSource.live)
    yield (defaults ++ live).map(f => f.fileName -> f).toMap

  // Live layer only: defaults never ship a patch, and a deleted patch must
  // revert the skill rather than linger.
  private def readPatch(name: String): Task[Option[String]] =
    ZIO.attemptBlocking(Some(Files.readString(liveDir.resolve(s"$name.patch.md")))).catchSome {
      case _: NoSuchFileException => ZIO.none
    }

  private def readRef(file: FileRef): Task[SkillDocument] =
    for
      content <- ZIO.attemptBlocking(Files.readString(file.path))
      modified <- ZIO.attemptBlocking(Files.getLastModifiedTime(file.path).toInstant)
      patch <- readPatch(file.name)
    yield
      val title = extractTitle(file.name, content)
      val summary = SkillSummary(
        file.name,
        file.name,
        file.fileName,
        title,
        extractDescription(content, title),
        extractTools(content),
        file.source,
        content.getBytes(UTF_8).length,
        Time.iso(modified),
        patch.exists(_.trim.nonEmpty)
      )
      SkillDocument(summary, content, patch, appendPatch(bodyWithoutFrontmatter(content), patch.getOrElse("")))

  def listSkills: Task[List[SkillSummary]] =
    activeFiles
      .flatMap(files => ZIO.foreach(files.values.toList)(readRef))
      .map(_.map(_.summary).sortBy(_.name.toLowerCase))

  def readSkill(fileName: String): Task[Option[SkillDocument]] =
    val valid = fileName.endsWith(".md") && nameOk(fileName.stripSuffix(".md"))
    if !valid then Tools.fail(s"""Invalid skill filename "$fileName". Use an exact fileName returned by list_skills.""")
    else activeFiles.flatMap(files => ZIO.foreach(files.get(fileName))(readRef))

object SkillCatalog:
  val PatchMarker = "<!-- improver-patch -->"

  private val NameRe = "(?i)^[a-z0-9][a-z0-9_-]*$".r
  private val FrontmatterRe = """^---\s*\r?\n([\s\S]*?)\r?\n---\s*\r?\n""".r
  private val HeadingRe = """(?m)^#\s+(.+?)\s*$""".r

  def nameOk(name: String): Boolean = NameRe.matches(name)

  def bodyWithoutFrontmatter(raw: String): String =
    FrontmatterRe.findPrefixMatchOf(raw).fold(raw)(m => raw.substring(m.end))

  // Byte-for-byte the agent's `appendPatch`.
  def appendPatch(body: String, patch: String): String =
    val trimmed = patch.trim
    if trimmed.isEmpty then body else s"${body.stripTrailing}\n\n$PatchMarker\n$trimmed\n"

  def fallbackTitle(name: String): String =
    name.split("[-_]+").filter(_.nonEmpty).map(p => p.take(1).toUpperCase + p.drop(1)).mkString(" ")

  def extractTitle(name: String, raw: String): String =
    HeadingRe
      .findFirstMatchIn(bodyWithoutFrontmatter(raw))
      .map(_.group(1).trim)
      .filter(_.nonEmpty)
      .getOrElse(fallbackTitle(name))

  private def cleanMarkdown(text: String): String =
    text
      .replaceAll("`([^`]+)`", "$1")
      .replaceAll("""\[([^\]]+)]\([^)]+\)""", "$1")
      .replaceAll("[*_~]", "")
      .replaceAll("""\s+""", " ")
      .trim

  def extractDescription(raw: String, title: String): String =
    val body = bodyWithoutFrontmatter(raw)
    val afterTitle = HeadingRe.findFirstMatchIn(body).fold(body)(m => body.substring(m.end))
    val paragraph = afterTitle
      .split("""\r?\n\s*\r?\n""")
      .map(_.trim)
      .find(block =>
        block.nonEmpty && !block.startsWith("#") && !block.startsWith("```") && !block.startsWith("|") &&
          !block.matches("""(?s)^[-*]\s.*""")
      )
    val description = cleanMarkdown(paragraph.getOrElse(s"Instructions for $title."))
    if description.codePointCount(0, description.length) <= 280 then description
    else truncateChars(description, 277).stripTrailing + "..."

  def extractTools(raw: String): Option[SkillTools] =
    FrontmatterRe.findPrefixMatchOf(raw).map(_.group(1)).flatMap { frontmatter =>
      if """(?m)^tools:\s*\*\s*$""".r.findFirstIn(frontmatter).isDefined then Some(SkillTools.All)
      else
        """(?m)^tools:\s*\[(.*?)\]\s*$""".r
          .findFirstMatchIn(frontmatter)
          .map(m => SkillTools.Only(m.group(1).split(',').map(_.trim).filter(_.nonEmpty).toList))
    }

// ── 2. tools ─────────────────────────────────────────────────────────────────

object SkillsTools:
  import Tools.*

  final case class ReadSkillParams(
      @description("Exact skill fileName returned by list_skills, for example telegram.md.") fileName: String
  ) derives JsonDecoder,
        Schema

  final case class Listed(count: Int, skills: List[SkillSummary]) derives JsonEncoder

  val tools: List[ToolDef] = List(
    tool(
      "list_skills",
      "List available skills",
      "Return the catalog of all available skills. Each entry includes the exact fileName, human-readable title, " +
        "short description, declared tool access, active source layer, size, modification time, and `patched` — " +
        "whether an improver patch is currently appended to that skill's instructions. Use this first to choose a " +
        "skill, then pass its exact fileName to read_skill."
    ) { (deps, _: NoArgs) => deps.skills.listSkills.map(skills => Listed(skills.size, skills)) },
    tool(
      "read_skill",
      "Read complete skill instructions",
      "Return the complete UTF-8 Markdown contents of one active skill file selected from list_skills, including its " +
        "frontmatter. Pass the exact fileName from the catalog, including the .md extension. Three views come back: " +
        "`content` is the editable source file; `patch` is the improver's append-only overlay (null when there is " +
        "none); and `effectiveInstructions` is what the agent actually runs — the body with the frontmatter stripped " +
        "and the patch appended. When `patch` is non-null, judge the skill's behaviour by effectiveInstructions, not " +
        "by content alone."
    ) { (deps, p: ReadSkillParams) =>
      deps.skills.readSkill(p.fileName).map {
        case None => Json.Obj("found" -> Json.Bool(false), "fileName" -> Json.Str(p.fileName), "content" -> Json.Null)
        case Some(skill) =>
          Json.Obj(
            "found" -> Json.Bool(true),
            "fileName" -> Json.Str(p.fileName),
            "content" -> Json.Str(skill.content),
            "patch" -> skill.patch.fold(Json.Null)(Json.Str(_)),
            "effectiveInstructions" -> Json.Str(skill.effectiveInstructions),
            "metadata" -> skill.summary.toJsonAST.getOrElse(Json.Null)
          )
      }
    }
  )
