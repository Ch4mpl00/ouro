package mcp

// The news / RAG store: HN, Habr and harvested Telegram channel posts in one
// `news_items` table, embedded on the way in, searched by meaning.
//
// Sections:
//   1. items      — the unified item and the shapes the tools return
//   2. article    — URL → readable plain text (Readability)
//   3. providers  — Hacker News, Habr, Telegram channels
//   4. repository — save / upsert / list / vector search over Postgres
//   5. ranking    — merging per-query pools: min distance, then dedup
//   6. poller     — one cadence loop over every provider
//   7. tools      — `news-read` toolset: search_news, list_news, fetch_article

import com.rometools.rome.io.SyndFeedInput
import com.rometools.rome.io.XmlReader
import net.dankito.readability4j.Readability4J
import org.jsoup.Jsoup
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.http.Header
import zio.http.Headers
import zio.json.*
import zio.json.ast.Json

import java.io.ByteArrayInputStream
import java.net.URI
import java.sql.ResultSet
import java.time.Instant
import scala.jdk.CollectionConverters.*

import Rows.*

// ── 1. items ─────────────────────────────────────────────────────────────────

// `externalId` is the natural key in the source: the URL for HN/Habr,
// "<chat_id>:<tg_message_id>" for channel posts.
final case class NewsItem(
    source: String,
    externalId: String,
    title: Option[String],
    url: Option[String],
    body: String,
    metadata: Json.Obj,
    postedAt: Option[Instant]
):
  def toJson: Json = Json.Obj(
    "source" -> Json.Str(source),
    "externalId" -> Json.Str(externalId),
    "title" -> title.fold(Json.Null)(Json.Str(_)),
    "url" -> url.fold(Json.Null)(Json.Str(_)),
    "body" -> Json.Str(body),
    "metadata" -> metadata,
    "postedAt" -> postedAt.fold(Json.Null)(t => Json.Str(Time.iso(t)))
  )

final case class SaveResult(saved: Int = 0, embedded: Int = 0, failed: Int = 0) derives JsonEncoder

final case class NewsFilter(
    source: Option[String] = None,
    since: Option[Instant] = None,
    until: Option[Instant] = None,
    // Point-in-time replay for evals: what was searchable (embedded_at) or
    // stored (fetched_at) at this instant. Not the same as `until`, which
    // bounds posted_at.
    asOf: Option[Instant] = None,
    // Channel posts only: metadata.chat_username OR chat_id.
    channel: Option[String] = None
)

final case class SearchResult(
    id: Long,
    source: String,
    title: Option[String],
    url: Option[String],
    snippet: String,
    postedAt: Option[String],
    distance: Double,
    metadata: Json.Obj,
    // Batch path only: indices of the queries that surfaced this item.
    matchedQueries: Option[List[Int]]
)

object SearchResult:
  given JsonEncoder[SearchResult] = JsonEncoder[Json].contramap { r =>
    val fields = List(
      "id" -> Json.Num(r.id),
      "source" -> Json.Str(r.source),
      "title" -> r.title.fold(Json.Null)(Json.Str(_)),
      "url" -> r.url.fold(Json.Null)(Json.Str(_)),
      "snippet" -> Json.Str(r.snippet),
      "postedAt" -> r.postedAt.fold(Json.Null)(Json.Str(_)),
      "distance" -> Json.Num(r.distance),
      "metadata" -> r.metadata
    ) ++ r.matchedQueries.map(qs => "matchedQueries" -> Json.Arr(qs.map(Json.Num(_))*))
    Json.Obj(fields*)
  }

// ── 2. article ───────────────────────────────────────────────────────────────

final case class Article(
    url: String,
    title: String,
    text: String,
    site: Option[String],
    // Unparseable dates are dropped rather than poisoning the row.
    publishedAt: Option[Instant],
    author: Option[String]
)

final class ArticleFetcher(http: HttpClient):
  def fetch(url: String): Task[Article] =
    for
      reply <- http.get(url, Headers(Header.Custom("User-Agent", Fetcher.BrowserUa)), timeout = 30.seconds)
      _ <- ZIO.unless(reply.ok)(Tools.fail(s"No article extracted from $url (HTTP ${reply.status})"))
      article <- ZIO.attemptBlocking(News.extractArticle(url, reply.body)).flatMap(Tools.orFail)
    yield article

  // Three attempts with a short backoff; None on terminal failure so a
  // poller simply drops the item.
  def fetchWithRetry(url: String): UIO[Option[Article]] =
    fetch(url)
      .retry(Schedule.recurs(2) && Schedule.linear(500.millis))
      .foldZIO(
        err => ZIO.logWarning(s"article fetch failed after 3 attempts: $url: ${Results.describe(err)}").as(None),
        a => ZIO.some(a)
      )

