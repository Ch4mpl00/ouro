package mcp

import java.net.InetAddress
import java.nio.file.{Files, Path}

import zio.*
import zio.json.*
import zio.json.ast.Json
import zio.test.*
import zio.test.TestAspect.*

// The integration domains' pure parts: Gmail MIME walking, Monobank amounts,
// the SSRF guard, embeddings math, the skills export, gateway config, the
// userbot session format — plus the knowledge base against Postgres.
object DomainSpec extends ZIOSpecDefault:
  private val message = """{
    "id": "m1", "threadId": "t1", "snippet": "Квитанція", "internalDate": "1780000000000",
    "payload": {
      "mimeType": "multipart/mixed",
      "headers": [{ "name": "subject", "value": "Квитанція за квітень" }, { "name": "From", "value": "nashdom@x" }],
      "parts": [
        { "mimeType": "text/plain", "body": { "size": 10 } },
        { "mimeType": "application/pdf", "filename": "bill.pdf", "body": { "attachmentId": "ANGjdJ_9-x", "size": 2048 } },
        { "mimeType": "multipart/related", "parts": [
          { "mimeType": "image/png", "filename": "", "body": { "attachmentId": "img", "size": 5 } }
        ] }
      ]
    }
  }""".fromJson[RawMessage].toOption.get

  private def priv(ip: String) = Ssrf.isPrivate(InetAddress.getByName(ip))

  private def tempDirs: ZIO[Scope, Throwable, (Path, Path)] =
    ZIO.acquireRelease(ZIO.attemptBlocking {
      val root = Files.createTempDirectory("mcp-skills")
      (Files.createDirectories(root.resolve("skills")), Files.createDirectories(root.resolve("skills.default")))
    })((live, _) => ZIO.attemptBlocking(scala.reflect.io.Directory(live.getParent.toFile).deleteRecursively()).orDie)

  private def write(dir: Path, name: String, body: String) = ZIO.attemptBlocking(Files.writeString(dir.resolve(name), body))

  def spec = suite("domains")(
    suite("gmail")(
      test("walks the MIME tree for attachments") {
        val found = GmailModule.findAttachments(message)
        assertTrue(
          found.size == 2,
          found.head.filename == "bill.pdf",
          found(1).filename == "untitled",
          found.count(GmailModule.isPdf) == 1,
        )
      },
      test("headers match case-insensitively") {
        val summary = message.summary.toOption.get
        assertTrue(summary.subject.contains("Квитанція за квітень"), summary.to.isEmpty)
      },
      test("the bill signal lists the PDF and the steps") {
        val content = GmailModule.nashdomContent(message.summary.toOption.get, GmailModule.findAttachments(message))
        assertTrue(
          content.startsWith("Пришла новая квитанция NashDom."),
          content.contains("  - attachmentId: ANGjdJ_9-x\n    filename: bill.pdf\n    mimeType: application/pdf\n    sizeBytes: 2048"),
          content.contains("Date: 1780000000000"),
          !content.contains("img"),
        )
      },
      test("attachment paths are sanitised and prefixed") {
        val path = GmailModule.attachmentPath(Path.of("storage"), "me@x.com", "m1", "ANGjdJ_9-x/more", Some("../evil\n.pdf"))
        assertTrue(path == Path.of("storage/gmail/me@x.com/m1/ANGjdJ9xmore_.._evil_.pdf"))
      },
      test("decodes padded and unpadded base64url") {
        val d = java.util.Base64.getUrlDecoder
        assertTrue(String(d.decode("aGk")) == "hi", String(d.decode("aGk=")) == "hi")
      },
    ),
    suite("monobank")(
      test("normalises minor units and currency") {
        val raw = """{ "id": "x", "time": 1780000000, "description": "Сільпо", "mcc": 5411, "originalMcc": 5411,
          "hold": false, "amount": -12550, "operationAmount": -10000, "currencyCode": 980,
          "commissionRate": 0, "cashbackAmount": 0, "balance": 100000 }""".fromJson[RawItem].toOption.get
        val tx = Transaction.of(raw).toJson
        assertTrue(
          tx.contains("\"amount\":-125.5"),
          // 100.0 must print as 100, the way JSON.stringify did.
          tx.contains("\"operationAmount\":-100,"),
          tx.contains("\"currency\":\"UAH\""),
          tx.contains("\"comment\":null"),
          Monobank.isoCurrency(999) == "999",
        )
      }
    ),
    suite("fetch")(
      test("flags loopback, RFC1918, link-local and metadata") {
        val v4 = List("127.0.0.1", "10.0.0.5", "192.168.1.1", "172.16.0.1", "172.31.255.255", "169.254.169.254", "0.0.0.0")
        val v6 = List("::1", "fe80::1", "fc00::1", "fd12:3456::1", "::ffff:10.0.0.1")
        assertTrue((v4 ++ v6).forall(priv))
      },
      test("allows public addresses") {
        assertTrue(List("8.8.8.8", "1.1.1.1", "172.15.0.1", "172.32.0.1", "2606:4700::1").forall(!priv(_)))
      },
      test("rejects bad schemes, private hosts and garbage") {
        def msg(url: String) = Ssrf.assertPublicUrl(url).flip.map(_.getMessage)
        for
          file <- msg("file:///etc/passwd")
          ftp <- msg("ftp://example.com")
          meta <- msg("http://169.254.169.254/latest/meta-data/")
          loop <- msg("http://127.0.0.1:8080/")
          local <- msg("http://localhost/admin")
          junk <- msg("not a url")
        yield assertTrue(
          file.contains("scheme"),
          ftp.contains("scheme"),
          meta.contains("private/loopback"),
          loop.contains("private/loopback"),
          local.contains("private/loopback"),
          junk.contains("invalid URL"),
        )
      },
      test("binary content types match like the TS regex") {
        assertTrue(
          Fetcher.isBinary("application/pdf"),
          Fetcher.isBinary("IMAGE/png"),
          Fetcher.isBinary("application/x-tar"),
          !Fetcher.isBinary("text/html; charset=utf-8"),
          !Fetcher.isBinary("application/json"),
        )
      },
    ),
    suite("embeddings")(
      test("drops exact and near duplicates keeping input order") {
        val t = math.Pi.toFloat / 12
        val items = List(
          1 -> Vector(1f, 0f, 0f),
          2 -> Vector(math.cos(t).toFloat, math.sin(t).toFloat, 0f),
          3 -> Vector(math.cos(2 * t).toFloat, math.sin(2 * t).toFloat, 0f),
          4 -> Vector(1f, 0f, 0f),
        )
        // 15° apart collapses, 30° apart survives — a transitive chain keeps
        // its endpoints.
        assertTrue(Retrieval.dedupByPairwiseCosine(items, x => Some(x._2), 0.05, keepNull = false).map(_._1) == List(1, 3))
      },
      test("null vectors are dropped or kept in place") {
        val items = List(1 -> Some(Vector(1f, 0f)), 2 -> None, 3 -> Some(Vector(1f, 0f)), 4 -> Some(Vector(0f, 1f)))
        assertTrue(
          Retrieval.dedupByPairwiseCosine(items, _._2, 0.05, keepNull = false).map(_._1) == List(1, 4),
          Retrieval.dedupByPairwiseCosine(items, _._2, 0.05, keepNull = true).map(_._1) == List(1, 2, 4),
          Retrieval.dedupByPairwiseCosine(items, _._2, 0.0, keepNull = false).map(_._1) == List(1, 2, 3, 4),
        )
      },
      test("truncates on characters, not code units") {
        assertTrue(truncateChars("привет", 3) == "при", truncateChars("hi", 10) == "hi", truncateChars("a😀b", 2) == "a😀")
      },
    ),
    suite("skills")(
      test("lists the live overlay over defaults with metadata") {
        ZIO.scoped {
          for
            (live, defaults) <- tempDirs
            _ <- write(defaults, "alpha.md", "---\ntools: [search_news]\n---\n\n# Default Alpha\n\nDefault instructions.\n")
            _ <- write(defaults, "beta.md", "---\ntools: *\n---\n\n# Beta title\n\nBeta summary for selection.\n")
            _ <- write(live, "alpha.md", "---\ntools: []\n---\n\n# Live Alpha\n\nActive overlay instructions.\n")
            _ <- write(live, "alpha.patch.md", "not a standalone skill\n")
            skills <- SkillCatalog(live, defaults).listSkills
          yield assertTrue(
            skills.map(_.fileName) == List("alpha.md", "beta.md"),
            skills.head.title == "Live Alpha",
            skills.head.description == "Active overlay instructions.",
            skills.head.tools.contains(SkillTools.Only(Nil)),
            skills.head.source == SkillSource.live,
            skills.head.patched,
            skills(1).tools.contains(SkillTools.All),
            skills(1).source == SkillSource.default,
            !skills(1).patched,
            skills(1).tools.toJson == "\"*\"",
          )
        }
      },
      test("composes the patch exactly as the agent does") {
        ZIO.scoped {
          val raw = "---\ntools: []\n---\n\n# Patched\n\nBase instructions.\n"
          for
            (live, defaults) <- tempDirs
            _ <- write(defaults, "patched.md", raw)
            _ <- write(live, "patched.patch.md", "Lesson learned.\n")
            skill <- SkillCatalog(live, defaults).readSkill("patched.md").someOrFailException
          yield assertTrue(
            skill.content == raw,
            skill.patch.contains("Lesson learned.\n"),
            // Must stay in step with the agent's appendPatch.
            skill.effectiveInstructions == "# Patched\n\nBase instructions.\n\n<!-- improver-patch -->\nLesson learned.\n",
          )
        }
      },
      test("ignores patches in defaults and rejects paths") {
        ZIO.scoped {
          for
            (live, defaults) <- tempDirs
            _ <- write(defaults, "plain.md", "---\ntools: []\n---\n\n# Plain\n\nBase only.\n")
            _ <- write(defaults, "plain.patch.md", "must be ignored\n")
            catalog = SkillCatalog(live, defaults)
            skill <- catalog.readSkill("plain.md").someOrFailException
            missing <- catalog.readSkill("missing.md")
            bad <- ZIO.foreach(List("plain", "../plain.md"))(catalog.readSkill(_).flip.map(_.getMessage))
          yield assertTrue(
            skill.patch.isEmpty,
            !skill.summary.patched,
            skill.effectiveInstructions == "# Plain\n\nBase only.\n",
            missing.isEmpty,
            bad.forall(_.contains("exact fileName")),
          )
        }
      },
      test("descriptions skip lists and code, and truncate") {
        val raw = "# T\n\n- a list\n\n```\ncode\n```\n\nThe `real` [summary](http://x) *here*.\n"
        assertTrue(
          SkillCatalog.extractDescription(raw, "T") == "The real summary here.",
          SkillCatalog.extractDescription(s"# T\n\n${"w " * 200}", "T").length == 280,
          SkillCatalog.fallbackTitle("news-digest_v2") == "News Digest V2",
        )
      },
    ),
    suite("gateway")(
      test("resolves secrets and defaults the prefix") {
        ZIO.scoped {
          for
            path <- ZIO.acquireRelease(ZIO.attemptBlocking(Files.createTempFile("gateway", ".json")))(p => ZIO.attemptBlocking(Files.delete(p)).orDie)
            _ <- write(path.getParent, path.getFileName.toString,
              """{ "upstreams": [
                  { "name": "tavily", "url": "https://mcp.tavily.com/mcp/?tavilyApiKey=${TAVILY_API_KEY}" },
                  { "name": "off", "url": "https://x", "enabled": false },
                  { "name": "nokey", "url": "https://y", "headers": { "Authorization": "Bearer ${MISSING}" } }
              ] }""")
            upstreams <- GatewayConfig.load(path, name => Option.when(name == "TAVILY_API_KEY")("secret"))
          yield assertTrue(upstreams == List(Upstream("tavily", "https://mcp.tavily.com/mcp/?tavilyApiKey=secret", "tavily", Nil)))
        }
      },
      test("a missing file means no upstreams; bad names fail") {
        ZIO.scoped {
          for
            none <- GatewayConfig.load(Path.of("/nonexistent/gateway.json"), _ => None)
            path <- ZIO.acquireRelease(ZIO.attemptBlocking(Files.createTempFile("gateway", ".json")))(p => ZIO.attemptBlocking(Files.delete(p)).orDie)
            _ <- write(path.getParent, path.getFileName.toString, """{ "upstreams": [{ "name": "bad name", "url": "https://x" }] }""")
            bad <- GatewayConfig.load(path, _ => None).either
          yield assertTrue(none.isEmpty, bad.isLeft)
        }
      },
      test("exposed names follow the OpenAI tool-name rule") {
        assertTrue(
          Gateway.exposedNameOk("tavily__tavily_search"),
          !Gateway.exposedNameOk("tavily__search.v2"),
          !Gateway.exposedNameOk("x" * 65),
        )
      },
      test("reads a JSON-RPC result out of an SSE reply") {
        val sse = HttpReply(200, "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n",
          zio.http.Headers(zio.http.Header.Custom("content-type", "text/event-stream")))
        val err = HttpReply(200, """{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"nope"}}""", zio.http.Headers.empty)
        assertTrue(
          RemoteClient.rpcResult(sse) == Right(Json.Obj("tools" -> Json.Arr())),
          RemoteClient.rpcResult(err) == Left("nope"),
        )
      },
    ),
    suite("userbot")(
      test("string session round-trips the gramjs layout") {
        val key = Array.tabulate[Byte](256)(_.toByte)
        val session = StringSession(2, "149.154.167.41", 443, key)
        val encoded = session.encode
        assertTrue(
          encoded.startsWith("1"),
          // 1 + 2 + len("149.154.167.41") + 2 + 256 bytes, as gramjs wrote it.
          java.util.Base64.getDecoder.decode(encoded.drop(1)).length == 275,
          StringSession.decode(encoded) == Right(session),
        )
      },
      test("rejects foreign session strings") {
        assertTrue(StringSession.decode("2abc").isLeft, StringSession.decode("1AAAA").isLeft)
      },
      test("normalises channel handles") {
        assertTrue(List("tginsider", "@tginsider", "https://t.me/tginsider/", " @@tginsider ").forall(Userbot.normalizeHandle(_) == "tginsider"))
      },
    ),
    suite("knowledge")(
      test("normalises tags without imposing a scheme") {
        assertTrue(
          KnowledgeRepository.normalizeTags(Some(List(" роутер ", "", "WiFi", "роутер"))) == List("роутер", "WiFi"),
          KnowledgeRepository.normalizeTags(None).isEmpty,
        )
      },
      test("adds and finds notes with a tag filter (Postgres)") {
        ZIO.scoped {
          for
            pool <- TestPg.newsPool
            repo = KnowledgeRepository(pool, FakeEmbedder(1536))
            tag <- Random.nextInt.map(n => s"t${n.abs}")
            added <- repo.addNote("пароль от роутера на наклейке снизу", Some(List(tag, tag)), Some("telegram"))
            hits <- repo.findNotes("пароль роутера", 5, Some(List(tag)))
            none <- repo.findNotes("пароль роутера", 5, Some(List("no-such-tag-xyz")))
          yield assertTrue(
            added.embedded,
            added.tags == List(tag),
            hits.size == 1,
            hits.head.body == "пароль от роутера на наклейке снизу",
            none.isEmpty,
          )
        }
      },
    ),
  ) @@ withLiveClock @@ withLiveRandom
