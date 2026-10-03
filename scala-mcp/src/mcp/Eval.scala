package mcp

// The RAG eval harness: a frozen corpus snapshot + labelled queries, scored
// under a config (embedding model, text composition, dedup). It re-embeds the
// corpus from text, so configs compare on one yardstick. Fixtures and their
// labelling rules: crates/mcp/eval/fixtures/README.md.
//
// Sections:
//   1. types   — corpus rows, queries, config, results
//   2. config  — loading/validating a config + the corpus-cache key
//   3. metrics — recall, precision, MRR, source diversity
//   4. cache   — corpus vectors per config hash (JSONL)
//   5. run     — embed, retrieve, dedup, score
//   6. report  — the markdown report
//   7. inspect — per-query top-k listing for debugging labels

import zio.*
import zio.json.*
import zio.json.ast.Json

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.Files
import java.nio.file.Path
import java.security.MessageDigest

// ── 1. types ─────────────────────────────────────────────────────────────────

final case class CorpusRow(
    id: Long,
    source: String,
    title: Option[String],
    body: String,
    metadata: Json.Obj = Json.Obj()
) derives JsonDecoder:
  // One "source" for diversity counting: the channel for Telegram posts, the
  // feed name otherwise.
  def bucket: String = metadata.get("chat_title").flatMap(_.asString).getOrElse(source)

final case class QueryRow(id: String, query: String, reformulation: String, gold: List[Long], acceptable: List[Long])
    derives JsonDecoder

enum BuildText:
  case TitleBody, BodyOnly

object BuildText:
  given JsonCodec[BuildText] = JsonCodec.string.transformOrFail(
    {
      case "title+body" => Right(TitleBody)
      case "body-only"  => Right(BodyOnly)
      case other        => Left(s"unknown buildText $other")
    },
    {
      case TitleBody => "title+body"
      case BodyOnly  => "body-only"
    }
  )

enum QueryField derives JsonCodec:
  case query, reformulation

final case class EmbedConfig(model: String, dimensions: Int) derives JsonCodec
final case class DedupConfig(threshold: Double) derives JsonCodec
final case class RetrievalConfig(
    embed: EmbedConfig,
    buildText: BuildText,
    topK: Int,
    dedup: Option[DedupConfig] = None,
    // Reserved; always null today.
    rerank: Option[Json] = None
) derives JsonCodec
final case class QueryConfig(field: QueryField) derives JsonCodec
final case class ScoringConfig(mode: String) derives JsonCodec
final case class EvalConfig(name: String, retrieval: RetrievalConfig, query: QueryConfig, scoring: ScoringConfig)
    derives JsonCodec

final case class Retrieved(id: Long, distance: Double)

final case class PerQuery(
    qid: String,
    query: String,
    goldCount: Int,
    hitAt5: Int,
    hitAt10: Int,
    hitAt30: Int,
    precisionAt5: Double,
    precisionAt10: Double,
    uniqueSourcesAt5: Int,
    uniqueSourcesAt10: Int,
    firstGoldRank: Option[Int],
    distToFirstGold: Option[Double]
)

final case class NegativeTest(qid: String, query: String, minDistance: Double, top1Id: Long)

final case class Aggregate(
    scoredQueries: Int,
    recallAt5: Double,
    recallAt10: Double,
    recallAt30: Double,
    precisionAt5: Double,
    precisionAt10: Double,
    meanUniqueSourcesAt5: Double,
    meanUniqueSourcesAt10: Double,
    mrr: Double,
    meanDistToFirstGold: Option[Double]
)

final case class EvalResult(
    config: EvalConfig,
    configHash: String,
    perQuery: List[PerQuery],
    negativeTests: List[NegativeTest],
    aggregate: Aggregate,
    cacheHit: Boolean
)

final case class EvalPaths(corpus: Path, queries: Path, cacheDir: Path)

object EvalPaths:
  def under(dir: Path): EvalPaths =
    EvalPaths(dir.resolve("fixtures/corpus.jsonl"), dir.resolve("fixtures/queries.jsonl"), dir.resolve("cache"))