object News:
  def extractArticle(url: String, html: String): Either[String, Article] =
    val parsed = Readability4J(url, html).parse()
    Option(parsed.getContent).filter(_.trim.nonEmpty) match
      case None          => Left(s"No article extracted from $url")
      case Some(content) =>
        // Readability4J has no site/date extraction; the meta tags it would
        // read are one jsoup query away.
        val doc = Jsoup.parse(html)
        def meta(property: String) =
          Option(doc.selectFirst(s"meta[property=$property], meta[name=$property]"))
            .map(_.attr("content").trim)
            .filter(_.nonEmpty)
        val host = scala.util.Try(URI(url).getHost).toOption.flatMap(Option(_)).map(_.stripPrefix("www."))
        Right(
          Article(
            url,
            Option(parsed.getTitle).getOrElse(""),
            stripHtml(content),
            meta("og:site_name").orElse(host),
            meta("article:published_time").flatMap(Time.parseJsDate),
            Option(parsed.getByline).map(_.trim).filter(_.nonEmpty)
          )
        )

  // A compact text blob for the LLM: whatever markup the extractor left, gone.
  def stripHtml(html: String): String =
    html
      .replaceAll("(?is)<script[\\s\\S]*?</script>", " ")
      .replaceAll("(?is)<style[\\s\\S]*?</style>", " ")
      .replaceAll("<[^>]+>", " ")
      .replace("&nbsp;", " ")
      .replace("&amp;", "&")
      .replace("&lt;", "<")
      .replace("&gt;", ">")
      .replace("&quot;", "\"")
      .replace("&#39;", "'")
      .replaceAll("\\s+", " ")
      .trim

  // ── 3. providers (shared mapping) ─────────────────────────────────────────

  final case class Headline(
      title: String,
      url: String,
      author: Option[String],
      postedAt: Option[Instant],
      // Source-specific extras, written into metadata ahead of author/site.
      extra: List[(String, Json)] = Nil
  )

  // The shared article mapping. Keys whose value is absent are omitted, as
  // JSON.stringify dropped `undefined`; the headline's author wins.
  def articleItem(source: String, headline: Headline, article: Option[Article]): Option[NewsItem] =
    article.filter(_.text.trim.nonEmpty).map { a =>
      val metadata = headline.extra ++
        headline.author.orElse(a.author).map(v => "author" -> Json.Str(v)) ++
        a.site.map(v => "site" -> Json.Str(v))
      NewsItem(
        source,
        headline.url,
        Some(if a.title.isEmpty then headline.title else a.title),
        Some(headline.url),
        a.text,
        Json.Obj(metadata*),
        a.publishedAt.orElse(headline.postedAt)
      )
    }

  def fetchArticles(source: String, articles: ArticleFetcher, headlines: List[Headline]): UIO[List[NewsItem]] =
    ZIO.foreachPar(headlines)(h => articles.fetchWithRetry(h.url).map(articleItem(source, h, _))).map(_.flatten)

  val HeadlineLimit = 30
  val ChannelSource = "channel"
  val BootstrapLimit = 50
  val DeltaLimit = 200

  def channelItem(chatId: String, title: Option[String], username: Option[String], m: ChannelMessage): NewsItem =
    def opt[A](a: Option[A])(f: A => Json) = a.fold(Json.Null)(f)
    NewsItem(
      ChannelSource,
      s"$chatId:${m.id}",
      // Always None: the channel name repeated on every post would only
      // pollute the embeddings. It lives in metadata.chat_title.
      None,
      username.map(u => s"https://t.me/$u/${m.id}"),
      m.text,
      Json.Obj(
        "chat_id" -> Json.Str(chatId),
        "chat_title" -> opt(title)(Json.Str(_)),
        "chat_username" -> opt(username)(Json.Str(_)),
        "tg_message_id" -> Json.Num(m.id),
        "views" -> opt(m.views)(Json.Num(_)),
        "forwards" -> opt(m.forwards)(Json.Num(_))
      ),
      Some(m.date)
    )

  // ── 5. ranking ─────────────────────────────────────────────────────────────

  final case class PoolRow(
      id: Long,
      source: String,
      title: Option[String],
      url: Option[String],
      body: String,
      metadata: Json.Obj,
      postedAt: Option[Instant],
      distance: Double,
      embedding: Option[Vector[Float]]
  )

  val SnippetChars = 400

  def snippet(body: String): String =
    if body.codePointCount(0, body.length) <= SnippetChars then body else truncateChars(body, SnippetChars) + "…"

  // Each pool is one query's rows, ascending by distance. An item several
  // queries found is kept once at its best distance; `matchedQueries` says
  // which facets surfaced it. Pure, so the ranking contract is testable
  // without a database.
  def mergeRankedPools(pools: List[List[PoolRow]], k: Int, threshold: Double, annotate: Boolean): List[SearchResult] =
    val merged = pools.zipWithIndex
      .flatMap((rows, qi) => rows.map(_ -> qi))
      .foldLeft(Vector.empty[(PoolRow, List[Int])]) { case (acc, (row, qi)) =>
        acc.indexWhere(_._1.id == row.id) match
          case -1 => acc :+ (row -> List(qi))
          case i  =>
            val (best, matched) = acc(i)
            acc.updated(i, (if row.distance < best.distance then row else best) -> (matched :+ qi))
      }
      .sortBy(_._1.distance)
      .toList
    Retrieval
      .dedupByPairwiseCosine(merged, (row, _) => row.embedding, threshold, keepNull = false)
      .take(k)
      .map { (row, matched) =>
        SearchResult(
          row.id,
          row.source,
          row.title,
          row.url,
          snippet(row.body),
          row.postedAt.map(Time.iso),
          row.distance,
          row.metadata,
          Option.when(annotate)(matched)
        )
      }

  // Exactly n contiguous parts, sizes differing by at most one (trailing ones
  // may be empty) — a workflow references ${bind.chunks.0} … statically, and
  // neighbouring items (time- or relevance-ordered) are the ones most likely
  // to describe the same event.
  def splitChunks[T](items: List[T], n: Int): List[List[T]] =
    val (base, extra) = (items.size / n, items.size % n)
    val sizes = (0 until n).map(i => base + (if i < extra then 1 else 0)).toList
    sizes
      .foldLeft((items, List.empty[List[T]])) { case ((rest, out), size) => (rest.drop(size), rest.take(size) :: out) }
      ._2
      .reverse

