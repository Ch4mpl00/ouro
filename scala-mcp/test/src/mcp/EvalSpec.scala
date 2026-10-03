package mcp

import zio.json.*
import zio.test.*

object EvalSpec extends ZIOSpecDefault:
  private def r(id: Long) = Retrieved(id, 0.1 * id)

  def spec = suite("eval")(
    test("first-gold rank is one-indexed") {
      assertTrue(
        Eval.firstGoldRank(List(3L), List(r(1), r(2), r(3))).contains(3),
        Eval.firstGoldRank(List(9L), List(r(1))).isEmpty,
        Eval.mean(Nil).isNaN,
        Eval.mean(List(1.0, 3.0)) == 2.0,
      )
    },
    test("aggregates only queries with gold") {
      def q(id: String, gold: List[Long]) = QueryRow(id, id, id, gold, Nil)
      val top = List(r(1), r(2), r(3))
      val agg = Eval.aggregate(List(Eval.scoreQuery(q("a", List(2L)), top, Map.empty), Eval.scoreQuery(q("b", Nil), top, Map.empty)))
      assertTrue(agg.scoredQueries == 1, agg.recallAt5 == 1.0, agg.mrr == 0.5)
    },
    test("the corpus cache key matches the TS harness") {
      val config =
        """{"name":"x","retrieval":{"embed":{"model":"text-embedding-3-small","dimensions":1536},"buildText":"title+body","topK":10,"dedup":null,"rerank":null},"query":{"field":"query"},"scoring":{"mode":"binary"}}"""
          .fromJson[EvalConfig]
          .toOption
          .get
      val expected = java.security.MessageDigest
        .getInstance("SHA-256")
        .digest("""{"model":"text-embedding-3-small","dimensions":1536,"buildText":"title+body"}""".getBytes("UTF-8"))
        .map(b => f"$b%02x")
        .mkString
        .take(16)
      assertTrue(Eval.hashCorpusInputs(config) == expected)
    },
    test("loads every shipped eval config") {
      // Tests run in a sandbox under out/; Mill names the project root.
      val root = Env.get("MILL_WORKSPACE_ROOT").getOrElse(".")
      val dir = java.nio.file.Path.of(root, "..", "crates/mcp/eval/configs")
      val files = java.nio.file.Files.list(dir).toArray.toList.map(_.asInstanceOf[java.nio.file.Path])
      zio.ZIO.foreach(files)(Eval.loadConfig).map(configs => assertTrue(configs.size == files.size, configs.nonEmpty))
    },
  )
