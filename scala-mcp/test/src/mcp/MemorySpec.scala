package mcp

import java.time.Instant

import zio.*
import zio.json.ast.Json
import zio.test.*
import zio.test.TestAspect.*

object MemorySpec extends ZIOSpecDefault:
  import Patching.*
  import Projection.*

  private val Roadmap = "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n"
  private val Actor = "supervisor"

  private def edit(old: String, `new`: String) = Edit(old, `new`)

  final private case class Harness(service: MemoryService, store: InMemoryStore, embedder: FakeEmbedder)

  private def harness: UIO[Harness] =
    for
      store <- InMemoryStore.make
      seq <- Ref.make(0)
      embedder = FakeEmbedder()
      ids = seq.updateAndGet(_ + 1).map(n => f"pa:$n%04x")
    yield Harness(MemoryService(store, Indexer(store, embedder), ids), store, embedder)

  // The expected failure's code and details.
  private def code[A](task: Task[A]): Task[(String, Json.Obj)] =
    task.foldZIO(
      {
        case e: MemoryError => ZIO.succeed(e.code -> e.details)
        case other => ZIO.fail(RuntimeException(s"not a MemoryError: $other"))
      },
      ok => ZIO.fail(RuntimeException(s"expected a MemoryError, got $ok")),
    )

  private def write(project: String, doc: String, body: String) = WriteDoc(project, doc, body, actor = Actor)

  private def hit(id: Long, distance: Double, daysAgo: Double, now: Instant) =
    IndexHit(id, s"fact:$id", "", Nil, None, MemoryState.active, now.minusMillis((daysAgo * 86_400_000).toLong), distance)

  def spec = suite("memory")(
    suite("patch")(
      test("replaces a unique literal and deletes with an empty new") {
        assertTrue(
          applyEdits(Roadmap, List(edit("- [ ] Dijkstra", "- [x] Dijkstra"))) == Right("# Roadmap\n\n- [ ] BFS\n- [x] Dijkstra\n- [ ] A*\n"),
          applyEdits(Roadmap, List(edit("- [ ] A*\n", ""))) == Right("# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n"),
        )
      },
      test("refuses ambiguous, empty and partial edit lists") {
        val ambiguous = applyEdits("- [ ] review\n- [ ] review\n- [ ] review\n", List(edit("- [ ] review", "x")))
        val partial = applyEdits(Roadmap, List(edit("- [ ] BFS", "- [x] BFS"), edit("- [ ] Floyd", "x")))
        val both = applyEdits(Roadmap, List(edit("nope one", "x"), edit("nope two", "y")))
        assertTrue(
          ambiguous == Left(List(EditFailure(0, "- [ ] review", EditFailureReason.ambiguous, 3, Nil))),
          partial.left.map(_.head.index) == Left(1),
          applyEdits(Roadmap, List(edit("", "anything"))).left.map(_.head.reason) == Left(EditFailureReason.empty),
          applyEdits(Roadmap, Nil).isLeft,
          both.left.map(_.map(_.index)) == Left(List(0, 1)),
        )
      },
      test("a later edit may target text an earlier one produced") {
        val out = applyEdits(Roadmap, List(edit("- [ ] A*", "- [ ] A*\n- [ ] Bellman-Ford"), edit("- [ ] Bellman-Ford", "- [x] Bellman-Ford")))
        assertTrue(out == Right("# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n- [x] Bellman-Ford\n"))
      },
      test("near matches recover normalised quotes") {
        val failure = applyEdits("Цель — пройти графы.\n", List(edit("Цель - пройти графы.", "x"))).left.toOption.get.head
        assertTrue(
          findNearMatches("Цель — пройти графы за месяц.\n", "Цель - пройти графы за месяц.", 3) == List("Цель — пройти графы за месяц."),
          findNearMatches("- [ ] Обойдём граф в ширину\n", "Обойдем граф в ширину", 3) == List("Обойдём граф в ширину"),
          findNearMatches("Плана\n    нет\n", "Плана нет", 3) == List("Плана\n    нет"),
          findNearMatches("Проект «Графы» стартовал.\n", "Проект \"Графы\" стартовал.", 3) == List("Проект «Графы» стартовал."),
          findNearMatches("# Roadmap\n\n- [ ] Dijkstra shortest path\n- [ ] Unrelated topic\n", "- [ ] Dijkstra shortest paths", 3) ==
            List("- [ ] Dijkstra shortest path"),
          findNearMatches("# Roadmap\n\n- [ ] BFS\n", "completely unrelated sentence", 3).isEmpty,
          failure.reason == EditFailureReason.not_found,
          failure.suggestions == List("Цель — пройти графы."),
        )
      },
      test("inverts edits except deletions") {
        assertTrue(
          invertEdits(List(edit("a", "b"), edit("c", "d"))).contains(List(edit("d", "c"), edit("b", "a"))),
          invertEdits(List(edit("gone", ""))).isEmpty,
        )
      },
      test("appends with stable spacing and under headings") {
        assertTrue(
          appendToBody("# Progress\n\nDay 1: BFS\n", "Day 2: Dijkstra", None) == Right("# Progress\n\nDay 1: BFS\n\nDay 2: Dijkstra\n"),
          appendToBody("# Progress\n\n\n\n", "Day 1", None) == Right("# Progress\n\nDay 1\n"),
          appendToBody("", "first note", None) == Right("first note\n"),
          appendToBody("# Doc\n\n## Progress\n\nDay 1\n\n## Mistakes\n\nForgot visited set\n", "Day 2", Some("Progress")) ==
            Right("# Doc\n\n## Progress\n\nDay 1\n\nDay 2\n\n## Mistakes\n\nForgot visited set\n"),
          appendToBody("## Progress\n\nDay 1\n\n### Notes\n\nn\n\n## Mistakes\n\nm\n", "Day 2", Some("## Progress")) ==
            Right("## Progress\n\nDay 1\n\n### Notes\n\nn\n\nDay 2\n\n## Mistakes\n\nm\n"),
          appendToBody("## Progress\n\nDay 1\n\n## Mistakes\n\nm\n", "x", Some("Roadmap")) == Left(List("## Progress", "## Mistakes")),
        )
      },
      test("lists headings") {
        assertTrue(listHeadings("# A\n\ntext\n\n### B\n#not a heading\n") == List(Heading(1, "A", 0), Heading(3, "B", 4)))
      },
    ),
    suite("projection")(
      test("chunks at headings with breadcrumbs") {
        val body = "# Project\n\nIntro line.\n\n## Progress\n\nDay 1\n\n### Notes\n\nWatch out\n"
        assertTrue(
          chunkMarkdown(body, DefaultChunkChars) == List(
            MarkdownChunk("Intro line.", "Project"),
            MarkdownChunk("Day 1", "Project > Progress"),
            MarkdownChunk("Watch out", "Project > Progress > Notes"),
          ),
          chunkMarkdown("## A\n\na\n\n### A1\n\na1\n\n## B\n\nb\n", 1200).map(_.headingPath) == List("A", "A > A1", "B"),
        )
      },
      test("packs and splits paragraphs by budget") {
        val body = List("## Log", "a" * 60, "b" * 60, "c" * 60).mkString("\n\n")
        val chunks = chunkMarkdown(body, 100)
        assertTrue(
          chunkMarkdown("## Log\n\none\n\ntwo\n\nthree\n", 100) == List(MarkdownChunk("one\n\ntwo\n\nthree", "Log")),
          chunks.size == 3,
          chunks.forall(c => c.text.length <= 100 && c.headingPath == "Log"),
          chunkMarkdown("x" * 250, 100).map(_.text.length) == List(100, 100, 50),
          chunkMarkdown("loose note\n", 1200) == List(MarkdownChunk("loose note", "")),
          chunkMarkdown("", 1200).isEmpty && chunkMarkdown("# Title\n", 1200).isEmpty,
        )
      },
      test("index text names its subject") {
        assertTrue(
          buildIndexText("Графы для интервью", "progress.md", "Прогресс", "застрял на Dijkstra") ==
            "Графы для интервью — progress.md · Прогресс\n\nзастрял на Dijkstra",
          buildIndexText("P", "d.md", "", "body") == "P — d.md\n\nbody",
        )
      },
      test("recency breaks ties but never beats relevance") {
        val now = Instant.now()
        def order(hits: List[IndexHit]) = rankHits(hits, now, DefaultRank).map(_._1.id)
        def score(h: IndexHit) = rankHits(List(h), now, DefaultRank).head._2
        assertTrue(
          order(List(hit(1, 0.3, 400, now), hit(2, 0.3, 0, now))) == List(2L, 1L),
          order(List(hit(1, 0.10, 400, now), hit(2, 0.40, 0, now))) == List(1L, 2L),
          math.abs(score(hit(1, 0.5, 0, now)) - 0.45) < 1e-9,
          math.abs(score(hit(1, 0.5, 30, now)) - 0.475) < 1e-6,
          math.abs(score(hit(1, 0.5, -10, now)) - 0.45) < 1e-9,
        )
      },
      test("refs parse strictly") {
        assertTrue(
          Refs.parse("fact:88").contains(MemoryRef.FactRef(88)),
          Refs.parse("doc:leetcode-graphs/roadmap.md#2").contains(MemoryRef.DocRef("leetcode-graphs", "roadmap.md", Some(2))),
          Refs.parse("doc:Bad/roadmap.md").isEmpty,
          Refs.parse("fact:x").isEmpty,
          Refs.docRef("p", "a.md", Some(1)) == "doc:p/a.md#1",
        )
      },
    ),
    suite("service")(
      test("round-trips a project and lists docs with summaries") {
        for
          h <- harness
          _ <- h.service.createProject("leetcode-graphs", "Графы для интервью")
          _ <- h.service.writeDoc(write("leetcode-graphs", "passport.md", "# Паспорт\n\nЦель.\n").copy(summary = Some("Цель и рамки проекта")))
          _ <- h.service.writeDoc(write("leetcode-graphs", "roadmap.md", "# Roadmap\n").copy(summary = Some("Список тем")))
          list <- h.service.listDocs("leetcode-graphs")
          roadmap <- h.service.readDoc("leetcode-graphs", "roadmap.md")
        yield assertTrue(
          list.project.title == "Графы для интервью",
          list.docs.map(d => (d.name, d.summary, d.version)) ==
            List(("passport.md", Some("Цель и рамки проекта"), 1), ("roadmap.md", Some("Список тем"), 1)),
          roadmap.body == "# Roadmap\n",
        )
      },
      test("appends without a version and names what exists on misses") {
        for
          h <- harness
          _ <- h.service.createProject("p", "P")
          _ <- h.service.writeDoc(write("p", "progress.md", "# Прогресс\n\nДень 1: BFS\n"))
          written <- h.service.appendDoc("p", "progress.md", "День 2: Dijkstra", None, Actor, None)
          body <- h.service.readDoc("p", "progress.md").map(_.body)
          missingDoc <- code(h.service.appendDoc("p", "roadmap2.md", "y", None, Actor, None))
          missingProject <- code(h.service.readDoc("q", "a.md"))
          badSlug <- h.service.createProject("Bad Slug", "x").flip.map(_.getMessage)
          badName <- h.service.writeDoc(write("p", "../etc/passwd", "x")).flip.map(_.getMessage)
          exists <- code(h.service.createProject("p", "Другое"))
        yield assertTrue(
          written.version == 2,
          body == "# Прогресс\n\nДень 1: BFS\n\nДень 2: Dijkstra\n",
          missingDoc._1 == "doc_not_found",
          missingDoc._2.get("docs").contains(Json.Arr(Json.Str("progress.md"))),
          missingProject._1 == "project_not_found",
          missingProject._2.get("projects").contains(Json.Arr(Json.Str("p"))),
          badSlug.contains("Invalid project slug"),
          badName.contains("Invalid document name"),
          exists._1 == "project_exists",
        )
      },
      test("write_doc never runs blind") {
        for
          h <- harness
          _ <- h.service.createProject("p", "P")
          first <- h.service.writeDoc(write("p", "a.md", "one\n"))
          blind <- code(h.service.writeDoc(write("p", "a.md", "two\n")))
          body <- h.service.readDoc("p", "a.md").map(_.body)
          ahead <- code(h.service.writeDoc(write("p", "new.md", "x").copy(expectedVersion = Some(3))))
        yield assertTrue(
          first.version == 1,
          blind._1 == "version_required",
          blind._2.get("currentVersion").contains(Json.Num(1)),
          body == "one\n",
          ahead._1 == "version_conflict",
        )
      },
      test("patch_doc checks versions and leaves failures untouched") {
        for
          h <- harness
          _ <- h.service.createProject("p", "P")
          _ <- h.service.writeDoc(write("p", "roadmap.md", Roadmap))
          ok <- h.service.patchDoc("p", "roadmap.md", 1, List(edit("- [ ] Dijkstra", "- [x] Dijkstra")), Actor, Some("отметь"))
          stale <- code(h.service.patchDoc("p", "roadmap.md", 1, List(edit("- [ ] BFS", "x")), Actor, None))
          partial <- code(h.service.patchDoc("p", "roadmap.md", 2, List(edit("- [ ] BFS", "- [x] BFS"), edit("- [ ] Floyd", "x")), Actor, None))
          doc <- h.service.readDoc("p", "roadmap.md")
        yield assertTrue(
          ok.version == 2,
          stale._1 == "version_conflict",
          stale._2.get("currentVersion").contains(Json.Num(2)),
          partial._1 == "edit_failed",
          partial._2.get("applied").contains(Json.Bool(false)),
          doc.version == 2,
          doc.body.contains("- [ ] BFS"),
        )
      },
      test("append absorbs a write that lands between read and commit") {
        for
          h <- harness
          _ <- h.service.createProject("p", "P")
          _ <- h.service.writeDoc(write("p", "log.md", "start\n"))
          sneaked <- Ref.make(false)
          _ <- ZIO.succeed {
            h.store.beforeUpdate = Some(docId =>
              sneaked.getAndSet(true).flatMap { already =>
                ZIO.unless(already) {
                  (for
                    current <- h.store.getDoc(1, "log.md").someOrFailException
                    _ <- ZIO.succeed { h.store.beforeUpdate = None }
                    _ <- h.store.updateDoc(docId, current.version, s"${current.body}\nfrom other agent\n", None)
                  yield ()).orDie
                }.unit
              }
            )
          }
          _ <- h.service.appendDoc("p", "log.md", "mine", None, Actor, None)
          body <- h.service.readDoc("p", "log.md").map(_.body)
        yield assertTrue(body.contains("from other agent"), body.contains("mine"))
      },
      test("history and revert follow the rules") {
        for
          h <- harness
          s = h.service
          _ <- s.createProject("p", "P")
          _ <- s.writeDoc(write("p", "roadmap.md", "- [ ] BFS\n- [ ] Dijkstra\n"))
          first <- s.patchDoc("p", "roadmap.md", 1, List(edit("- [ ] BFS", "- [x] BFS")), "claude-code", Some("BFS пройден"))
          history <- s.history("p", "roadmap.md", 20)
          // Newest reverts exactly, and the revert is itself a patch.
          _ <- s.revert("p", "roadmap.md", first.patchId, rollback = false, Actor)
          reverted <- s.readDoc("p", "roadmap.md")
          // Mid-stack: untouched text reverts in place, an extended line still
          // reverts, a rewritten anchor conflicts and offers a rollback.
          a <- s.patchDoc("p", "roadmap.md", 3, List(edit("- [ ] BFS", "- [x] BFS")), Actor, None)
          _ <- s.patchDoc("p", "roadmap.md", 4, List(edit("- [x] BFS", "- [x] BFS (повторить)")), Actor, None)
          _ <- s.revert("p", "roadmap.md", a.patchId, rollback = false, Actor)
          midStack <- s.readDoc("p", "roadmap.md").map(_.body)
          b <- s.patchDoc("p", "roadmap.md", 6, List(edit("- [ ] Dijkstra", "- [x] Dijkstra")), Actor, None)
          _ <- s.patchDoc("p", "roadmap.md", 7, List(edit("- [x] Dijkstra", "- [x] Дейкстра")), Actor, None)
          conflict <- code(s.revert("p", "roadmap.md", b.patchId, rollback = false, Actor))
          _ <- s.revert("p", "roadmap.md", b.patchId, rollback = true, Actor)
          rolledBack <- s.readDoc("p", "roadmap.md").map(_.body)
          unknown <- code(s.revert("p", "roadmap.md", "pa:beef", rollback = false, Actor))
        yield assertTrue(
          history.head.kind == PatchKind.patch,
          history.head.editCount == 1,
          history(1).versionBefore == 0,
          history.head.patchId.startsWith("pa:"),
          reverted.body == "- [ ] BFS\n- [ ] Dijkstra\n",
          reverted.version == 3,
          midStack == "- [ ] BFS (повторить)\n- [ ] Dijkstra\n",
          conflict._1 == "revert_conflict",
          conflict._2.get("rollbackToVersion").contains(Json.Num(6)),
          rolledBack == "- [ ] BFS (повторить)\n- [ ] Dijkstra\n",
          unknown._1 == "patch_not_found",
          unknown._2.get("knownPatchIds").exists(_ != Json.Arr()),
        )
      },
      test("an append cannot be undone in place but names the rollback") {
        for
          h <- harness
          _ <- h.service.createProject("p", "P")
          _ <- h.service.writeDoc(write("p", "r.md", "x\n"))
          appended <- h.service.appendDoc("p", "r.md", "- [ ] Floyd", None, Actor, None)
          _ <- h.service.appendDoc("p", "r.md", "- [ ] Kruskal", None, Actor, None)
          conflict <- code(h.service.revert("p", "r.md", appended.patchId, rollback = false, Actor))
        yield assertTrue(conflict._1 == "revert_conflict", conflict._2.get("rollbackToVersion").contains(Json.Num(1)))
      },
      test("facts round-trip and archive out of recall") {
        for
          h <- harness
          s = h.service
          fact <- s.remember("Лёша платит за интернет 1-го числа", Some(List("интернет")), None)
          loaded <- s.getFact(fact.id)
          empty <- code(s.remember("  ", None, None))
          missing <- code(s.getFact(999))
          now <- Clock.instant
          hits <- s.recall("интернет Лёша", None, None, None, now)
          _ <- s.updateFact(fact.id, None, None, Some(MemoryState.archived))
          gone <- s.recall("интернет Лёша", None, None, None, now)
          archived <- s.recall("интернет Лёша", None, Some(List(MemoryState.archived)), None, now)
        yield assertTrue(
          loaded.body == fact.body,
          empty._1 == "empty_body",
          missing._1 == "fact_not_found",
          hits.head.ref == Refs.factRef(fact.id),
          gone.isEmpty,
          archived.size == 1,
        )
      },
      test("recall spans documents and drops stale chunks") {
        for
          h <- harness
          s = h.service
          _ <- s.createProject("graphs", "Графы")
          _ <- s.writeDoc(write("graphs", "progress.md", "## Прогресс\n\nзастрял на dijkstra\n"))
          _ <- s.remember("купить молоко и хлеб", None, None)
          now <- Clock.instant
          hits <- s.recall("dijkstra прогресс графы", Some(1), None, None, now)
          _ <- s.writeDoc(write("graphs", "progress.md", "").copy(expectedVersion = Some(1)))
          after <- s.recall("dijkstra прогресс графы", None, None, None, now)
        yield assertTrue(hits.head.ref == "doc:graphs/progress.md#0", after.forall(!_.ref.startsWith("doc:")))
      },
      test("the embedder being down degrades search, not writes") {
        for
          h <- harness
          s = h.service
          _ <- ZIO.succeed(h.embedder.down.set(true))
          _ <- s.createProject("p", "P")
          _ <- s.writeDoc(write("p", "a.md", "текст\n"))
          _ <- s.patchDoc("p", "a.md", 1, List(edit("текст", "новый текст")), Actor, None)
          now <- Clock.instant
          down <- code(s.recall("текст", None, None, None, now))
          _ <- ZIO.succeed(h.embedder.down.set(false))
          drained <- s.indexer.embedMissingBatch(100)
          again <- s.indexer.embedMissingBatch(100)
          hits <- s.recall("новый текст", None, None, None, now)
        yield assertTrue(
          down._1 == "search_unavailable",
          drained == EmbedResult(1, 0),
          again.embedded == 0,
          hits.head.ref == "doc:p/a.md#0",
        )
      },
      test("imports legacy notes idempotently") {
        val notes = List(MemoryService.LegacyNote(1, "пароль от роутера на наклейке", List("роутер")), MemoryService.LegacyNote(2, "   ", Nil))
        for
          h <- harness
          first <- MemoryService.importLegacyNotes(notes, h.service)
          second <- MemoryService.importLegacyNotes(notes, h.service)
          fact <- h.store.getFactBySource(MemoryService.legacyNoteSource(1)).someOrFailException
        yield assertTrue(first == (1, 1), second == (0, 2), fact.tags == List("роутер"))
      },
      test("the tool envelope flattens payloads and errors") {
        for
          ok <- MemoryTools.envelope(ZIO.succeed(WriteResult("p", "a.md", 2, "pa:1", 3)))
          err <- MemoryTools.envelope(ZIO.fail(MemoryService.versionConflict("p", "a.md", 9, 1)): Task[WriteResult])
          // A malformed slug is not a MemoryError: it fails the call.
          failed <- MemoryTools.envelope(ZIO.fail(ToolFailure("Invalid project slug")): Task[WriteResult]).either
        yield assertTrue(
          ok == Json.Obj(
            "ok" -> Json.Bool(true),
            "project" -> Json.Str("p"),
            "doc" -> Json.Str("a.md"),
            "version" -> Json.Num(2),
            "patchId" -> Json.Str("pa:1"),
            "sizeBytes" -> Json.Num(3),
          ),
          err.get("ok").contains(Json.Bool(false)),
          err.get("error").contains(Json.Str("version_conflict")),
          err.get("currentVersion").contains(Json.Num(9)),
          failed.isLeft,
        )
      },
    ),
    // The same rules against Postgres: the CAS, patch history order, revert,
    // index replacement and recall filters are SQL there, not Scala.
    test("the Postgres store upholds the contract") {
      ZIO.scoped {
        for
          pool <- TestPg.newsPool
          s = MemoryService.make(PgMemoryStore(pool), FakeEmbedder(1536))
          slug <- Random.nextInt.map(n => s"pg-test-${n.abs}")
          _ <- s.createProject(slug, "PG test")
          again <- code(s.createProject(slug, "again"))
          created <- s.writeDoc(write(slug, "roadmap.md", "## Темы\n\n- [ ] BFS\n- [ ] Dijkstra\n").copy(summary = Some("roadmap")))
          patched <- s.patchDoc(slug, "roadmap.md", 1, List(edit("- [ ] BFS", "- [x] BFS")), Actor, Some("BFS done"))
          stale <- code(s.patchDoc(slug, "roadmap.md", 1, List(edit("x", "y")), Actor, None))
          _ <- s.appendDoc(slug, "roadmap.md", "- [ ] A*", Some("Темы"), Actor, None)
          doc <- s.readDoc(slug, "roadmap.md")
          history <- s.history(slug, "roadmap.md", 10)
          _ <- s.revert(slug, "roadmap.md", patched.patchId, rollback = false, Actor)
          afterRevert <- s.readDoc(slug, "roadmap.md").map(_.body)
          now <- Clock.instant
          hits <- s.recall("Dijkstra темы", Some(5), None, None, now)
          tag <- Random.nextInt.map(n => s"tag${n.abs}")
          fact <- s.remember("кот любит сметану", Some(List(tag)), Some("test"))
          tagged <- s.recall("кот сметана", Some(5), None, Some(List(tag)), now)
          _ <- s.updateFact(fact.id, Some(" кот любит сливки "), None, Some(MemoryState.archived))
          updated <- s.getFact(fact.id)
          activeOnly <- s.recall("кот", Some(5), None, Some(List(tag)), now)
          archived <- s.recall("кот", Some(5), Some(List(MemoryState.archived)), Some(List(tag)), now)
          // Emptying a document drops its chunks from the projection.
          _ <- s.writeDoc(write(slug, "roadmap.md", "").copy(expectedVersion = Some(4)))
          after <- s.recall("Dijkstra темы", Some(50), None, None, now)
        yield assertTrue(
          again._1 == "project_exists",
          created.version == 1,
          stale._1 == "version_conflict",
          doc.version == 3,
          doc.summary.contains("roadmap"),
          doc.body == "## Темы\n\n- [x] BFS\n- [ ] Dijkstra\n\n- [ ] A*\n",
          history.map(_.kind) == List(PatchKind.append, PatchKind.patch, PatchKind.write),
          history(1).rationale.contains("BFS done"),
          afterRevert.contains("- [ ] BFS"),
          hits.exists(_.ref == s"doc:$slug/roadmap.md#0"),
          tagged.map(_.ref) == List(Refs.factRef(fact.id)),
          updated.body == "кот любит сливки",
          activeOnly.isEmpty,
          archived.size == 1,
          after.forall(!_.ref.startsWith(s"doc:$slug/")),
        )
      }
    },
  ) @@ withLiveClock @@ withLiveRandom