// ── 3. providers ─────────────────────────────────────────────────────────────

// A self-contained source. The poller ticks it every `cadence`, stores what
// it returns; quirks (endpoints, watermarks) stay inside the provider.
trait NewsProvider:
  def source: String
  def cadence: Duration
  def fetch: Task[List[NewsItem]]

final class HackerNews(http: HttpClient) extends NewsProvider:
  private val Api = "https://hacker-news.firebaseio.com/v0"
  private val articles = ArticleFetcher(http)

  val source = "hackernews"
  val cadence: Duration = 15.minutes

  final private case class Item(
      title: Option[String],
      url: Option[String],
      score: Option[Long],
      descendants: Option[Long],
      by: Option[String],
      time: Option[Long]
  ) derives JsonDecoder

  private def headline(id: Long): UIO[Option[News.Headline]] =
    http
      .get(s"$Api/item/$id.json")
      .map(_.as[Item].toOption)
      .orElseSucceed(None)
      .map(_.flatMap { item =>
        item.title.map { title =>
          News.Headline(
            title,
            item.url.getOrElse(s"https://news.ycombinator.com/item?id=$id"),
            item.by,
            item.time.map(Instant.ofEpochSecond),
            List("hn_id" -> Json.Num(id)) ++ item.score.map(s => "score" -> Json.Num(s)) ++
              item.descendants.map(c => "comments" -> Json.Num(c))
          )
        }
      })

  def fetch: Task[List[NewsItem]] =
    for
      reply <- http.get(s"$Api/topstories.json")
      _ <- ZIO.unless(reply.ok)(Tools.fail(s"HN topstories failed: ${reply.status}"))
      ids <- ZIO.fromEither(reply.as[List[Long]]).mapError(ToolFailure(_))
      headlines <- ZIO.foreachPar(ids.take(News.HeadlineLimit))(headline).map(_.flatten)
      items <- News.fetchArticles(source, articles, headlines)
    yield items

