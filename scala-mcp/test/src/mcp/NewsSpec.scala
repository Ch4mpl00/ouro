package mcp

import java.time.Instant

import zio.*
import zio.json.*
import zio.json.ast.Json
import zio.test.*
import zio.test.TestAspect.*

object NewsSpec extends ZIOSpecDefault:
  private def unit(xs: Float*): Vector[Float] =
    val n = math.sqrt(xs.map(x => x * x).sum).toFloat
    xs.map(_ / n).toVector

  private def row(id: Long, distance: Double, embedding: Vector[Float]) =
    News.PoolRow(id, "hackernews", Some(s"t$id"), None, "body", Json.Obj(), None, distance, Some(embedding))

  private val T = Retrieval.DefaultDedupThreshold

  private def headline(extra: List[(String, Json)]) =
    News.Headline("Headline title", "https://example.com/a", Some("alice"), Time.parseJsDate("2026-01-01T00:00:00Z"), extra)

  private def article(title: String, text: String) =
    Article("https://example.com/a", title, text, Some("example.com"), Time.parseJsDate("2026-01-02T00:00:00Z"), Some("bob"))

  private def item(source: String, id: String, title: String, body: String, posted: String) =
    NewsItem(source, id, Some(title), Some(s"https://example.com/$id"), body, Json.Obj(), Time.parseJsDate(posted))

  def spec = suite("news")(
    test("keeps a multiply retrieved item once at its min distance") {
      val out = News.mergeRankedPools(
        List(List(row(1, 0.2, unit(1, 0, 0)), row(2, 0.5, unit(0, 1, 0))), List(row(1, 0.1, unit(1, 0, 0)), row(3, 0.4, unit(0, 0, 1)))),
        10,
        T,
        annotate = true,
      )
      assertTrue(out.map(_.id) == List(1L, 3L, 2L), math.abs(out.head.distance - 0.1) < 1e-9, out.head.matchedQueries.contains(List(0, 1)))
    },
    test("collapses cross-query near duplicates keeping the closer one") {
      val pools = List(List(row(1, 0.2, unit(1, 0, 0))), List(row(2, 0.15, unit(1, 0.1f, 0))))
      assertTrue(
        News.mergeRankedPools(pools, 10, T, annotate = true).map(_.id) == List(2L),
        // Threshold 0 disables dedup: both survive.
        News.mergeRankedPools(pools, 10, 0.0, annotate = true).map(_.id) == List(2L, 1L),
      )
    },
    test("k caps after dedup; a single query omits matches") {
      val out = News.mergeRankedPools(
        List(
          List(row(1, 0.5, unit(1, 0, 0)), row(2, 0.1, unit(0, 1, 0))),
          List(row(3, 0.3, unit(0, 0, 1)), row(4, 0.2, unit(1, 1, 0)), row(5, 0.9, unit(0, 1, 1))),
        ),
        2,
        T,
        annotate = true,
      )
      val single = News.mergeRankedPools(List(List(row(1, 0.3, unit(1, 0)), row(2, 0.1, unit(0, 1)))), 10, 0.03, annotate = false)
      assertTrue(out.map(_.id) == List(2L, 4L), single.map(_.id) == List(2L, 1L), !single.head.toJson.contains("matchedQueries"))
    },
    test("snippets cut at 400 characters") {
      val long = News.snippet("я" * 401)
      assertTrue(long.codePointCount(0, long.length) == 401, long.endsWith("…"), News.snippet("short") == "short")
    },
    test("splits into exactly n contiguous chunks") {
      assertTrue(
        News.splitChunks((0 until 10).toList, 3) == List(List(0, 1, 2, 3), List(4, 5, 6), List(7, 8, 9)),
        News.splitChunks(List(1, 2), 4) == List(List(1), List(2), Nil, Nil),
        News.splitChunks(List.empty[Int], 3) == List(Nil, Nil, Nil),
      )
    },
    test("maps an article with the headline's author winning") {
      val mapped = News.articleItem("hackernews", headline(List("hn_id" -> Json.Num(1))), Some(article("Article title", "body content"))).get
      val fallback = News.articleItem("habr", headline(Nil), Some(article("", "b"))).get
      assertTrue(
        mapped.title.contains("Article title"),
        mapped.metadata == Json.Obj("hn_id" -> Json.Num(1), "author" -> Json.Str("alice"), "site" -> Json.Str("example.com")),
        mapped.postedAt == Time.parseJsDate("2026-01-02T00:00:00Z"),
        // An empty extracted title falls back to the headline's.
        fallback.title.contains("Headline title"),
      )
    },
    test("drops failed or blank extractions") {
      assertTrue(
        News.articleItem("habr", headline(Nil), None).isEmpty,
        News.articleItem("habr", headline(Nil), Some(article("t", "   \n "))).isEmpty,
      )
    },
    test("channel posts have no title and link when public") {
      val m = ChannelMessage(7, Instant.parse("2026-01-01T00:00:00Z"), "body text", Some(123), Some(4))
      val item = News.channelItem("100", Some("Channel One"), Some("channel_one"), m)
      assertTrue(
        item.externalId == "100:7",
        item.title.isEmpty,
        item.url.contains("https://t.me/channel_one/7"),
        item.metadata.toJson ==
          """{"chat_id":"100","chat_title":"Channel One","chat_username":"channel_one","tg_message_id":7,"views":123,"forwards":4}""",
        News.channelItem("100", None, None, m).url.isEmpty,
      )
    },
    test("strips markup and entities to compact text") {
      assertTrue(
        News.stripHtml("<p>Hello&nbsp;<b>world</b></p><script>x()</script>\n<style>.a{}</style> &amp; more") == "Hello world & more"
      )
    },
    test("extracts a readable article") {
      val body = "Графы — это набор вершин и рёбер. " * 40
      val html =
        s"""<html><head><title>Про графы</title><meta property="og:site_name" content="Хабр">
           |<meta property="article:published_time" content="2026-01-02T00:00:00Z"></head>
           |<body><nav>menu</nav><article><h1>Про графы</h1><p>$body</p><p>$body</p></article></body></html>""".stripMargin
      val a = News.extractArticle("https://habr.com/ru/articles/1/", html).toOption.get
      assertTrue(a.text.contains("набор вершин"), a.site.contains("Хабр"), a.publishedAt == Time.parseJsDate("2026-01-02T00:00:00Z"))
    },
    test("parses the Habr RSS shape") {
      val rss =
        """<?xml version="1.0"?><rss version="2.0"><channel><title>Habr</title>
          |<item><title>Про графы</title><link>https://habr.com/ru/articles/1/</link><dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/">alice</dc:creator>
          |<pubDate>Fri, 02 Oct 2026 09:00:00 GMT</pubDate></item>
          |<item><title></title><link>https://habr.com/ru/articles/2/</link></item>
          |</channel></rss>""".stripMargin
      val headlines = Habr(null).headlines(rss.getBytes("UTF-8"))
      assertTrue(
        headlines.map(_.url) == List("https://habr.com/ru/articles/1/"),
        headlines.head.author.contains("alice"),
        headlines.head.postedAt == Time.parseJsDate("2026-10-02T09:00:00Z"),
      )
    },
    test("classifies URLs by host") {
      assertTrue(
        NewsTools.sourceForUrl("https://habr.com/ru/articles/1/") == "habr",
        NewsTools.sourceForUrl("https://news.ycombinator.com/item?id=1") == "hackernews",
        NewsTools.sourceForUrl("https://example.com") == "external",
      )
    },
    // ── against Postgres ─────────────────────────────────────────────────────
    test("saves, lists and searches (Postgres)") {
      ZIO.scoped {
        for
          pool <- TestPg.newsPool
          embedder = FakeEmbedder(1536)
          repo = NewsRepository(pool, embedder)
          source <- Random.nextInt.map(n => s"test-${n.abs}")
          items = List(
            item(source, "a", "Rust async runtimes", "tokio executor scheduling futures", "2026-01-01T00:00:00Z"),
            item(source, "b", "Postgres vector search", "pgvector cosine distance ivfflat", "2026-01-02T00:00:00Z"),
            item(source, "c", "Одесса новости", "удары дронов по Одессе ночью", "2026-01-03T00:00:00Z"),
          )
          first <- repo.save(items)
          // Re-saving the same natural keys is a no-op.
          again <- repo.save(items)
          filter = NewsFilter(source = Some(source))
          listed <- repo.list(filter, 10, 0.03)
          ascending <- repo.list(filter.copy(since = Time.parseJsDate("2026-01-01T12:00:00Z")), 10, 0.03)
          hits <- repo.search(List("pgvector cosine distance"), 2, filter, annotate = false)
          multi <- repo.search(List("tokio futures", "Одесса дроны"), 5, filter, annotate = true)
          // Upsert replaces the body and re-embeds it.
          changed = items.head.copy(body = "completely new body about borrow checker")
          upserted <- repo.upsert(changed)
          found <- repo.findByExternalId(source, "a")
          // A failed inline embed leaves a NULL vector for the backfill.
          _ <- ZIO.succeed(embedder.down.set(true))
          late <- repo.save(List(item(source, "d", "late", "embedder was down", "2026-01-04T00:00:00Z")))
          _ <- ZIO.succeed(embedder.down.set(false))
          drained <- repo.embedMissingBatch(1000)
          lateHit <- repo.search(List("embedder was down"), 1, filter, annotate = false)
        yield assertTrue(
          first == SaveResult(3, 3, 0),
          again.saved == 0,
          listed.map(_.externalId) == List("c", "b", "a"),
          ascending.map(_.externalId) == List("b", "c"),
          hits.head.url.contains("https://example.com/b"),
          hits.head.matchedQueries.isEmpty,
          multi.size == 3,
          multi.forall(_.matchedQueries.isDefined),
          upserted.embedded == 1,
          found.map(_.body).contains(changed.body),
          late == SaveResult(1, 0, 1),
          drained.embedded >= 1,
          lateHit.head.url.contains("https://example.com/d"),
        )
      }
    },
    test("channel posts filter and watermark (Postgres)") {
      ZIO.scoped {
        for
          pool <- TestPg.newsPool
          repo = NewsRepository(pool, FakeEmbedder(1536))
          chat <- Random.nextInt.map(_.abs.toString)
          now <- Clock.instant
          before <- repo.channelWatermark(chat)
          _ <- repo.save(
            List(
              News.channelItem(chat, Some("Chan"), Some("chan_user"), ChannelMessage(7, now, "first post", Some(1), None)),
              News.channelItem(chat, Some("Chan"), None, ChannelMessage(12, now, "second post", Some(1), None)),
            )
          )
          after <- repo.channelWatermark(chat)
          byChat <- repo.list(NewsFilter(source = Some("channel"), channel = Some(chat)), 10, 0.0)
        yield assertTrue(before.isEmpty, after.contains(12L), byChat.size == 2)
      }
    },
    test("migrations adopt a drizzle-migrated database and re-run as no-ops (Postgres)") {
      ZIO.scoped {
        for
          pool <- TestPg.newsPool
          applied <- Migrations.run(pool.ds, "db/news", None)
          history <- pool.query("SELECT version FROM flyway_schema_history WHERE success ORDER BY installed_rank")(_.getString("version"))
        yield assertTrue(applied == 0, history == List("1", "2", "3", "4", "5"))
      }
    },
    test("adopts a database the drizzle/Rust migrator owns (Postgres)") {
      // drizzle's journal present, Flyway's history absent → baselined at the
      // journal's length; nothing already applied runs twice.
      ZIO.scoped {
        for
          base <- TestPg.baseUrl
          admin <- TestPg.newsPool
          name <- Random.nextInt.map(n => s"adopt_${n.abs}")
          _ <- admin.update(s"CREATE DATABASE $name")
          pool <- PgPool.make(base.withDatabase(name), maxSize = 2)
          _ <- pool.update("CREATE SCHEMA drizzle")
          _ <- pool.update("CREATE TABLE drizzle.__drizzle_migrations (id serial PRIMARY KEY, hash text NOT NULL, created_at bigint)")
          _ <- pool.update("INSERT INTO drizzle.__drizzle_migrations (hash, created_at) SELECT 'h' || n, n FROM generate_series(1, 5) n")
          _ <- pool.update("CREATE TABLE news_items (id bigserial PRIMARY KEY)")
          _ <- Migrations.migrateNews(pool)
          history <- pool.query("SELECT version, type FROM flyway_schema_history ORDER BY installed_rank")(rs =>
            rs.getString("version") -> rs.getString("type")
          )
        yield assertTrue(history == List("5" -> "BASELINE"))
      }
    },
  ) @@ withLiveClock @@ withLiveRandom
