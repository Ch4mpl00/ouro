package mcp

// Personal knowledge base (`knowledge_base_notes`): freeform notes the user
// asked the agent to remember, recalled by meaning. Superseded by unified
// memory facts (Memory.scala imports these) but still served while skills
// reference add_note / find_notes.
//
// Sections:
//   1. repository — add (store + inline embed), find (vector search), backfill
//   2. tools      — `knowledge` toolset: add_note, find_notes

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.json.*

import java.sql.ResultSet

import Rows.*

// ── 1. repository ────────────────────────────────────────────────────────────

final case class NoteHit(
    id: Long,
    body: String,
    tags: List[String],
    source: Option[String],
    createdAt: String,
    updatedAt: String,
    distance: Double
) derives JsonEncoder

final case class NoteAdded(id: Long, embedded: Boolean, tags: List[String]) derives JsonEncoder

final case class StoredNote(id: Long, body: String, tags: List[String], source: Option[String])

final class KnowledgeRepository(pool: PgPool, embedder: Embedder):
  // Only the body is embedded; tags are filter metadata. A failed embed
  // leaves the vector NULL for the backfill.
  private def embedRows(rows: List[(Long, String)]): Task[EmbedResult] =
    if rows.isEmpty then ZIO.succeed(EmbedResult())
    else
      embedder.embedBatch(rows.map(_._2.trim)).either.flatMap {
        case Left(err) =>
          ZIO
            .logError(s"knowledge embed failed for ${rows.size} notes: ${Results.describe(err)}")
            .as(EmbedResult(0, rows.size))
        case Right(vectors) =>
          pool
            .withConnection { c =>
              rows.zip(vectors).foreach { case ((id, _), v) =>
                Sql.update(
                  c,
                  "UPDATE knowledge_base_notes SET embedding = ?::text::vector, embedded_at = now() WHERE id = ?",
                  Vectors.literal(v),
                  id
                )
              }
            }
            .as(EmbedResult(rows.size, 0))
      }

  def addNote(body: String, tags: Option[List[String]], source: Option[String]): Task[NoteAdded] =
    val clean = KnowledgeRepository.normalizeTags(tags)
    for
      rows <- pool.query(
        "INSERT INTO knowledge_base_notes (body, tags, source) VALUES (?, ?, ?) RETURNING id, body",
        body,
        clean,
        source
      )(rs => (rs.getLong("id"), rs.getString("body")))
      row = rows.head
      result <- embedRows(List(row))
    yield NoteAdded(row._1, result.embedded > 0, clean)

  def findNotes(query: String, k: Int, tags: Option[List[String]]): Task[List[NoteHit]] =
    val clean = KnowledgeRepository.normalizeTags(tags)
    val tagFilter = if clean.isEmpty then "" else "AND tags && ?"
    val sql =
      s"""SELECT id, body, tags, source, created_at, updated_at, (embedding <=> ?::text::vector) AS distance
            FROM knowledge_base_notes WHERE embedding IS NOT NULL $tagFilter
           ORDER BY distance LIMIT ?"""
    for
      vector <- embedder.embedOne(query)
      params = List(Vectors.literal(vector)) ++ Option.when(clean.nonEmpty)(clean) ++ List(k)
      hits <- pool.query(sql, params*)(KnowledgeRepository.hit)
    yield hits

  def embedMissingBatch(batch: Int): Task[EmbedResult] =
    pool
      .query("SELECT id, body FROM knowledge_base_notes WHERE embedding IS NULL LIMIT ?", batch)(rs =>
        (rs.getLong("id"), rs.getString("body"))
      )
      .flatMap(embedRows)

  // Every note, for the one-shot import into memory facts.
  def allNotes: Task[List[StoredNote]] =
    pool.query("SELECT id, body, tags, source FROM knowledge_base_notes")(rs =>
      StoredNote(
        rs.getLong("id"),
        rs.getString("body"),
        KnowledgeRepository.textArray(rs, "tags"),
        rs.optString("source")
      )
    )

