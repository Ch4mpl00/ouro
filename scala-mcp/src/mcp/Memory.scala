package mcp

// Unified memory: one memory every agent shares — the droplet supervisor,
// Claude Code sessions, ChatGPT through the tunnel. Design and rationale in
// .claude/tasks/unified-memory.md (the D-numbers below refer to it).
//
// Two read models behind one search projection (D8): projects made of
// markdown documents, and flat facts. Both feed `memory_index`, the only
// table with embeddings. The agent's flow is two-step: `recall` returns refs,
// `read_doc` / `get_fact` loads the whole thing.
//
// Sections:
//   1. types      — states, projects, documents, facts, patches, index rows
//   2. refs       — `doc:<project>/<name>#<chunk>` / `fact:<id>`, parsed strictly
//   3. patch      — search/replace edits, near-match hints, append, invert
//   4. projection — markdown chunking, index text, recency-aware ranking
//   5. store      — the dumb-CRUD port + its Postgres implementation
//   6. indexer    — keeps `memory_index` in step with the read models
//   7. service    — every rule that protects a document
//   8. import     — one-shot copy of knowledge_base_notes into facts
//   9. tools      — `memory` toolset
//
// The rules live in the service and the store underneath is dumb, so the
// whole contract is tested against an in-memory store without a Postgres.

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.json.*
import zio.json.ast.Json

import java.sql.ResultSet
import java.time.Instant
import java.util.Locale

import Rows.*

// ── 1. types ─────────────────────────────────────────────────────────────────

// D7 — decay is an explicit state plus a recency boost at ranking time.
// Nothing is ever deleted.
enum MemoryState derives JsonEncoder, JsonDecoder:
  case active, done, archived

object MemoryState:
  given Schema[MemoryState] = Schema.derivedEnumeration[MemoryState].defaultStringBased
  def parse(s: String): MemoryState = values.find(_.toString == s).getOrElse(active)

final case class Project(id: Long, slug: String, title: String, createdAt: Instant, updatedAt: Instant)
    derives JsonEncoder

// Enough to choose a document without loading it; the summary is what keeps a
// project from growing notes.md / notes2.md / progress-new.md (D5).
final case class DocSummary(name: String, summary: Option[String], version: Int, sizeBytes: Int, updatedAt: Instant)
    derives JsonEncoder

final case class Doc(
    id: Long,
    projectId: Long,
    name: String,
    summary: Option[String],
    body: String,
    version: Int,
    sizeBytes: Int,
    updatedAt: Instant
) derives JsonEncoder:
  def summaryView: DocSummary = DocSummary(name, summary, version, sizeBytes, updatedAt)

final case class Fact(
    id: Long,
    body: String,
    tags: List[String],
    source: Option[String],
    state: MemoryState,
    createdAt: Instant,
    updatedAt: Instant
) derives JsonEncoder

enum PatchKind derives JsonEncoder:
  case write, append, patch, revert

object PatchKind:
  def parse(s: String): PatchKind = values.find(_.toString == s).getOrElse(write)

final case class Edit(
    @description("Exact text to find, unique in the document.") old: String,
    @description("Replacement. Empty string deletes.") `new`: String
) derives JsonEncoder,
      JsonDecoder,
      Schema

// One row per write. `bodyBefore` is the whole previous document: it makes
// "roll roadmap.md back to v7" answerable and is what a failed mid-stack
// revert falls back to (D9).
final case class DocPatch(
    // Short and random: sequential ids invite guessing a neighbour's.
    pid: String,
    docId: Long,
    kind: PatchKind,
    edits: List[Edit],
    bodyBefore: String,
    versionBefore: Int,
    versionAfter: Int,
    actor: String,
    rationale: Option[String],
    createdAt: Instant
)

// A row on its way into the search projection. `sourceRef` is the owning
// object and what a re-index deletes by — exact equality, never a prefix, so
// `fact:88` can't wipe `fact:880`.
final case class IndexUpsert(
    sourceRef: String,
    ref: String,
    text: String,
    tags: List[String],
    actor: Option[String],
    state: MemoryState,
    ts: Instant,
    embedding: Option[Vector[Float]]
)

final case class IndexHit(
    id: Long,
    ref: String,
    text: String,
    tags: List[String],
    actor: Option[String],
    state: MemoryState,
    ts: Instant,
    distance: Double
)

// ── 2. refs ──────────────────────────────────────────────────────────────────

enum MemoryRef:
  case DocRef(project: String, doc: String, chunk: Option[Int])
  case FactRef(id: Long)

object Refs:
  // Refs travel through an LLM, so they are parsed strictly: one that doesn't
  // round-trip is a bug to see, not something to guess at.
  private val Slug = "^[a-z0-9][a-z0-9-]*$".r
  // Markdown filenames, no directories: a project is a flat folder, and a
  // separator would make refs ambiguous.
  private val DocName = """^[a-z0-9][a-z0-9._-]*\.md$""".r
  private val DocRefRe = """^doc:([^/]+)/([^#]+)(?:#(\d+))?$""".r

  def slugOk(slug: String): Boolean = Slug.matches(slug)
  def docNameOk(name: String): Boolean = DocName.matches(name)

  def assertProjectSlug(slug: String): IO[ToolFailure, Unit] =
    ZIO
      .unless(slugOk(slug))(
        Tools.fail(
          s"""Invalid project slug "$slug". Use lowercase letters, digits and hyphens, e.g. "leetcode-graphs"."""
        )
      )
      .unit

  def assertDocName(name: String): IO[ToolFailure, Unit] =
    ZIO
      .unless(docNameOk(name))(
        Tools.fail(
          s"""Invalid document name "$name". Use a lowercase markdown filename with no directories, e.g. "roadmap.md"."""
        )
      )
      .unit

  def docRef(project: String, doc: String, chunk: Option[Int]): String =
    chunk.fold(s"doc:$project/$doc")(c => s"doc:$project/$doc#$c")

  def factRef(id: Long): String = s"fact:$id"

  def parse(raw: String): Option[MemoryRef] =
    if raw.startsWith("fact:") then
      val id = raw.stripPrefix("fact:")
      Option.when(id.nonEmpty && id.forall(_.isDigit))(id.toLongOption).flatten.map(MemoryRef.FactRef(_))
    else
      raw match
        case DocRefRe(project, doc, chunk) if slugOk(project) && docNameOk(doc) =>
          Some(MemoryRef.DocRef(project, doc, Option(chunk).flatMap(_.toIntOption)))
        case _ => None

// ── 3. patch ─────────────────────────────────────────────────────────────────

enum EditFailureReason:
  case empty, not_found, ambiguous

object EditFailureReason:
  given JsonEncoder[EditFailureReason] = JsonEncoder.string.contramap(_.toString)

final case class EditFailure(
    // Position in the caller's edits, so the model fixes one edit rather than
    // resending the whole call blind.
    index: Int,
    old: String,
    reason: EditFailureReason,
    occurrences: Int,
    // Literal text that *nearly* matched. Suggested, never applied:
    // fuzzy-applying is how an agent silently edits the wrong sentence.
    suggestions: List[String]
) derives JsonEncoder

final case class Heading(level: Int, text: String, line: Int)