// The overall feed; downstream filters by interest.
final class Habr(http: HttpClient) extends NewsProvider:
  private val articles = ArticleFetcher(http)

  val source = "habr"
  val cadence: Duration = 30.minutes

  def headlines(xml: Array[Byte]): List[News.Headline] =
    val feed = SyndFeedInput().build(XmlReader(ByteArrayInputStream(xml)))
    feed.getEntries.asScala.toList.take(News.HeadlineLimit).flatMap { entry =>
      for
        title <- Option(entry.getTitle).map(_.trim).filter(_.nonEmpty)
        url <- Option(entry.getLink).filter(_.nonEmpty)
      yield News.Headline(
        title,
        url,
        Option(entry.getAuthor).filter(_.nonEmpty),
        Option(entry.getPublishedDate).orElse(Option(entry.getUpdatedDate)).map(_.toInstant)
      )
    }

  def fetch: Task[List[NewsItem]] =
    for
      reply <- http.get("https://habr.com/ru/rss/all/", timeout = 15.seconds)
      parsed <- ZIO.attempt(headlines(reply.body.getBytes(java.nio.charset.StandardCharsets.UTF_8)))
      items <- News.fetchArticles(source, articles, parsed)
    yield items

// Every channel dialog the userbot follows. Per-channel watermark = the max
// tg_message_id already stored for that chat. A new channel gets its last
// BootstrapLimit posts, then DeltaLimit at a time; a short pause between
// channels keeps clear of FLOOD_WAIT. No userbot session → nothing, not an
// error.
final class TelegramChannels(userbot: Userbot, repository: NewsRepository) extends NewsProvider:
  val source: String = News.ChannelSource
  val cadence: Duration = 30.minutes

  def fetch: Task[List[NewsItem]] =
    userbot.hasSession.flatMap {
      case false =>
        ZIO.logWarning("no saved userbot session — run `pnpm userbot:auth`. Skipping channels tick.").as(Nil)
      case true =>
        userbot.listChannels.flatMap { channels =>
          ZIO
            .foreach(channels) { channel =>
              for
                watermark <- repository.channelWatermark(channel.chatId)
                limit = if watermark.isEmpty then News.BootstrapLimit else News.DeltaLimit
                items <- userbot
                  .fetchMessages(channel, watermark, limit)
                  .map(_.map(News.channelItem(channel.chatId, channel.title, channel.username, _)))
                  .catchAll(err =>
                    ZIO
                      .logWarning(
                        s"channel fetch failed: ${channel.title.getOrElse(channel.chatId)}: ${Results.describe(err)}"
                      )
                      .as(Nil)
                  )
                _ <- ZIO.sleep(200.millis)
              yield items
            }
            .map(_.flatten)
        }
    }

// ── 4. repository ────────────────────────────────────────────────────────────