object KnowledgeRepository:
  // Trim, drop empties, de-duplicate — keeping the model's own wording.
  def normalizeTags(tags: Option[List[String]]): List[String] =
    tags.getOrElse(Nil).map(_.trim).filter(_.nonEmpty).distinct

  def textArray(rs: ResultSet, col: String): List[String] =
    Option(rs.getArray(col)).map(_.getArray.asInstanceOf[Array[String]].toList).getOrElse(Nil)

  private def hit(rs: ResultSet): NoteHit = NoteHit(
    rs.getLong("id"),
    rs.getString("body"),
    textArray(rs, "tags"),
    rs.optString("source"),
    Time.iso(rs.instant("created_at")),
    Time.iso(rs.instant("updated_at")),
    rs.getDouble("distance")
  )

// ── 2. tools ─────────────────────────────────────────────────────────────────

object KnowledgeTools:
  import Tools.*

  final case class AddNoteParams(
      @description(
        "The fact to remember, as a self-contained sentence including its subject. This text is what semantic recall matches against."
      ) body: String,
      @description(
        "3–6 short lowercase topical tags you generate for this note. Used for the optional overlap filter in find_notes, not for semantic recall."
      ) tags: Option[List[String]],
      @description("Optional provenance, e.g. \"telegram\".") source: Option[String]
  ) derives JsonDecoder,
        Schema

  final case class FindNotesParams(
      @description("Natural-language description of what to recall.") query: String,
      @description("Max notes to return. Default 10.") @validate(Validator.inRange(1, 50)) limit: Option[Int],
      @description(
        "Restrict to notes sharing at least one of these tags (array overlap). Lowercase to match how tags are stored."
      ) tags: Option[List[String]]
  ) derives JsonDecoder,
        Schema

  final case class Found(count: Int, notes: List[NoteHit]) derives JsonEncoder

  val tools: List[ToolDef] = List(
    tool(
      "add_note",
      "Save a note to the personal knowledge base",
      "Persist a freeform fact the user asked you to remember (\"запомни, что …\", \"запиши …\", \"заметка: …\"). The " +
        "note becomes semantically searchable later via find_notes.\n\nYOU generate the tags: pick 3–6 short, " +
        "lowercase topical tags на свой вкус — the things you'd later search this note by (people, topics, objects), " +
        "e.g. [\"роутер\", \"пароль\", \"wifi\"]. Tags are metadata only: they help filtering and scanning, but recall " +
        "runs over the note TEXT, so write a self-contained `body` that names its subject explicitly (\"Лёша платит за " +
        "интернет 1-го числа\", not \"платит 1-го\"). Returns the new note id."
    ) { (deps, p: AddNoteParams) =>
      val badTags = p.tags.exists(t => t.size > 12 || t.exists(_.isEmpty))
      ZIO.when(p.body.isEmpty || badTags)(invalid("body must be non-empty; at most 12 non-empty tags")) *>
        deps.knowledge.addNote(p.body, p.tags, p.source)
    },
    tool(
      "find_notes",
      "Semantic search over the personal knowledge base",
      "Recall notes saved with add_note by meaning, not exact wording (\"что ты помнишь про роутер?\", \"когда Лёша " +
        "платит за интернет?\", \"напомни пароль от роутера\"). Returns the closest notes by semantic similarity to " +
        "`query`, each with body, tags, source, created_at and distance (lower = closer). Optionally pass `tags` to " +
        "additionally restrict to notes sharing at least one tag. This is the ONLY way to read the knowledge base — " +
        "use it whenever the user asks what you know/remember about something personal."
    ) { (deps, p: FindNotesParams) =>
      ZIO.when(p.query.isEmpty || p.limit.exists(l => l < 1 || l > 50))(
        invalid("query must be non-empty; limit 1–50")
      ) *>
        deps.knowledge.findNotes(p.query, p.limit.getOrElse(10), p.tags).map(notes => Found(notes.size, notes))
    }
  )