// Search/replace on quoted literals, never line numbers (D10): an off-by-one
// line number corrupts silently, a quote that doesn't match fails loudly and
// changes nothing. Pure — body in, body out.
object Patching:
  def countOccurrences(haystack: String, needle: String): Int =
    Iterator
      .iterate(haystack.indexOf(needle))(at => haystack.indexOf(needle, at + needle.length))
      .takeWhile(_ >= 0)
      .size

  private def replaceFirst(s: String, old: String, replacement: String): String =
    val at = s.indexOf(old)
    s.substring(0, at) + replacement + s.substring(at + old.length)

  // Atomic: all edits apply or none do. Applied in order to a working copy,
  // so a later edit may target text an earlier one produced.
  def applyEdits(body: String, edits: List[Edit]): Either[List[EditFailure], String] =
    if edits.isEmpty then Left(List(EditFailure(0, "", EditFailureReason.empty, 0, Nil)))
    else
      val (working, failures) = edits.zipWithIndex.foldLeft((body, List.empty[EditFailure])) {
        case ((working, failures), (edit, index)) =>
          // An empty `old` matches everywhere; "insert at the start" is what
          // append_doc is for.
          if edit.old.isEmpty then (working, failures :+ EditFailure(index, edit.old, EditFailureReason.empty, 0, Nil))
          else
            countOccurrences(working, edit.old) match
              case 1 => (replaceFirst(working, edit.old, edit.`new`), failures)
              // Never "take the first match": quote more context.
              case 0 =>
                val near = findNearMatches(working, edit.old, 3)
                (working, failures :+ EditFailure(index, edit.old, EditFailureReason.not_found, 0, near))
              case n => (working, failures :+ EditFailure(index, edit.old, EditFailureReason.ambiguous, n, Nil))
      }
      if failures.isEmpty then Right(working) else Left(failures)

  // The inverse of a patch, for reverting one that is no longer the newest. A
  // deletion has no inverse — re-inserting needs a position whose anchor is
  // gone — so callers fall back to a whole-document rollback.
  def invertEdits(edits: List[Edit]): Option[List[Edit]] =
    if edits.exists(_.`new`.isEmpty) then None else Some(edits.reverse.map(e => Edit(e.`new`, e.old)))

  // The usual miss is a *normalised* quote: an em-dash retyped as a hyphen, a
  // «quote» straightened, ё written as е, whitespace reflowed. Normalising
  // both sides and mapping the hit back to the original characters hands the
  // agent the exact literal to retry with. Works on code points, like Rust's
  // chars.
  final private case class Normalised(text: Vector[Int], origin: Vector[Int])

  private def isDash(c: Int) = (c >= 0x2010 && c <= 0x2015) || c == 0x2212
  private val Quotes = "«»“”„‘’′″".codePoints.toArray.toSet
  private def isSpace(c: Int) = Character.isWhitespace(c) || Character.isSpaceChar(c) || c == 0xfeff

  private def normalise(source: Vector[Int]): Normalised =
    val text = Vector.newBuilder[Int]
    val origin = Vector.newBuilder[Int]
    var pendingSpace = false
    var size = 0
    source.zipWithIndex.foreach { (raw, i) =>
      if isSpace(raw) then
        // Collapse runs; never let one start the string.
        pendingSpace = size > 0
      else
        if pendingSpace then
          text += ' '
          origin += i
          size += 1
          pendingSpace = false
        String(Character.toChars(raw)).toLowerCase(Locale.ROOT).codePoints.toArray.foreach { c =>
          text += (if isDash(c) then '-' else if Quotes(c) then '"' else if c == 'ё' then 'е' else c)
          origin += i
          size += 1
        }
    }
    Normalised(text.result(), origin.result())

  private def codePoints(s: String): Vector[Int] = s.codePoints.toArray.toVector
  private def fromCodePoints(cs: Seq[Int]): String = String(cs.toArray, 0, cs.size)

  def findNearMatches(body: String, needle: String, limit: Int): List[String] =
    val bodyCps = codePoints(body)
    val nBody = normalise(bodyCps)
    val nNeedle = normalise(codePoints(needle))
    if nNeedle.text.isEmpty then Nil
    else
      val spans = Iterator
        .unfold(0) { from =>
          Option(nBody.text.indexOfSlice(nNeedle.text, from)).filter(_ >= 0).map { at =>
            val (start, end) = (nBody.origin(at), nBody.origin(at + nNeedle.text.size - 1))
            fromCodePoints(bodyCps.slice(start, end + 1)) -> (at + nNeedle.text.size.max(1))
          }
        }
        .take(limit)
        .toList
      if spans.nonEmpty then spans
      else
        // Nothing matched even loosely: the most similar lines at least tell
        // the agent where to look.
        val target = nNeedle.text.take(200)
        body
          .split("\n", -1)
          .toList
          .filter(_.trim.nonEmpty)
          .map(line => line -> dice(normalise(codePoints(line)).text, target))
          .filter(_._2 >= 0.4)
          .sortBy(-_._2)
          .take(limit)
          .map(_._1)

  // Bigram Dice coefficient: cheap, and enough to rank "which line did they
  // mean" without pretending to be a diff.
  def dice(a: Vector[Int], b: Vector[Int]): Double =
    if a == b then 1.0
    else if a.size < 2 || b.size < 2 then 0.0
    else
      val bigrams = scala.collection.mutable.Map.from(a.sliding(2).toList.groupMapReduce(identity)(_ => 1)(_ + _))
      val hits = b.sliding(2).count { w =>
        val left = bigrams.getOrElse(w, 0)
        if left > 0 then bigrams(w) = left - 1
        left > 0
      }
      (2 * hits).toDouble / (a.size - 1 + b.size - 1)

  private val HeadingRe = """^(#{1,6})\s+(.+?)\s*$""".r

  def listHeadings(body: String): List[Heading] =
    body.split("\n", -1).toList.zipWithIndex.collect { case (HeadingRe(hashes, text), line) =>
      Heading(hashes.length, text, line)
    }

  // The safe default (D10, op 1): it cannot destroy text and needs no read.
  // Headings are the anchors because they survive edits elsewhere. Left = the
  // headings that do exist.
  def appendToBody(body: String, text: String, underHeading: Option[String]): Either[List[String], String] =
    val addition = text.trim
    underHeading.filter(_.trim.nonEmpty) match
      case None          => Right(joinBlocks(body, addition))
      case Some(heading) =>
        val headings = listHeadings(body)
        // "Progress" as readily as "## Progress".
        val wanted = normalise(codePoints(heading.dropWhile(_ == '#').stripLeading)).text
        headings.find(h => normalise(codePoints(h.text)).text == wanted) match
          case None         => Left(headings.map(h => s"${"#" * h.level} ${h.text}"))
          case Some(target) =>
            val lines = body.split("\n", -1).toList
            // The section runs to the next heading of the same or higher
            // rank; a deeper sub-heading is still part of it.
            val end = headings.find(h => h.line > target.line && h.level <= target.level).fold(lines.size)(_.line)
            val (head, tail) = (lines.take(end).mkString("\n"), lines.drop(end).mkString("\n"))
            val merged = joinBlocks(head, addition)
            Right(if tail.isEmpty then merged else s"$merged\n$tail")

  // One blank line between blocks, exactly one trailing newline: stable
  // formatting keeps later quotes matching what the agent last saw.
  def joinBlocks(existing: String, addition: String): String =
    val base = existing.stripTrailing
    (base.isEmpty, addition.isEmpty) match
      case (true, true)   => ""
      case (false, true)  => s"$base\n"
      case (true, false)  => s"$addition\n"
      case (false, false) => s"$base\n\n$addition\n"

// ── 4. projection ────────────────────────────────────────────────────────────

final case class MarkdownChunk(
    text: String,
    // Breadcrumb of enclosing headings ("Progress > Notes"); empty before the
    // first heading.
    headingPath: String
)

final case class RankOpts(
    // Days for the recency bonus to halve.
    halfLifeDays: Double,
    // How much a brand-new row may improve its distance. Small on purpose:
    // recency breaks ties, it must not float an irrelevant note over a
    // relevant one (D7).
    recencyWeight: Double
)

object Projection:
  val DefaultChunkChars = 1200
  val DefaultRank: RankOpts = RankOpts(30.0, 0.05)
  private val HeadingRe = """^(#{1,6})\s+(.+?)\s*$""".r

  private def charLen(s: String) = s.codePointCount(0, s.length)

  // Paragraphs packed up to `maxChars`, split at every heading (the strongest
  // topical boundary markdown has), never mid-paragraph unless one alone is
  // oversized. A month of daily entries must not become one vector (D8).
  def chunkMarkdown(body: String, maxChars: Int): List[MarkdownChunk] =
    final case class State(
        chunks: Vector[MarkdownChunk] = Vector.empty,
        stack: List[(Int, String)] = Nil,
        buffer: Vector[String] = Vector.empty,
        bufferPath: String = ""
    ):
      def path: String = stack.reverse.map(_._2).mkString(" > ")
      def flush: State =
        val text = buffer.mkString("\n\n").trim
        copy(chunks = if text.isEmpty then chunks else chunks :+ MarkdownChunk(text, bufferPath), buffer = Vector.empty)

    val end = body.split("(?U)\n\\s*\n", -1).map(_.trim).filter(_.nonEmpty).foldLeft(State()) { (state, paragraph) =>
      paragraph match
        case HeadingRe(hashes, text) =>
          val flushed = state.flush
          val stack = (hashes.length, text) :: flushed.stack.dropWhile(_._1 >= hashes.length)
          val next = flushed.copy(stack = stack)
          next.copy(bufferPath = next.path)
        case _ =>
          val started = if state.buffer.isEmpty then state.copy(bufferPath = state.path) else state
          val projected = started.buffer.map(charLen).sum + 2 * started.buffer.size + charLen(paragraph)
          val room =
            if started.buffer.nonEmpty && projected > maxChars then
              val f = started.flush
              f.copy(bufferPath = f.path)
            else started
          if charLen(paragraph) > maxChars then
            // A pasted log or a long table still has to fit the embedder.
            val cps = paragraph.codePoints.toArray
            val pieces = cps.grouped(maxChars).map(p => MarkdownChunk(String(p, 0, p.length), room.path))
            room.copy(chunks = room.chunks ++ pieces)
          else room.copy(buffer = room.buffer :+ paragraph)
    }
    end.flush.chunks.toList

  // Chunks must be self-contained; documents must not (D8). "застрял на
  // Dijkstra" matches nothing alone, so the subject goes into the indexed text
  // while the stored document stays clean.
  def buildIndexText(projectTitle: String, docName: String, headingPath: String, text: String): String =
    if headingPath.isEmpty then s"$projectTitle — $docName\n\n$text"
    else s"$projectTitle — $docName · $headingPath\n\n$text"

  def rankHits(hits: List[IndexHit], now: Instant, opts: RankOpts): List[(IndexHit, Double)] =
    hits
      .map { hit =>
        val ageDays = ((now.toEpochMilli - hit.ts.toEpochMilli).toDouble / 86_400_000.0).max(0.0)
        val boost = opts.recencyWeight * math.pow(2, -ageDays / opts.halfLifeDays)
        hit -> (hit.distance - boost)
      }
      .sortBy(_._2)

// ── 5. store ─────────────────────────────────────────────────────────────────

final case class NewPatch(
    pid: String,
    docId: Long,
    kind: PatchKind,
    edits: List[Edit],
    bodyBefore: String,
    versionBefore: Int,
    versionAfter: Int,
    actor: String,
    rationale: Option[String]
)

final case class FactUpdate(body: Option[String], tags: Option[List[String]], state: Option[MemoryState])

// Dumb CRUD: no rules here, only the persistence the service's rules are
// expressed in. The one piece of real logic is the compare-and-swap.
trait MemoryStore:
  def createProject(slug: String, title: String): Task[Project]
  def getProject(slug: String): Task[Option[Project]]
  def listProjects: Task[List[Project]]

  def listDocs(projectId: Long): Task[List[DocSummary]]
  def getDoc(projectId: Long, name: String): Task[Option[Doc]]
  def createDoc(projectId: Long, name: String, summary: Option[String], body: String): Task[Doc]
  // None when `expectedVersion` no longer matches — the whole concurrency
  // story for several agents on one document (D1). `summary = None` keeps it.
  def updateDoc(docId: Long, expectedVersion: Int, body: String, summary: Option[Option[String]]): Task[Option[Doc]]

  def insertPatch(patch: NewPatch): Task[DocPatch]
  // Newest first.
  def listPatches(docId: Long, limit: Int): Task[List[DocPatch]]
  def getPatch(docId: Long, pid: String): Task[Option[DocPatch]]

  def createFact(body: String, tags: List[String], source: Option[String]): Task[Fact]
  def getFact(id: Long): Task[Option[Fact]]
  // By provenance — what makes the knowledge_base_notes import re-runnable.
  def getFactBySource(source: String): Task[Option[Fact]]
  def updateFact(id: Long, update: FactUpdate): Task[Option[Fact]]

  // Replaces every row owned by `sourceRef`, so a shrunken document leaves no
  // orphaned chunks answering recalls.
  def replaceIndex(sourceRef: String, entries: List[IndexUpsert]): Task[Unit]
  def searchIndex(
      embedding: Vector[Float],
      limit: Int,
      states: List[MemoryState],
      tags: List[String]
  ): Task[List[IndexHit]]
  def listUnembedded(limit: Int): Task[List[(Long, String)]]
  def setEmbedding(id: Long, embedding: Vector[Float]): Task[Unit]

final class PgMemoryStore(pool: PgPool) extends MemoryStore:
  import PgMemoryStore.*

  def createProject(slug: String, title: String): Task[Project] =
    pool.query("INSERT INTO memory_projects (slug, title) VALUES (?, ?) RETURNING *", slug, title)(project).map(_.head)

  def getProject(slug: String): Task[Option[Project]] =
    pool.query("SELECT * FROM memory_projects WHERE slug = ? LIMIT 1", slug)(project).map(_.headOption)

  def listProjects: Task[List[Project]] = pool.query("SELECT * FROM memory_projects ORDER BY slug")(project)

  def listDocs(projectId: Long): Task[List[DocSummary]] =
    pool.query("SELECT * FROM memory_project_docs WHERE project_id = ? ORDER BY name", projectId)(doc(_).summaryView)

  def getDoc(projectId: Long, name: String): Task[Option[Doc]] =
    pool
      .query("SELECT * FROM memory_project_docs WHERE project_id = ? AND name = ? LIMIT 1", projectId, name)(doc)
      .map(_.headOption)

  def createDoc(projectId: Long, name: String, summary: Option[String], body: String): Task[Doc] =
    pool
      .query(
        "INSERT INTO memory_project_docs (project_id, name, summary, body_md) VALUES (?, ?, ?, ?) RETURNING *",
        projectId,
        name,
        summary,
        body
      )(doc)
      .map(_.head)

  // The version in the WHERE clause matches no row once another agent has
  // moved on; the caller reports a conflict instead of overwriting.
  def updateDoc(docId: Long, expectedVersion: Int, body: String, summary: Option[Option[String]]): Task[Option[Doc]] =
    val rows = summary match
      case None =>
        pool.query(
          """UPDATE memory_project_docs SET body_md = ?, version = ?, updated_at = now()
              WHERE id = ? AND version = ? RETURNING *""",
          body,
          expectedVersion + 1,
          docId,
          expectedVersion
        )(doc)
      case Some(s) =>
        pool.query(
          """UPDATE memory_project_docs SET body_md = ?, summary = ?, version = ?, updated_at = now()
              WHERE id = ? AND version = ? RETURNING *""",
          body,
          s,
          expectedVersion + 1,
          docId,
          expectedVersion
        )(doc)
    rows.map(_.headOption)

  def insertPatch(p: NewPatch): Task[DocPatch] =
    pool
      .query(
        """INSERT INTO memory_doc_patches (doc_id, pid, kind, edits, body_before, version_before, version_after, actor, rationale)
           VALUES (?, ?, ?, ?::jsonb, ?, ?, ?, ?, ?) RETURNING *""",
        p.docId,
        p.pid,
        p.kind.toString,
        p.edits.toJson,
        p.bodyBefore,
        p.versionBefore,
        p.versionAfter,
        p.actor,
        p.rationale
      )(patch)
      .map(_.head)

  def listPatches(docId: Long, limit: Int): Task[List[DocPatch]] =
    pool.query(
      "SELECT * FROM memory_doc_patches WHERE doc_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
      docId,
      limit
    )(patch)

  def getPatch(docId: Long, pid: String): Task[Option[DocPatch]] =
    pool
      .query("SELECT * FROM memory_doc_patches WHERE doc_id = ? AND pid = ? LIMIT 1", docId, pid)(patch)
      .map(_.headOption)

  def createFact(body: String, tags: List[String], source: Option[String]): Task[Fact] =
    pool
      .query("INSERT INTO memory_facts (body, tags, source) VALUES (?, ?, ?) RETURNING *", body, tags, source)(fact)
      .map(_.head)

  def getFact(id: Long): Task[Option[Fact]] =
    pool.query("SELECT * FROM memory_facts WHERE id = ? LIMIT 1", id)(fact).map(_.headOption)

  def getFactBySource(source: String): Task[Option[Fact]] =
    pool.query("SELECT * FROM memory_facts WHERE source = ? LIMIT 1", source)(fact).map(_.headOption)

  def updateFact(id: Long, update: FactUpdate): Task[Option[Fact]] =
    pool
      .query(
        """UPDATE memory_facts SET body = COALESCE(?, body), tags = COALESCE(?::text[], tags),
                  state = COALESCE(?, state), updated_at = now()
            WHERE id = ? RETURNING *""",
        update.body,
        update.tags,
        update.state.map(_.toString),
        id
      )(fact)
      .map(_.headOption)

  // Delete-then-insert in one transaction rather than upsert: a re-indexed
  // document can have fewer chunks than before.
  def replaceIndex(sourceRef: String, entries: List[IndexUpsert]): Task[Unit] =
    pool.transaction { c =>
      Sql.update(c, "DELETE FROM memory_index WHERE source_ref = ?", sourceRef)
      entries.foreach { e =>
        val embedding = e.embedding.map(Vectors.literal)
        Sql.update(
          c,
          """INSERT INTO memory_index (source_ref, ref, text, tags, actor, state, ts, embedding, embedded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?::text::vector, CASE WHEN ?::text IS NULL THEN NULL ELSE now() END)""",
          e.sourceRef,
          e.ref,
          e.text,
          e.tags,
          e.actor,
          e.state.toString,
          e.ts,
          embedding,
          embedding
        )
      }
    }

  def searchIndex(
      embedding: Vector[Float],
      limit: Int,
      states: List[MemoryState],
      tags: List[String]
  ): Task[List[IndexHit]] =
    pool.query(
      """SELECT id, ref, text, tags, actor, state, ts, (embedding <=> ?::text::vector) AS distance
           FROM memory_index
          WHERE embedding IS NOT NULL AND state = ANY(?) AND (cardinality(?::text[]) = 0 OR tags && ?::text[])
          ORDER BY distance LIMIT ?""",
      Vectors.literal(embedding),
      states.map(_.toString),
      tags.filter(_.nonEmpty),
      tags.filter(_.nonEmpty),
      limit
    )(rs =>
      IndexHit(
        rs.getLong("id"),
        rs.getString("ref"),
        rs.getString("text"),
        KnowledgeRepository.textArray(rs, "tags"),
        rs.optString("actor"),
        MemoryState.parse(rs.getString("state")),
        rs.instant("ts"),
        rs.getDouble("distance")
      )
    )

  def listUnembedded(limit: Int): Task[List[(Long, String)]] =
    pool.query("SELECT id, text FROM memory_index WHERE embedding IS NULL LIMIT ?", limit)(rs =>
      rs.getLong("id") -> rs.getString("text")
    )

  def setEmbedding(id: Long, embedding: Vector[Float]): Task[Unit] =
    pool
      .update(
        "UPDATE memory_index SET embedding = ?::text::vector, embedded_at = now() WHERE id = ?",
        Vectors.literal(embedding),
        id
      )
      .unit

object PgMemoryStore:
  private def project(rs: ResultSet) =
    Project(
      rs.getLong("id"),
      rs.getString("slug"),
      rs.getString("title"),
      rs.instant("created_at"),
      rs.instant("updated_at")
    )

  private def doc(rs: ResultSet) =
    val body = rs.getString("body_md")
    Doc(
      rs.getLong("id"),
      rs.getLong("project_id"),
      rs.getString("name"),
      rs.optString("summary"),
      body,
      rs.getInt("version"),
      body.getBytes(java.nio.charset.StandardCharsets.UTF_8).length,
      rs.instant("updated_at")
    )

  private def patch(rs: ResultSet) = DocPatch(
    rs.getString("pid"),
    rs.getLong("doc_id"),
    PatchKind.parse(rs.getString("kind")),
    rs.getString("edits").fromJson[List[Edit]].getOrElse(Nil),
    rs.getString("body_before"),
    rs.getInt("version_before"),
    rs.getInt("version_after"),
    rs.getString("actor"),
    rs.optString("rationale"),
    rs.instant("created_at")
  )

  private def fact(rs: ResultSet) = Fact(
    rs.getLong("id"),
    rs.getString("body"),
    KnowledgeRepository.textArray(rs, "tags"),
    rs.optString("source"),
    MemoryState.parse(rs.getString("state")),
    rs.instant("created_at"),
    rs.instant("updated_at")
  )

// ── 6. indexer ───────────────────────────────────────────────────────────────

// Chunk, denormalise, embed — inline after each write. With the provider
// down a row still lands with a NULL vector: reads and patches never depend
// on OpenAI, only searchability may lag (D4).
final class Indexer(store: MemoryStore, embedder: Embedder, chunkChars: Int = Projection.DefaultChunkChars):
  private def embedTexts(texts: List[String]): UIO[List[Option[Vector[Float]]]] =
    if texts.isEmpty then ZIO.succeed(Nil)
    else
      embedder.embedBatch(texts).either.flatMap {
        case Right(vectors) => ZIO.succeed(texts.indices.toList.map(vectors.lift))
        case Left(err)      =>
          ZIO
            .logError(s"memory index embed failed for ${texts.size} chunks: ${Results.describe(err)}")
            .as(texts.map(_ => None))
      }

  def indexDoc(project: Project, doc: Doc): Task[Unit] =
    val chunks = Projection.chunkMarkdown(doc.body, chunkChars)
    val texts = chunks.map(c => Projection.buildIndexText(project.title, doc.name, c.headingPath, c.text))
    val owner = Refs.docRef(project.slug, doc.name, None)
    embedTexts(texts).flatMap { vectors =>
      val entries = texts.zip(vectors).zipWithIndex.map { case ((text, embedding), i) =>
        // Documents have no lifecycle of their own.
        IndexUpsert(
          owner,
          Refs.docRef(project.slug, doc.name, Some(i)),
          text,
          Nil,
          None,
          MemoryState.active,
          doc.updatedAt,
          embedding
        )
      }
      // Always replace, even with nothing: an emptied document must stop
      // answering from its old chunks.
      store.replaceIndex(owner, entries)
    }

  def indexFact(fact: Fact): Task[Unit] =
    val owner = Refs.factRef(fact.id)
    embedTexts(List(fact.body)).flatMap { vectors =>
      // Archiving drops a fact from default recall, deleting nothing.
      store.replaceIndex(
        owner,
        List(IndexUpsert(owner, owner, fact.body, fact.tags, fact.source, fact.state, fact.updatedAt, vectors.head))
      )
    }

  // Rows a failed inline embed left NULL. 0/0 when drained.
  def embedMissingBatch(batch: Int): Task[EmbedResult] =
    store.listUnembedded(batch).flatMap { rows =>
      if rows.isEmpty then ZIO.succeed(EmbedResult())
      else
        embedTexts(rows.map(_._2)).flatMap { vectors =>
          ZIO
            .foreach(rows.zip(vectors)) {
              case ((id, _), Some(v)) => store.setEmbedding(id, v).as(1)
              case (_, None)          => ZIO.succeed(0)
            }
            .map(done => EmbedResult(done.sum, rows.size - done.sum))
        }
    }

  // None when the provider is unreachable — recall reports "search is down"
  // rather than an empty memory.
  def embedQuery(query: String): UIO[Option[Vector[Float]]] = embedTexts(List(query)).map(_.head)

// ── 7. service ───────────────────────────────────────────────────────────────

// Expected failures are values, not stack traces: an agent that gets
// `{ error: "version_conflict", currentVersion: 9 }` knows what to do next.
final case class MemoryError(code: String, message: String, details: Json.Obj = Json.Obj()) extends Exception(message)

final case class WriteResult(project: String, doc: String, version: Int, patchId: String, sizeBytes: Int)
    derives JsonEncoder

final case class HistoryEntry(
    patchId: String,
    kind: PatchKind,
    actor: String,
    rationale: Option[String],
    versionBefore: Int,
    versionAfter: Int,
    editCount: Int,
    createdAt: Instant
) derives JsonEncoder

final case class RecallHit(
    ref: String,
    text: String,
    score: Double,
    distance: Double,
    state: MemoryState,
    actor: Option[String],
    ts: Instant
) derives JsonEncoder

final case class DocList(project: Project, docs: List[DocSummary]) derives JsonEncoder

final case class WriteDoc(
    project: String,
    doc: String,
    body: String,
    // None keeps the stored summary.
    summary: Option[String] = None,
    // Required once the document exists; absent or 0 when creating it.
    expectedVersion: Option[Int] = None,
    actor: String,
    rationale: Option[String] = None
)

final class MemoryService(val store: MemoryStore, val indexer: Indexer, newPatchId: UIO[String]):
  import MemoryService.*

  private def requireProject(slug: String): Task[Project] =
    Refs.assertProjectSlug(slug) *> store.getProject(slug).flatMap {
      case Some(p) => ZIO.succeed(p)
      case None    =>
        store.listProjects.flatMap(ps =>
          ZIO.fail(
            MemoryError("project_not_found", s"""No project "$slug".""", Json.Obj("projects" -> strs(ps.map(_.slug))))
          )
        )
    }

  private def requireDoc(project: Project, name: String): Task[Doc] =
    Refs.assertDocName(name) *> store.getDoc(project.id, name).flatMap {
      case Some(d) => ZIO.succeed(d)
      case None    =>
        // Listing what does exist is the cheapest defence against notes.md /
        // notes2.md multiplying (D5).
        store.listDocs(project.id).flatMap { docs =>
          ZIO.fail(
            MemoryError(
              "doc_not_found",
              s"""No document "$name" in project "${project.slug}".""",
              Json.Obj("project" -> Json.Str(project.slug), "docs" -> strs(docs.map(_.name)))
            )
          )
        }
    }

  // The document is the truth; searchability may lag (D4).
  private def reindexDoc(project: Project, doc: Doc): UIO[Unit] =
    indexer
      .indexDoc(project, doc)
      .catchAll(err =>
        ZIO.logError(s"memory indexing failed for ${project.slug}/${doc.name}: ${Results.describe(err)}")
      )

  private[mcp] def indexFactQuietly(fact: Fact): UIO[Unit] =
    indexer
      .indexFact(fact)
      .catchAll(err => ZIO.logError(s"memory fact ${fact.id} indexing failed: ${Results.describe(err)}"))

  // The shared tail of every mutating op: swap the body under a version
  // check, record the patch, re-index. None when the CAS lost.
  private def commit(project: Project, doc: Doc, change: Change): Task[Option[WriteResult]] =
    store.updateDoc(doc.id, doc.version, change.body, change.summary).flatMap {
      case None          => ZIO.none
      case Some(updated) =>
        for
          pid <- newPatchId
          patch <- store.insertPatch(
            NewPatch(
              pid,
              doc.id,
              change.kind,
              change.edits,
              doc.body,
              doc.version,
              updated.version,
              change.actor,
              change.rationale
            )
          )
          _ <- reindexDoc(project, updated)
        yield Some(writeResult(project, updated, patch.pid))
    }

  def createProject(slug: String, title: String): Task[Project] =
    Refs.assertProjectSlug(slug) *> store.getProject(slug).flatMap {
      case Some(existing) =>
        ZIO.fail(
          MemoryError(
            "project_exists",
            s"""Project "$slug" already exists.""",
            Json.Obj("project" -> Json.Str(existing.slug), "title" -> Json.Str(existing.title))
          )
        )
      case None => store.createProject(slug, if title.trim.isEmpty then slug else title.trim)
    }

  def listProjects: Task[List[Project]] = store.listProjects

  def listDocs(slug: String): Task[DocList] =
    requireProject(slug).flatMap(p => store.listDocs(p.id).map(DocList(p, _)))

  def readDoc(slug: String, name: String): Task[Doc] = requireProject(slug).flatMap(requireDoc(_, name))

  def writeDoc(input: WriteDoc): Task[WriteResult] =
    for
      project <- requireProject(input.project)
      _ <- Refs.assertDocName(input.doc)
      existing <- store.getDoc(project.id, input.doc)
      result <- existing match
        case None      => create(project, input)
        case Some(doc) =>
          // write_doc is the only op that can lose content: never blind.
          input.expectedVersion match
            case None =>
              ZIO.fail(
                MemoryError(
                  "version_required",
                  s"""Document "${input.doc}" exists at version ${doc.version}; read it and pass expected_version.""",
                  Json.Obj(
                    "project" -> Json.Str(project.slug),
                    "doc" -> Json.Str(input.doc),
                    "currentVersion" -> Json.Num(doc.version)
                  )
                )
              )
            case Some(expected) if expected != doc.version =>
              ZIO.fail(versionConflict(project.slug, input.doc, doc.version, expected))
            case Some(_) =>
              commit(
                project,
                doc,
                Change(input.body, input.summary.map(Some(_)), PatchKind.write, Nil, input.actor, input.rationale)
              )
                .someOrFail(raced(project.slug, input.doc))
    yield result

  private def create(project: Project, input: WriteDoc): Task[WriteResult] =
    if input.expectedVersion.exists(_ != 0) then
      ZIO.fail(
        MemoryError(
          "version_conflict",
          s"""Document "${input.doc}" does not exist yet; expected_version must be omitted or 0.""",
          Json.Obj("project" -> Json.Str(project.slug), "doc" -> Json.Str(input.doc), "currentVersion" -> Json.Num(0))
        )
      )
    else
      for
        created <- store.createDoc(project.id, input.doc, input.summary, input.body)
        pid <- newPatchId
        patch <- store.insertPatch(
          NewPatch(pid, created.id, PatchKind.write, Nil, "", 0, created.version, input.actor, input.rationale)
        )
        _ <- reindexDoc(project, created)
      yield writeResult(project, created, patch.pid)

  // append_doc takes no version — it is the version-free safe op — so it
  // absorbs a race by re-reading instead of pushing it onto the caller.
  def appendDoc(
      slug: String,
      name: String,
      text: String,
      underHeading: Option[String],
      actor: String,
      rationale: Option[String]
  ): Task[WriteResult] =
    requireProject(slug).flatMap { project =>
      val attempt = for
        doc <- requireDoc(project, name)
        body <- ZIO.fromEither(Patching.appendToBody(doc.body, text, underHeading)).mapError { headings =>
          MemoryError(
            "heading_not_found",
            s"""No heading "${underHeading.getOrElse("")}" in "$name".""",
            Json.Obj("project" -> Json.Str(project.slug), "doc" -> Json.Str(name), "headings" -> strs(headings))
          )
        }
        result <- commit(project, doc, Change(body, None, PatchKind.append, Nil, actor, rationale))
      yield result
      def loop(left: Int): Task[WriteResult] =
        if left == 0 then ZIO.fail(raced(project.slug, name))
        else attempt.flatMap(_.fold(loop(left - 1))(ZIO.succeed(_)))
      loop(AppendCasAttempts)
    }

  def patchDoc(
      slug: String,
      name: String,
      expectedVersion: Int,
      edits: List[Edit],
      actor: String,
      rationale: Option[String]
  ): Task[WriteResult] =
    for
      project <- requireProject(slug)
      doc <- requireDoc(project, name)
      _ <- ZIO.when(doc.version != expectedVersion)(
        ZIO.fail(versionConflict(project.slug, name, doc.version, expectedVersion))
      )
      body <- ZIO.fromEither(Patching.applyEdits(doc.body, edits)).mapError(editFailure(project.slug, name, _))
      result <- commit(project, doc, Change(body, None, PatchKind.patch, edits, actor, rationale))
        .someOrFail(raced(project.slug, name))
    yield result

  def history(slug: String, name: String, limit: Int): Task[List[HistoryEntry]] =
    for
      project <- requireProject(slug)
      doc <- requireDoc(project, name)
      patches <- store.listPatches(doc.id, limit)
    yield patches.map(p =>
      HistoryEntry(p.pid, p.kind, p.actor, p.rationale, p.versionBefore, p.versionAfter, p.edits.size, p.createdAt)
    )

  def revert(slug: String, name: String, patchId: String, rollback: Boolean, actor: String): Task[WriteResult] =
    for
      project <- requireProject(slug)
      doc <- requireDoc(project, name)
      patch <- store.getPatch(doc.id, patchId).someOrElseZIO {
        // The model will hallucinate ids: a hard error listing real ones,
        // never a fuzzy match (D9).
        store.listPatches(doc.id, 10).flatMap { known =>
          ZIO.fail(
            MemoryError(
              "patch_not_found",
              s"""No patch "$patchId" on "$name".""",
              Json.Obj(
                "project" -> Json.Str(project.slug),
                "doc" -> Json.Str(name),
                "knownPatchIds" -> strs(known.map(_.pid))
              )
            )
          )
        }
      }
      isNewest <- store.listPatches(doc.id, 1).map(_.headOption.exists(_.pid == patch.pid))
      body <-
        if rollback || isNewest then
          // Exact by construction: every patch stored the body it replaced.
          ZIO.succeed(patch.bodyBefore)
        else
          Option.when(patch.edits.nonEmpty)(patch.edits).flatMap(Patching.invertEdits) match
            case None          => ZIO.fail(revertConflict(project.slug, name, patch, Nil))
            case Some(inverse) =>
              ZIO
                .fromEither(Patching.applyEdits(doc.body, inverse))
                .mapError(revertConflict(project.slug, name, patch, _))
      rationale =
        if rollback && !isNewest then s"rollback to v${patch.versionBefore} (discards patches after ${patch.pid})"
        else s"revert ${patch.pid}"
      result <- commit(project, doc, Change(body, None, PatchKind.revert, Nil, actor, Some(rationale)))
        .someOrFail(raced(project.slug, name))
    yield result

  def remember(body: String, tags: Option[List[String]], source: Option[String]): Task[Fact] =
    val trimmed = body.trim
    if trimmed.isEmpty then ZIO.fail(MemoryError("empty_body", "A fact needs a body."))
    else store.createFact(trimmed, KnowledgeRepository.normalizeTags(tags), source).tap(indexFactQuietly)

  def getFact(id: Long): Task[Fact] =
    store.getFact(id).someOrFail(MemoryError("fact_not_found", s"No fact $id.", Json.Obj("id" -> Json.Num(id))))

  def updateFact(id: Long, body: Option[String], tags: Option[List[String]], state: Option[MemoryState]): Task[Fact] =
    store
      .updateFact(id, FactUpdate(body.map(_.trim), tags.map(t => KnowledgeRepository.normalizeTags(Some(t))), state))
      .someOrFail(MemoryError("fact_not_found", s"No fact $id.", Json.Obj("id" -> Json.Num(id))))
      .tap(indexFactQuietly)

  def recall(
      query: String,
      limit: Option[Int],
      states: Option[List[MemoryState]],
      tags: Option[List[String]],
      now: Instant
  ): Task[List[RecallHit]] =
    val n = limit.getOrElse(10).max(1).min(50)
    indexer.embedQuery(query).flatMap {
      case None =>
        // "Search is down" and "we remember nothing" must not look alike: one
        // is worth retrying.
        ZIO.fail(
          MemoryError(
            "search_unavailable",
            "Recall needs the embedding provider, which is currently unreachable. Documents still read and patch."
          )
        )
      case Some(embedding) =>
        // Over-fetch so the recency boost has something to reorder.
        store
          .searchIndex(embedding, n * 3, states.getOrElse(List(MemoryState.active)), tags.getOrElse(Nil))
          .map(hits =>
            Projection
              .rankHits(hits, now, Projection.DefaultRank)
              .take(n)
              .map((hit, score) => RecallHit(hit.ref, hit.text, score, hit.distance, hit.state, hit.actor, hit.ts))
          )
    }

object MemoryService:
  val AppendCasAttempts = 3

  // One write, as `commit` records it.
  final private case class Change(
      body: String,
      // None keeps the stored summary.
      summary: Option[Option[String]],
      kind: PatchKind,
      edits: List[Edit],
      actor: String,
      rationale: Option[String]
  )

  def randomPatchId: UIO[String] = Random.nextIntBounded(0x1000000).map(n => f"pa:$n%06x")

  def make(store: MemoryStore, embedder: Embedder): MemoryService =
    MemoryService(store, Indexer(store, embedder), randomPatchId)

  private def strs(xs: List[String]): Json = Json.Arr(xs.map(Json.Str(_))*)

  private def writeResult(project: Project, doc: Doc, patchId: String) =
    WriteResult(project.slug, doc.name, doc.version, patchId, doc.sizeBytes)

  def versionConflict(project: String, doc: String, current: Int, expected: Int): MemoryError =
    MemoryError(
      "version_conflict",
      s"""Document "$doc" is at version $current, not $expected. Re-read it and retry.""",
      Json.Obj("project" -> Json.Str(project), "doc" -> Json.Str(doc), "currentVersion" -> Json.Num(current))
    )

  private def raced(project: String, doc: String) =
    MemoryError(
      "version_conflict",
      s"""Document "$doc" changed while this write was in flight. Re-read it and retry.""",
      Json.Obj("project" -> Json.Str(project), "doc" -> Json.Str(doc))
    )

  private def summariseFailures(failures: List[EditFailure]): String =
    failures
      .map { f =>
        f.reason match
          case EditFailureReason.empty     => s"edit ${f.index}: `old` is empty; use append_doc to add text"
          case EditFailureReason.ambiguous =>
            s"edit ${f.index}: `old` matches ${f.occurrences} times; quote more context"
          case EditFailureReason.not_found =>
            val hint = f.suggestions.headOption.fold("")(s => s"; did you mean: ${Json.Str(s).toJson}")
            s"edit ${f.index}: `old` not found$hint"
      }
      .mkString("; ")

  // Nothing was written: the document is exactly as it was.
  private def editFailure(project: String, doc: String, failures: List[EditFailure]) =
    MemoryError(
      "edit_failed",
      summariseFailures(failures),
      Json.Obj(
        "project" -> Json.Str(project),
        "doc" -> Json.Str(doc),
        "applied" -> Json.Bool(false),
        "failures" -> failures.toJsonAST.getOrElse(Json.Arr())
      )
    )

  private def revertConflict(project: String, doc: String, patch: DocPatch, failures: List[EditFailure]) =
    MemoryError(
      "revert_conflict",
      s"Patch ${patch.pid} cannot be undone in place because later patches touched the same text. Retry with " +
        s"rollback=true to restore version ${patch.versionBefore}, discarding everything written after it.",
      Json.Obj(
        "project" -> Json.Str(project),
        "doc" -> Json.Str(doc),
        "patchId" -> Json.Str(patch.pid),
        "rollbackToVersion" -> Json.Num(patch.versionBefore),
        "failures" -> failures.toJsonAST.getOrElse(Json.Arr())
      )
    )

  // ── 8. import ──────────────────────────────────────────────────────────────

  final case class LegacyNote(id: Long, body: String, tags: List[String])

  def legacyNoteSource(noteId: Long): String = s"knowledge_base_notes:$noteId"

  // A copy, not a transformation: the original phrasing is what a later
  // structuring pass needs. Provenance in `source` makes it re-runnable.
  // (imported, skipped)
  def importLegacyNotes(notes: List[LegacyNote], service: MemoryService): Task[(Int, Int)] =
    ZIO
      .foreach(notes) { note =>
        val provenance = legacyNoteSource(note.id)
        val body = note.body.trim
        service.store.getFactBySource(provenance).flatMap {
          case Some(_)              => ZIO.succeed(false)
          case None if body.isEmpty => ZIO.succeed(false)
          case None                 =>
            service.store.createFact(body, note.tags, Some(provenance)).tap(service.indexFactQuietly).as(true)
        }
      }
      .map(done => (done.count(identity), done.count(!_)))

// ── 9. tools ─────────────────────────────────────────────────────────────────

object MemoryTools:
  import Tools.*

  // A MemoryError becomes `{ ok: false, error, message, …details }` — a result
  // the model can act on. Anything else (a malformed slug, a DB outage) stays
  // a tool failure. Payloads are spread flat into `{ ok: true, … }`.
  def envelope[A: JsonEncoder](task: Task[A]): Task[Json] =
    task.foldZIO(
      {
        case e: MemoryError =>
          ZIO.succeed(
            Json.Obj(
              (List(
                "ok" -> Json.Bool(false),
                "error" -> Json.Str(e.code),
                "message" -> Json.Str(e.message)
              ) ++ e.details.fields)*
            )
          )
        case other => ZIO.fail(other)
      },
      value =>
        ZIO.fromEither(value.toJsonAST).mapError(RuntimeException(_)).map {
          case Json.Obj(fields) => Json.Obj((("ok" -> Json.Bool(true)) +: fields)*)
          case other            => Json.Obj("ok" -> Json.Bool(true), "value" -> other)
        }
    )

  private def tagsOk(tags: Option[List[String]], max: Int) = tags.forall(t => t.size <= max && t.forall(_.nonEmpty))

  final case class RecallParams(
      @description("What to look for, in natural language.") query: String,
      @description("Max hits (default 10).") @validate(Validator.inRange(1, 50)) limit: Option[Int],
      @description("Keep only hits carrying one of these tags.") tags: Option[List[String]]
  ) derives JsonDecoder,
        Schema

  final case class RememberParams(
      @description("The fact, as a self-contained sentence including its subject.") body: String,
      @description("3–6 short lowercase topical tags.") tags: Option[List[String]],
      @description("Optional provenance, e.g. \"telegram\".") source: Option[String]
  ) derives JsonDecoder,
        Schema

  final case class GetFactParams(
      @description("Fact id, the number in a fact:<id> ref.") @validate(Validator.min(1L)) id: Long
  ) derives JsonDecoder,
        Schema

  final case class UpdateFactParams(
      @validate(Validator.min(1L)) id: Long,
      @description("Replacement text.") body: Option[String],
      @description("Replacement tags (not merged).") tags: Option[List[String]],
      @description("active = current, done = finished, archived = out of the way.") state: Option[MemoryState]
  ) derives JsonDecoder,
        Schema

  final case class ListMemoryParams(@description("Project slug. Omit to list all projects.") project: Option[String])
      derives JsonDecoder,
        Schema

  final case class CreateProjectParams(
      @description("Lowercase id, hyphens only, e.g. \"leetcode-graphs\".") slug: String,
      @description("Human-readable name.") title: String
  ) derives JsonDecoder,
        Schema

  final case class ReadDocParams(project: String, @description("Document filename, e.g. \"roadmap.md\".") doc: String)
      derives JsonDecoder,
        Schema

  final case class AppendDocParams(
      project: String,
      doc: String,
      @description("Markdown to append. One blank line is inserted before it.") text: String,
      @description("Append at the end of this section instead of the file, e.g. \"Прогресс\".") under_heading: Option[
        String
      ],
      @description("The user's own words that prompted this write.") rationale: Option[String]
  ) derives JsonDecoder,
        Schema

  final case class PatchDocParams(
      project: String,
      doc: String,
      @description("Version from the read_doc you just did.") @validate(Validator.min(1)) expected_version: Int,
      edits: List[Edit],
      @description("The user's own words that prompted this change.") rationale: Option[String]
  ) derives JsonDecoder,
        Schema

  final case class WriteDocParams(
      project: String,
      @description("Filename, e.g. \"roadmap.md\".") doc: String,
      @description("Full markdown content.") body: String,
      @description("One line describing what this document is for.") summary: Option[String],
      @description("Required when the document already exists. Omit when creating.") @validate(Validator.min(0))
      expected_version: Option[Int],
      rationale: Option[String]
  ) derives JsonDecoder,
        Schema

  final case class DocHistoryParams(
      project: String,
      doc: String,
      @description("Default 20.") @validate(Validator.inRange(1, 100)) limit: Option[Int]
  ) derives JsonDecoder,
        Schema

  final case class RevertParams(
      project: String,
      doc: String,
      @description("Patch id from doc_history, e.g. pa:4f2a1c.") patch_id: String,
      @description(
        "Discard everything after this patch and restore the document as it was before it."
      ) rollback: Option[Boolean]
  ) derives JsonDecoder,
        Schema

  final case class Hits(hits: List[RecallHit]) derives JsonEncoder
  final case class OneFact(fact: Fact) derives JsonEncoder
  final case class Projects(projects: List[Project]) derives JsonEncoder
  final case class OneProject(project: Project) derives JsonEncoder
  final case class OneDoc(doc: Doc) derives JsonEncoder
  final case class History(history: List[HistoryEntry]) derives JsonEncoder

  val tools: List[ToolDef] = List(
    // Public recall is deliberately active-only: letting the model pick
    // lifecycle states made ordinary queries opt into archived facts and
    // defeated archive as a discovery boundary.
    tool(
      "recall",
      "Search everything the agents remember",
      "Semantic search across ALL shared memory — project documents and standalone facts alike. Use it whenever the " +
        "user refers to something from the past (\"что там у нас было по X\", \"напомни про Y\") and before starting " +
        "work that might already have a project. Archived facts are intentionally excluded from this ordinary recall " +
        "path.\n\nReturns REFS, not full content: `doc:<project>/<file>#<chunk>` or `fact:<id>`. Follow the " +
        "interesting ones with read_doc / get_fact — the snippet is for choosing, the document is for answering."
    ) { (deps, p: RecallParams) =>
      ZIO.when(p.query.isEmpty || p.limit.exists(l => l < 1 || l > 50) || !tagsOk(p.tags, Int.MaxValue))(
        invalid("query must be non-empty, limit 1–50, tags non-empty")
      ) *> Clock.instant.flatMap(now => envelope(deps.memory.recall(p.query, p.limit, None, p.tags, now).map(Hits(_))))
    },
    tool(
      "remember",
      "Store a standalone fact",
      "Persist a freeform fact the user asked you to remember (\"запомни, что …\"). For anything with ongoing progress " +
        "use a project document instead — a fact is a single self-contained statement, not a running log.\n\nYOU " +
        "generate the tags: 3–6 short lowercase topical words. Recall runs over the TEXT, so write a body that names " +
        "its own subject (\"Лёша платит за интернет 1-го числа\", not \"платит 1-го\")."
    ) { (deps, p: RememberParams) =>
      ZIO.when(p.body.isEmpty || !tagsOk(p.tags, 12))(invalid("body must be non-empty; at most 12 non-empty tags")) *>
        envelope(deps.memory.remember(p.body, p.tags, p.source).map(OneFact(_)))
    },
    tool(
      "get_fact",
      "Read one fact in full",
      "Load a fact by id — the read half of a `fact:<id>` ref returned by recall."
    ) { (deps, p: GetFactParams) =>
      ZIO.when(p.id < 1)(invalid("id must be a positive integer")) *> envelope(
        deps.memory.getFact(p.id).map(OneFact(_))
      )
    },
    tool(
      "update_fact",
      "Correct a fact or retire it",
      "Change a fact's text, tags or lifecycle state. Nothing is ever deleted: mark a finished or obsolete item `done` " +
        "/ `archived` and it drops out of the default recall while staying findable on request."
    ) { (deps, p: UpdateFactParams) =>
      ZIO.when(p.id < 1 || p.body.exists(_.isEmpty) || !tagsOk(p.tags, 12))(
        invalid("id must be positive; body non-empty; at most 12 non-empty tags")
      ) *> envelope(deps.memory.updateFact(p.id, p.body, p.tags, p.state).map(OneFact(_)))
    },
    tool(
      "list_memory",
      "List projects, or the documents in one",
      "Without `project`: every project. With it: that project's documents, each with a one-line summary, version and " +
        "size.\n\nALWAYS call this before creating a document. It is what keeps a project from growing notes.md, " +
        "notes2.md and progress-new.md until nobody knows which one is current."
    ) { (deps, p: ListMemoryParams) =>
      p.project match
        case None       => envelope(deps.memory.listProjects.map(Projects(_)))
        case Some(slug) => envelope(deps.memory.listDocs(slug))
    },
    tool(
      "create_project",
      "Start a new project",
      "Create an empty project — a folder of markdown documents with progress, e.g. preparing for an interview on a " +
        "topic. Check list_memory first; reusing an existing project is almost always right."
    ) { (deps, p: CreateProjectParams) =>
      ZIO.when(p.slug.isEmpty || p.title.isEmpty)(invalid("slug and title must be non-empty")) *>
        envelope(deps.memory.createProject(p.slug, p.title).map(OneProject(_)))
    },
    tool(
      "read_doc",
      "Read a project document",
      "Return a document's full markdown plus its `version`. You need that version to patch it, and the text has to be " +
        "fresh or your quoted `old` strings won't match. Read immediately before writing."
    ) { (deps, p: ReadDocParams) => envelope(deps.memory.readDoc(p.project, p.doc).map(OneDoc(_))) },
    tool(
      "append_doc",
      "Add text to the end of a document (safe default)",
      "Append to a document, or to one section of it. THE PREFERRED WRITE: it cannot destroy existing text and needs no " +
        "prior read or version.\n\nUse it for anything that is a record of what happened — progress entries, notes, " +
        "mistakes. Progress is history: correct an old entry by appending a correction, never by editing the past."
    ) { (deps, p: AppendDocParams) =>
      ZIO.when(p.text.isEmpty)(invalid("text must be non-empty")) *>
        envelope(deps.memory.appendDoc(p.project, p.doc, p.text, p.under_heading, deps.memoryActor, p.rationale))
    },
    tool(
      "patch_doc",
      "Change existing text in a document",
      "Apply search/replace edits. Each `old` must appear EXACTLY ONCE — copy it verbatim from a fresh read_doc, " +
        "including punctuation, dashes and ё. `new: \"\"` deletes. All edits apply or none do.\n\nFailures come back " +
        "with what you need to fix them: `version_conflict` gives the current version, an ambiguous quote gives the " +
        "match count, and a miss gives the literal from the document that nearly matched."
    ) { (deps, p: PatchDocParams) =>
      ZIO.when(p.expected_version < 1 || p.edits.isEmpty || p.edits.exists(_.old.isEmpty))(
        invalid("expected_version must be ≥1; edits non-empty, each with a non-empty `old`")
      ) *> envelope(deps.memory.patchDoc(p.project, p.doc, p.expected_version, p.edits, deps.memoryActor, p.rationale))
    },
    tool(
      "write_doc",
      "Create a document, or replace one wholesale",
      "Create a new document, or overwrite an existing one entirely. THE ONLY OP THAT CAN LOSE CONTENT — prefer " +
        "append_doc for additions and patch_doc for changes; use this to create, or when the user explicitly asks to " +
        "rewrite from scratch.\n\nCreating: omit expected_version. Overwriting: read_doc first and pass its version. " +
        "Always give a `summary` — it is the line other agents see in list_memory."
    ) { (deps, p: WriteDocParams) =>
      ZIO.when(p.expected_version.exists(_ < 0))(invalid("expected_version must be ≥0")) *>
        envelope(
          deps.memory.writeDoc(
            WriteDoc(p.project, p.doc, p.body, p.summary, p.expected_version, deps.memoryActor, p.rationale)
          )
        )
    },
    tool(
      "doc_history",
      "Who changed a document, when and why",
      "List a document's patches, newest first: patch id, kind, actor, the rationale recorded at the time, and the " +
        "versions it moved between. Patch ids come from here — never invent one."
    ) { (deps, p: DocHistoryParams) =>
      ZIO.when(p.limit.exists(l => l < 1 || l > 100))(invalid("limit must be 1–100")) *>
        envelope(deps.memory.history(p.project, p.doc, p.limit.getOrElse(20)).map(History(_)))
    },
    tool(
      "revert_patch",
      "Undo a recorded change",
      "Undo one patch from doc_history. The newest patch always reverts exactly. An older one is undone in place when " +
        "later patches left its text alone; when they didn't, the call fails and tells you which version a rollback " +
        "would restore.\n\n`rollback: true` then restores the whole document to the state before that patch, " +
        "DISCARDING everything written after it — only do that when the user asked for it. Reverting is itself " +
        "recorded; history is never rewritten."
    ) { (deps, p: RevertParams) =>
      ZIO.when(p.patch_id.isEmpty)(invalid("patch_id must be non-empty")) *>
        envelope(deps.memory.revert(p.project, p.doc, p.patch_id, p.rollback.contains(true), deps.memoryActor))
    }
  )