final class NewsRepository(pool: PgPool, embedder: Embedder):
  import NewsRepository.*

  // Embedding failures never fail a write: rows keep a NULL vector and the
  // backfill picks them up.
  private def embed(rows: List[EmbedRow]): Task[EmbedResult] =
    if rows.isEmpty then ZIO.succeed(EmbedResult())
    else
      embedder.embedBatch(rows.map(_.text)).either.flatMap {
        case Left(err) =>
          ZIO
            .logError(s"news embed failed for ${rows.size} rows: ${Results.describe(err)}")
            .as(EmbedResult(0, rows.size))
        case Right(vectors) =>
          pool
            .withConnection { c =>
              rows.zip(vectors).foreach { (row, v) =>
                Sql.update(
                  c,
                  "UPDATE news_items SET embedding = ?::text::vector, embedded_at = now() WHERE id = ?",
                  Vectors.literal(v),
                  row.id
                )
              }
            }
            .as(EmbedResult(rows.size, 0))
      }

  def save(items: List[NewsItem]): Task[SaveResult] =
    if items.isEmpty then ZIO.succeed(SaveResult())
    else
      val values = items.map(_ => "(?, ?, ?, ?, ?, ?::jsonb, ?)").mkString(", ")
      val params =
        items.flatMap(i => List(i.source, i.externalId, i.title, i.url, i.body, i.metadata.toJson, i.postedAt))
      val sql =
        s"""INSERT INTO news_items (source, external_id, title, url, body, metadata, posted_at) VALUES $values
            ON CONFLICT (source, external_id) DO NOTHING RETURNING id, title, body"""
      pool.query(sql, params*)(embedRow).flatMap { inserted =>
        if inserted.isEmpty then ZIO.succeed(SaveResult())
        else embed(inserted).map(r => SaveResult(inserted.size, r.embedded, r.failed))
      }

  // Insert-or-replace by (source, external_id); a replaced body is
  // re-embedded, so the old vector is cleared first.
  def upsert(item: NewsItem): Task[SaveResult] =
    pool
      .query(
        """INSERT INTO news_items (source, external_id, title, url, body, metadata, posted_at)
           VALUES (?, ?, ?, ?, ?, ?::jsonb, ?)
           ON CONFLICT (source, external_id) DO UPDATE SET
             title = EXCLUDED.title, body = EXCLUDED.body, metadata = EXCLUDED.metadata,
             embedding = NULL, embedded_at = NULL
           RETURNING id, title, body""",
        item.source,
        item.externalId,
        item.title,
        item.url,
        item.body,
        item.metadata.toJson,
        item.postedAt
      )(embedRow)
      .flatMap(rows => embed(rows).map(r => SaveResult(1, r.embedded, r.failed)))

  def findByExternalId(source: String, externalId: String): Task[Option[NewsItem]] =
    pool
      .query(s"SELECT $ItemColumns FROM news_items WHERE source = ? AND external_id = ? LIMIT 1", source, externalId)(
        item
      )
      .map(_.headOption)

  // The highest tg_message_id already stored for a channel.
  def channelWatermark(chatId: String): Task[Option[Long]] =
    pool
      .query(
        """SELECT max((metadata ->> 'tg_message_id')::int)::bigint AS m FROM news_items
            WHERE source = ? AND metadata ->> 'chat_id' = ?""",
        News.ChannelSource,
        chatId
      )(_.optLong("m"))
      .map(_.headOption.flatten)

  // Chronological: ascending when `since` is set, descending otherwise.
  // as_of bounds fetched_at here — list needs no embedding, so "was it in the
  // store" is the visibility question.
  def list(filter: NewsFilter, limit: Int, dedupThreshold: Double): Task[List[NewsItem]] =
    val dedup = dedupThreshold > 0
    // A 2× pool leaves room for near-duplicates to drop out.
    val fetchLimit = if dedup then (limit * 2).min(2000) else limit
    val (where, params) = filters(filter, "fetched_at")
    val order = if filter.since.isDefined then "ASC" else "DESC"
    val sql =
      s"SELECT $ItemColumns, embedding::text AS embedding FROM news_items ${whereClause(where)} ORDER BY posted_at $order LIMIT ?"
    pool
      .query(sql, (params :+ fetchLimit)*)(rs => item(rs) -> rs.optString("embedding").flatMap(Vectors.parse))
      .map(rows => Retrieval.dedupByPairwiseCosine(rows, _._2, dedupThreshold, keepNull = true).take(limit).map(_._1))

  // One batch embed for every query, then one vector search per query (each
  // with its own pool), merged and de-duplicated across the batch. A single
  // query is the N=1 case.
  def search(queries: List[String], k: Int, filter: NewsFilter, annotate: Boolean): Task[List[SearchResult]] =
    val poolSize = (k * 2).max(30)
    for
      vectors <- embedder.embedBatch(queries)
      pools <- ZIO.foreachPar(vectors) { vector =>
        // as_of bounds embedded_at here: a row embedded later wasn't
        // retrievable then, whenever it was posted.
        val (where, params) = filters(filter, "embedded_at")
        val sql =
          s"""SELECT id, source, title, url, body, metadata, posted_at,
                     (embedding <=> ?::text::vector) AS distance, embedding::text AS embedding
                FROM news_items ${whereClause("embedding IS NOT NULL" :: where)} ORDER BY distance LIMIT ?"""
        pool.query(sql, ((Vectors.literal(vector) :: params) :+ poolSize)*)(poolRow)
      }
    yield News.mergeRankedPools(pools, k, Retrieval.DefaultDedupThreshold, annotate)

  def embedMissingBatch(batch: Int): Task[EmbedResult] =
    pool.query("SELECT id, title, body FROM news_items WHERE embedding IS NULL LIMIT ?", batch)(embedRow).flatMap(embed)