object Eval:
  // Always retrieve this many so R@30 is computable whatever topK says
  // (topK stays a hint of what a skill would actually show).
  private val RetrievalSize = 30
  // Cyrillic runs ~6 bytes/token, so the prod 8000-char cap overflows the
  // model's 8192-token limit on some posts; 6000 stays inside it.
  val MaxChars = 6000

  // ── 2. config ──────────────────────────────────────────────────────────────

  def loadConfig(path: Path): Task[EvalConfig] =
    for
      raw <- ZIO.attemptBlocking(Files.readString(path))
      config <- ZIO.fromEither(raw.fromJson[EvalConfig]).mapError(e => RuntimeException(s"$path: $e"))
      _ <- check(config.name.nonEmpty, "config.name must be a non-empty string")
      _ <- check(config.retrieval.topK > 0, "config.retrieval.topK must be a positive number")
      _ <- check(
        config.retrieval.dedup.forall(_.threshold >= 0),
        "config.retrieval.dedup.threshold must be a non-negative number"
      )
      _ <- check(config.scoring.mode == "binary", "config.scoring.mode must be 'binary' (only mode supported today)")
    yield config

  private def check(ok: Boolean, message: String): Task[Unit] = ZIO.unless(ok)(ZIO.fail(RuntimeException(message))).unit

  // Only what changes the corpus vectors — model, dimensions, text
  // composition — so swapping the query field or scoring keeps the cache.
  def hashCorpusInputs(config: EvalConfig): String =
    val key = Json.Obj(
      "model" -> Json.Str(config.retrieval.embed.model),
      "dimensions" -> Json.Num(config.retrieval.embed.dimensions),
      "buildText" -> config.retrieval.buildText.toJsonAST.toOption.get
    )
    val digest = MessageDigest.getInstance("SHA-256").digest(key.toJson.getBytes(UTF_8))
    digest.map(b => f"$b%02x").mkString.take(16)

  def buildText(row: CorpusRow, mode: BuildText): String =
    val title = row.title.getOrElse("").trim
    val body = row.body.trim
    mode match
      case BuildText.TitleBody if title.nonEmpty => s"$title\n\n$body"
      case _                                     => body

  // ── 3. metrics ─────────────────────────────────────────────────────────────

  private def countHits(gold: Set[Long], items: List[Retrieved]) = items.count(i => gold.contains(i.id))

  def firstGoldRank(gold: List[Long], top: List[Retrieved]): Option[Int] =
    Some(top.indexWhere(i => gold.contains(i.id))).filter(_ >= 0).map(_ + 1)

  def mean(values: List[Double]): Double = if values.isEmpty then Double.NaN else values.sum / values.size

  private def uniqueSources(items: List[Retrieved], corpus: Map[Long, CorpusRow]) =
    items.flatMap(i => corpus.get(i.id)).map(_.bucket).distinct.size

  def scoreQuery(q: QueryRow, top: List[Retrieved], corpus: Map[Long, CorpusRow]): PerQuery =
    val gold = q.gold.toSet
    val (h5, h10, h30) = (countHits(gold, top.take(5)), countHits(gold, top.take(10)), countHits(gold, top.take(30)))
    PerQuery(
      q.id,
      q.query,
      q.gold.size,
      h5,
      h10,
      h30,
      h5 / 5.0,
      h10 / 10.0,
      uniqueSources(top.take(5), corpus),
      uniqueSources(top.take(10), corpus),
      firstGoldRank(q.gold, top),
      top.find(i => gold.contains(i.id)).map(_.distance)
    )

  def aggregate(perQuery: List[PerQuery]): Aggregate =
    val scored = perQuery.filter(_.goldCount > 0)
    def m(f: PerQuery => Double) = mean(scored.map(f))
    val distances = scored.flatMap(_.distToFirstGold)
    Aggregate(
      scored.size,
      m(p => p.hitAt5.toDouble / p.goldCount),
      m(p => p.hitAt10.toDouble / p.goldCount),
      m(p => p.hitAt30.toDouble / p.goldCount),
      m(_.precisionAt5),
      m(_.precisionAt10),
      m(_.uniqueSourcesAt5.toDouble),
      m(_.uniqueSourcesAt10.toDouble),
      m(_.firstGoldRank.fold(0.0)(r => 1.0 / r)),
      Option.when(distances.nonEmpty)(mean(distances))
    )

  // ── 4. cache ───────────────────────────────────────────────────────────────

  final private case class CachedVector(id: Long, embedding: Vector[Float]) derives JsonCodec

  def readCache(dir: Path, hash: String): Task[Option[Map[Long, Vector[Float]]]] =
    val path = dir.resolve(s"$hash.jsonl")
    ZIO.attemptBlocking(Files.exists(path)).flatMap {
      case false => ZIO.none
      case true  => loadJsonl[CachedVector](path).map(vs => Some(vs.map(v => v.id -> v.embedding).toMap))
    }

  def writeCache(dir: Path, hash: String, vectors: Map[Long, Vector[Float]]): Task[Unit] =
    ZIO.attemptBlocking {
      Files.createDirectories(dir)
      Files.writeString(dir.resolve(s"$hash.jsonl"), vectors.map((id, e) => CachedVector(id, e).toJson + "\n").mkString)
    }.unit

  // ── 5. run ─────────────────────────────────────────────────────────────────

  def loadJsonl[A: JsonDecoder](path: Path): Task[List[A]] =
    ZIO.attemptBlocking(Files.readAllLines(path)).flatMap { lines =>
      ZIO.foreach(scala.jdk.CollectionConverters.ListHasAsScala(lines).asScala.toList.filter(_.trim.nonEmpty))(l =>
        ZIO.fromEither(l.fromJson[A]).mapError(e => RuntimeException(s"$path: $e"))
      )
    }

  def corpusVectors(
      corpus: List[CorpusRow],
      config: EvalConfig,
      embedder: Embedder,
      cacheDir: Path
  ): Task[(Map[Long, Vector[Float]], Boolean)] =
    val hash = hashCorpusInputs(config)
    readCache(cacheDir, hash).flatMap {
      case Some(cached) if cached.size == corpus.size => ZIO.succeed((cached, true))
      case _                                          =>
        for
          vectors <- embedder.embedBatch(corpus.map(buildText(_, config.retrieval.buildText)))
          _ <- check(vectors.size == corpus.size, "missing embeddings for corpus rows")
          map = corpus.map(_.id).zip(vectors).toMap
          _ <- writeCache(cacheDir, hash, map)
        yield (map, false)
    }

  // Cosine top-k over the corpus, then dedup over a 2× pool so k survive.
  def retrieve(
      query: Vector[Float],
      corpus: Map[Long, Vector[Float]],
      ids: List[Long],
      k: Int,
      dedup: Option[Double]
  ): List[Retrieved] =
    val poolSize = if dedup.isDefined then (k * 2).max(30) else k
    val scored = ids
      .flatMap(id => corpus.get(id).map(v => Retrieved(id, Retrieval.cosineDistance(query, v))))
      .sortBy(_.distance)
      .take(poolSize)
    val kept =
      dedup.fold(scored)(t => Retrieval.dedupByPairwiseCosine(scored, r => corpus.get(r.id), t, keepNull = false))
    kept.take(k)

  private def queryText(q: QueryRow, field: QueryField) = field match
    case QueryField.query         => q.query
    case QueryField.reformulation => q.reformulation

  def run(config: EvalConfig, paths: EvalPaths, embedder: Embedder): Task[EvalResult] =
    for
      corpus <- loadJsonl[CorpusRow](paths.corpus)
      queries <- loadJsonl[QueryRow](paths.queries)
      (vectors, cacheHit) <- corpusVectors(corpus, config, embedder, paths.cacheDir)
      ids = corpus.map(_.id)
      byId = corpus.map(r => r.id -> r).toMap
      queryVectors <- embedder.embedBatch(queries.map(queryText(_, config.query.field)))
      dedup = config.retrieval.dedup.map(_.threshold)
      scored = queries.zip(queryVectors).map { (q, v) =>
        val top = retrieve(v, vectors, ids, RetrievalSize, dedup)
        if q.gold.isEmpty && q.acceptable.isEmpty then
          Left(NegativeTest(q.id, q.query, top.headOption.fold(Double.NaN)(_.distance), top.headOption.fold(-1L)(_.id)))
        else Right(scoreQuery(q, top, byId))
      }
      perQuery = scored.collect { case Right(p) => p }
    yield EvalResult(
      config,
      hashCorpusInputs(config),
      perQuery,
      scored.collect { case Left(n) => n },
      aggregate(perQuery),
      cacheHit
    )

  // ── 6. report ──────────────────────────────────────────────────────────────

  private def fmt(n: Double) = if n.isNaN then "—" else f"$n%.3f"
  private def escapePipe(s: String) = s.replace("|", "\\|")

  def renderMarkdown(r: EvalResult): String =
    val a = r.aggregate
    val header = List(
      s"# Eval: ${r.config.name}",
      "",
      s"Config hash: `${r.configHash}` ${if r.cacheHit then "(corpus cache hit)" else "(fresh embed)"}",
      "",
      "## Config",
      "```json",
      r.config.toJsonPretty,
      "```",
      "",
      "## Aggregate",
      "",
      "| Metric | Value |",
      "|---|---|",
      s"| Scored queries (gold > 0) | ${a.scoredQueries} |",
      s"| Precision@5 | ${fmt(a.precisionAt5)} |",
      s"| Precision@10 | ${fmt(a.precisionAt10)} |",
      s"| Recall@5 | ${fmt(a.recallAt5)} |",
      s"| Recall@10 | ${fmt(a.recallAt10)} |",
      s"| Recall@30 | ${fmt(a.recallAt30)} |",
      s"| MRR | ${fmt(a.mrr)} |",
      s"| Mean unique sources @5 | ${fmt(a.meanUniqueSourcesAt5)} |",
      s"| Mean unique sources @10 | ${fmt(a.meanUniqueSourcesAt10)} |",
      s"| Mean distance to first gold | ${a.meanDistToFirstGold.fold("—")(fmt)} |",
      "",
      "## Per-query",
      "",
      "| qid | query | gold | hit@5 | hit@10 | hit@30 | P@5 | P@10 | uniq@5 | uniq@10 | first-gold rank | dist to gold |",
      "|---|---|---|---|---|---|---|---|---|---|---|---|"
    )
    val rows = r.perQuery.map { q =>
      def hits(h: Int) = if q.goldCount > 0 then s"$h/${q.goldCount}" else "—"
      s"| ${q.qid} | ${escapePipe(q.query)} | ${q.goldCount} | ${hits(q.hitAt5)} | ${hits(q.hitAt10)} | ${hits(q.hitAt30)} | " +
        s"${fmt(q.precisionAt5)} | ${fmt(q.precisionAt10)} | ${q.uniqueSourcesAt5} | ${q.uniqueSourcesAt10} | " +
        s"${q.firstGoldRank.fold("—")(_.toString)} | ${q.distToFirstGold.fold("—")(fmt)} |"
    }
    val negatives =
      if r.negativeTests.isEmpty then Nil
      else
        List(
          "## Negative tests (gold = 0)",
          "",
          "Queries where the corpus has nothing relevant. Min distance is the closest",
          "result the retriever surfaced — if it's low (< ~0.5), the retriever is",
          "confidently wrong; high distance is correct \"I don't know\" behaviour.",
          "",
          "| qid | query | min distance | top-1 id |",
          "|---|---|---|---|"
        ) ++ r.negativeTests.map(n =>
          s"| ${n.qid} | ${escapePipe(n.query)} | ${fmt(n.minDistance)} | ${n.top1Id} |"
        ) :+ ""
    (header ++ rows ++ ("" :: negatives)).mkString("\n")

  // ── 7. inspect ─────────────────────────────────────────────────────────────

  private def shortTitle(row: CorpusRow, max: Int): String =
    val oneLine = row.title.getOrElse(truncateChars(row.body, 200)).split("\\s+").filter(_.nonEmpty).mkString(" ")
    if oneLine.codePointCount(0, oneLine.length) > max then truncateChars(oneLine, max - 1) + "…" else oneLine

  def inspect(config: EvalConfig, paths: EvalPaths, embedder: Embedder, qids: List[String], k: Int): Task[String] =
    for
      corpus <- loadJsonl[CorpusRow](paths.corpus)
      all <- loadJsonl[QueryRow](paths.queries)
      queries <- ZIO.foreach(qids)(qid =>
        ZIO.fromOption(all.find(_.id == qid)).orElseFail(RuntimeException(s"unknown qid: $qid"))
      )
      (vectors, cacheHit) <- corpusVectors(corpus, config, embedder, paths.cacheDir)
      ids = corpus.map(_.id)
      byId = corpus.map(r => r.id -> r).toMap
      queryVectors <- embedder.embedBatch(queries.map(queryText(_, config.query.field)))
    yield
      def src(id: Long) = byId.get(id).fold("?")(r => truncateChars(r.bucket, 30))
      val blocks = queries.zip(queryVectors).flatMap { (q, v) =>
        val top = retrieve(v, vectors, ids, k, config.retrieval.dedup.map(_.threshold))
        val (gold, acceptable) = (q.gold.toSet, q.acceptable.toSet)
        def hits(n: Int) = countHits(gold, top.take(n))
        val ranked = top.zipWithIndex.map { (item, rank) =>
          val mark = if gold(item.id) then "GOLD" else if acceptable(item.id) then "okay" else "    "
          val title = byId.get(item.id).fold("<row not found>")(shortTitle(_, 120))
          f"${rank + 1}%4d  ${item.distance}%.3f  $mark  ${src(item.id)}%-30s  $title"
        }
        val retrieved = top.map(_.id).toSet
        val missed = q.gold.filterNot(retrieved)
        val missedLines =
          if missed.isEmpty then Nil
          else
            s"missed gold (not in top-$k): ${missed.size}" ::
              missed.take(8).map { id =>
                val title = byId.get(id).fold("<not in corpus>")(shortTitle(_, 120))
                f"        id=$id%4d              ${src(id)}%-30s  $title"
              } ++ Option.when(missed.size > 8)(s"        …and ${missed.size - 8} more")
        List(
          s"━━━ ${q.id} ━━━",
          s"query:         ${q.query}",
          s"reformulation: ${q.reformulation}",
          s"gold: ${q.gold.size} · acceptable: ${q.acceptable.size}\n",
          s"gold hits  top-5: ${hits(5)}/${q.gold.size}   top-10: ${hits(10)}/${q.gold.size}   top-$k: ${hits(k)}/${q.gold.size}\n",
          "rank  dist   mark  source                          title",
          "────  ─────  ────  ──────                          ─────"
        ) ++ ranked ++ ("" :: missedLines) :+ ""
      }
      ((if cacheHit then s"[inspect] corpus cache hit (${vectors.size} rows)\n" else "") :: blocks).mkString("\n")