object NewsRepository:
  val ItemColumns = "id, source, external_id, title, url, body, metadata, posted_at"

  final case class EmbedRow(id: Long, title: Option[String], body: String):
    def text: String = List(title.getOrElse("").trim, body.trim).filter(_.nonEmpty).mkString("\n\n")

  private def embedRow(rs: ResultSet) = EmbedRow(rs.getLong("id"), rs.optString("title"), rs.getString("body"))

  def metadataOf(rs: ResultSet): Json.Obj =
    rs.optString("metadata").flatMap(_.fromJson[Json].toOption) match
      case Some(obj: Json.Obj) => obj
      case _                   => Json.Obj()

  private def item(rs: ResultSet) = NewsItem(
    rs.getString("source"),
    rs.getString("external_id"),
    rs.optString("title"),
    rs.optString("url"),
    rs.getString("body"),
    metadataOf(rs),
    rs.optInstant("posted_at")
  )

  private def poolRow(rs: ResultSet) = News.PoolRow(
    rs.getLong("id"),
    rs.getString("source"),
    rs.optString("title"),
    rs.optString("url"),
    rs.getString("body"),
    metadataOf(rs),
    rs.optInstant("posted_at"),
    rs.getDouble("distance"),
    rs.optString("embedding").flatMap(Vectors.parse)
  )

  def whereClause(filters: List[String]): String =
    if filters.isEmpty then "" else filters.mkString("WHERE ", " AND ", "")

  private def filters(f: NewsFilter, asOfColumn: String): (List[String], List[Any]) =
    val parts = List(
      f.source.map(s => "source = ?" -> List(s)),
      f.since.map(t => "posted_at > ?" -> List(t)),
      f.until.map(t => "posted_at <= ?" -> List(t)),
      f.asOf.map(t => s"$asOfColumn <= ?" -> List(t)),
      f.channel.map(c => "(metadata ->> 'chat_username' = ? OR metadata ->> 'chat_id' = ?)" -> List(c, c))
    ).flatten
    (parts.map(_._1), parts.flatMap(_._2))

// ── 6. poller ────────────────────────────────────────────────────────────────

object NewsPoller:
  // One loop; each tick fires every provider whose cadence has elapsed. One
  // provider failing never stops the others. Starts after a boot delay so
  // transport startup finishes first.
  def run(providers: List[NewsProvider], repository: NewsRepository): UIO[Unit] =
    val names = providers.map(p => s"${p.source}(${p.cadence.toMinutes}min)").mkString(", ")
    def tickOne(p: NewsProvider) =
      p.fetch
        .flatMap { items =>
          ZIO.unless(items.isEmpty)(
            repository
              .save(items)
              .flatMap(r =>
                ZIO.logInfo(
                  s"news tick ${p.source}: fetched ${items.size}, saved ${r.saved}, embedded ${r.embedded}, failed ${r.failed}"
                )
              )
          )
        }
        .catchAll(err => ZIO.logError(s"news tick failed (${p.source}): ${Results.describe(err)}"))
    for
      _ <- ZIO.logInfo(s"news poller starting: $names")
      _ <- ZIO.sleep(10.seconds)
      last <- Ref.make(Map.empty[String, Instant])
      tick = Clock.instant.flatMap { now =>
        ZIO.foreachDiscard(providers) { p =>
          last.get
            .map(_.get(p.source).exists(at => java.time.Duration.between(at, now).compareTo(p.cadence) < 0))
            .flatMap {
              case true  => ZIO.unit
              case false => last.update(_ + (p.source -> now)) *> tickOne(p)
            }
        }
      }
      _ <- tick.repeat(Schedule.spaced(30.seconds))
    yield ()

// ── 7. tools ─────────────────────────────────────────────────────────────────

enum KnownSource derives JsonDecoder:
  case hackernews, habr, channel

object KnownSource:
  given Schema[KnownSource] = Schema.derivedEnumeration[KnownSource].defaultStringBased

object NewsTools:
  import Tools.*

  private val ChunksDoc =
    "Map-reduce mode: split the result into EXACTLY this many contiguous chunks and return { count, chunks: [...] } " +
      "instead of a flat list — one chunk per parallel map step. The chunk count is fixed so a workflow can reference " +
      "${bind.chunks.0}, ${bind.chunks.1}, … statically; trailing chunks may be empty when there are few items."

  final case class SearchParams(
      @description("Natural-language search query. Use for a single facet.") query: Option[String],
      @description("Batch of 1–8 independent queries for a multi-facet ask. Mutually exclusive with `query`.")
      queries: Option[List[String]],
      @description("Number of results to return. Default 10.") @validate(Validator.inRange(1, 50)) k: Option[Int],
      @description("Restrict results to one source.") source: Option[KnownSource],
      @description("Only items with posted_at > this ISO timestamp.") sinceISO: Option[String],
      @description("Only items with posted_at <= this ISO timestamp.") untilISO: Option[String],
      @description(
        "Eval/judge point-in-time replay: only items already searchable at this ISO instant (embedded_at <= asOfISO). " +
          "Reconstructs what search could have returned at a past moment, excluding rows embedded later (poller/backfill " +
          "lag). Normal runs omit this."
      ) asOfISO: Option[String],
      @description("For source='channel' only: restrict to one Telegram channel by chat_username or chat_id.")
      channel: Option[String],
      @description(ChunksDoc) @validate(Validator.inRange(2, 8)) chunks: Option[Int]
  ) derives JsonDecoder,
        Schema

  final case class ListParams(
      @description("Restrict to one source.") source: Option[KnownSource],
      @description("Only items with posted_at > this ISO timestamp. Typical use: now - 24h for a daily digest.")
      sinceISO: Option[String],
      @description("Only items with posted_at <= this ISO timestamp.") untilISO: Option[String],
      @description(
        "Eval/judge point-in-time replay: only items already in the store at this ISO instant (fetched_at <= asOfISO). " +
          "Reconstructs what the store held at a past moment, excluding rows fetched later (poller lag). Normal runs " +
          "omit this."
      ) asOfISO: Option[String],
      @description("For source='channel' only: restrict to one Telegram channel by chat_username or chat_id.")
      channel: Option[String],
      @description("Max rows. Default 500.") @validate(Validator.inRange(1, 2000)) limit: Option[Int],
      @description(ChunksDoc) @validate(Validator.inRange(2, 8)) chunks: Option[Int]
  ) derives JsonDecoder,
        Schema

  final case class FetchArticleParams(@description("Article URL to fetch and extract.") url: String)
      derives JsonDecoder,
        Schema

  def buildFilter(
      source: Option[KnownSource],
      since: Option[String],
      until: Option[String],
      asOf: Option[String],
      channel: Option[String]
  ): Either[String, NewsFilter] =
    def date(field: String, v: Option[String]) = v match
      case Some(raw) => Time.requireJsDate(field, raw).map(Some(_))
      case None      => Right(None)
    for
      s <- date("sinceISO", since)
      u <- date("untilISO", until)
      a <- date("asOfISO", asOf)
    yield NewsFilter(source.map(_.toString), s, u, a, channel)

  private def chunksOk(chunks: Option[Int]) = chunks.forall(c => c >= 2 && c <= 8)

  def sourceForUrl(url: String): String =
    scala.util.Try(URI(url).getHost).toOption.flatMap(Option(_)) match
      case Some(host) if host.endsWith("habr.com")        => "habr"
      case Some(host) if host.endsWith("ycombinator.com") => "hackernews"
      case _                                              => "external"

  private def counted(items: List[Json], key: String, chunks: Option[Int]): Json =
    chunks match
      case Some(n) =>
        Json.Obj(
          "count" -> Json.Num(items.size),
          "chunks" -> Json.Arr(News.splitChunks(items, n).map(c => Json.Arr(c*))*)
        )
      case None => Json.Obj("count" -> Json.Num(items.size), key -> Json.Arr(items*))

  val tools: List[ToolDef] = List(
    tool(
      "search_news",
      "Semantic search over the news store",
      "Vector search across every news item the pollers have ingested (Hacker News, Habr, harvested Telegram " +
        "channels). Returns the closest matches by semantic similarity. Use this when the user asks about a topic — " +
        "the background pollers keep the store fresh, so there is no need to fetch articles before searching. Returns " +
        "id, source, title, url, snippet (first ~400 chars of the body), posted_at, distance (lower = closer), and " +
        "source-specific metadata.\n\nFor a multi-facet ask (e.g. one topic spanning several distinct subjects), pass " +
        "`queries: [...]` — one entry per facet — instead of calling this tool N times or blurring everything into one " +
        "`query`. Each query is searched independently; results are merged and already de-duplicated across the batch " +
        "(an item's `distance` is its best match across the facets, `matchedQueries` lists which facets surfaced it), " +
        "so do NOT re-query per facet or re-dedup. Pass exactly one of `query` or `queries`."
    ) { (deps, p: SearchParams) =>
      val bad = p.query.exists(_.isEmpty) || p.queries.exists(q => q.isEmpty || q.size > 8 || q.exists(_.isEmpty)) ||
        p.k.exists(k => k < 1 || k > 50) || !chunksOk(p.chunks)
      ZIO.when(bad)(invalid("query must be non-empty, queries 1–8 non-empty strings, k 1–50, chunks 2–8")) *>
        ((p.query, p.queries) match
          case (Some(q), None)  => ZIO.succeed(Some(List(q) -> false))
          case (None, Some(qs)) => ZIO.succeed(Some(qs -> true))
          case _                => ZIO.none
        ).flatMap {
          case None => ZIO.succeed(Json.Obj("error" -> Json.Str("Pass exactly one of `query` or `queries`.")))
          case Some((queries, annotate)) =>
            for
              filter <- orFail(buildFilter(p.source, p.sinceISO, p.untilISO, p.asOfISO, p.channel))
              results <- deps.news.search(queries, p.k.getOrElse(10), filter, annotate)
            yield counted(results.map(_.toJsonAST.toOption.get), "results", p.chunks)
        }
    },
    tool(
      "list_news",
      "List news items chronologically",
      "Read items from the news store ordered by posted_at. Use when you need everything in a time window (e.g. a 24h " +
        "channel digest) rather than a topical match. Ascending when sinceISO is provided, descending otherwise."
    ) { (deps, p: ListParams) =>
      for
        _ <- ZIO.when(p.limit.exists(l => l < 1 || l > 2000) || !chunksOk(p.chunks))(
          invalid("limit must be 1–2000, chunks 2–8")
        )
        filter <- orFail(buildFilter(p.source, p.sinceISO, p.untilISO, p.asOfISO, p.channel))
        items <- deps.news.list(filter, p.limit.getOrElse(500), Retrieval.DefaultDedupThreshold)
      yield counted(items.map(_.toJson), "items", p.chunks)
    },
    tool(
      "fetch_article",
      "Fetch and store an arbitrary article URL",
      "Download a web article (Mozilla Readability) and save it to the news store so it becomes searchable. Manual " +
        "override — the HN and Habr pollers already cover their feeds. Use this for ad-hoc URLs the user shares. " +
        "Returns clean plaintext (title + body). If the URL is already cached, returns the cached row without " +
        "re-fetching."
    ) { (deps, p: FetchArticleParams) =>
      val valid = scala.util.Try(URI(p.url)).toOption.exists(u => u.isAbsolute && u.getScheme != null)
      ZIO.when(!valid)(invalid("url must be a valid URL")) *>
        ZIO
          .foreach(List("hackernews", "habr", "external"))(s => deps.news.findByExternalId(s, p.url))
          .map(_.flatten.find(_.body.trim.nonEmpty))
          .flatMap {
            case Some(cached) =>
              ZIO.succeed(
                Json.Obj(
                  (List(
                    "url" -> Json.Str(p.url),
                    "title" -> Json.Str(cached.title.getOrElse("")),
                    "text" -> Json.Str(cached.body),
                    "source" -> Json.Str(cached.source),
                    "sizeChars" -> Json.Num(cached.body.length),
                    "cached" -> Json.Bool(true)
                  ) ++ cached.postedAt.map(t => "publishedAt" -> Json.Str(Time.iso(t))))*
                )
              )
            case None => fetchAndStore(deps, p.url)
          }
    }
  )

  private def fetchAndStore(deps: Deps, url: String): Task[Json] =
    for
      article <- ArticleFetcher(deps.fetcher.http).fetch(url)
      source = sourceForUrl(url)
      metadata = Json.Obj(
        (article.author.map(a => "author" -> Json.Str(a)).toList ++ article.site.map(s => "site" -> Json.Str(s)))*
      )
      item = NewsItem(
        source,
        url,
        Some(article.title).filter(_.nonEmpty),
        Some(url),
        article.text,
        metadata,
        article.publishedAt
      )
      _ <- deps.news
        .upsert(item)
        .catchAll(err => ZIO.logError(s"fetch_article: save step failed: ${Results.describe(err)}"))
    yield Json.Obj(
      (List("url" -> Json.Str(article.url), "title" -> Json.Str(article.title), "text" -> Json.Str(article.text)) ++
        article.site.map(s => "site" -> Json.Str(s)) ++
        article.publishedAt.map(t => "publishedAt" -> Json.Str(Time.iso(t))) ++
        article.author.map(a => "author" -> Json.Str(a)) ++
        List("source" -> Json.Str(source), "sizeChars" -> Json.Num(article.text.length), "cached" -> Json.Bool(false)))*
    )
