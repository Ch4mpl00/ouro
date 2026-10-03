package mcp

// Text → vector, and the vector math every retrieval path shares.
//
// Sections:
//   1. embedder  — the `Embedder` port + the OpenAI implementation
//   2. retrieval — cosine distance and near-duplicate filtering
//
// Generic infrastructure: nothing here knows a table. Domains (news,
// knowledge, memory) compose the text they embed and store the vectors.

import zio.*
import zio.http.{Header, Headers}
import zio.json.*
import zio.json.ast.Json

// ── 1. embedder ──────────────────────────────────────────────────────────────

trait Embedder:
  // One vector per input, in order.
  def embedBatch(texts: List[String]): Task[List[Vector[Float]]]

  def embedOne(text: String): Task[Vector[Float]] =
    embedBatch(List(text)).flatMap(vs =>
      ZIO.fromOption(vs.headOption).orElseFail(RuntimeException("OpenAI embeddings returned no vector for input"))
    )

// How an inline or backfill embed went: rows embedded, rows left NULL.
final case class EmbedResult(embedded: Int = 0, failed: Int = 0) derives JsonEncoder:
  def +(other: EmbedResult): EmbedResult = EmbedResult(embedded + other.embedded, failed + other.failed)

final class OpenAiEmbedder(
    http: HttpClient,
    apiKey: String,
    model: String = "text-embedding-3-small",
    dimensions: Int = 1536,
    maxChars: Int = OpenAiEmbedder.DefaultMaxChars,
) extends Embedder:
  import OpenAiEmbedder.*

  final private case class Item(index: Int, embedding: Vector[Float]) derives JsonDecoder
  final private case class Response(data: List[Item]) derives JsonDecoder

  private final class Retryable(message: String) extends Exception(message)

  private def request(input: List[String]): Task[List[Vector[Float]]] =
    val body = Json.Obj(
      "model" -> Json.Str(model),
      "input" -> Json.Arr(input.map(Json.Str(_))*),
      "dimensions" -> Json.Num(dimensions),
    )
    val once = http
      .postJson(Url, body, Headers(Header.Authorization.Bearer(apiKey)), timeout = 60.seconds)
      .mapError(err => Retryable(Results.describe(err)))
      .flatMap { reply =>
        if reply.status == 429 || reply.status >= 500 then
          ZIO.fail(Retryable(s"OpenAI embeddings failed (${reply.status}): ${reply.body}"))
        else if !reply.ok then ZIO.fail(ToolFailure(s"OpenAI embeddings failed (${reply.status}): ${reply.body}"))
        else ZIO.fromEither(reply.as[Response]).mapError(ToolFailure(_))
      }
    // The openai SDK's default: two retries on 429 / 5xx / connection errors,
    // 1s then 2s apart.
    once
      .retry(Schedule.recurWhile[Throwable](_.isInstanceOf[Retryable]) && Schedule.exponential(1.second) && Schedule.recurs(MaxRetries))
      .map(_.data.sortBy(_.index).map(_.embedding))

  def embedBatch(texts: List[String]): Task[List[Vector[Float]]] =
    if texts.isEmpty then ZIO.succeed(Nil)
    else
      val heads = texts.map(truncateChars(_, maxChars))
      ZIO.foreachPar(heads.grouped(BatchSize).toList)(request).map(_.flatten)

object OpenAiEmbedder:
  val Url = "https://api.openai.com/v1/embeddings"
  // 8000 chars sits under the ~8191-token limit of text-embedding-3-small at
  // ~4 chars/token. Only the head of a long text is embedded; multi-chunk
  // documents are chunked by their own domain (memory) before they get here.
  val DefaultMaxChars = 8000
  // OpenAI accepts 2048 inputs per request; 100 keeps the payload modest.
  val BatchSize = 100
  val MaxRetries = 2

  def fromEnv(http: HttpClient): IO[String, OpenAiEmbedder] =
    ZIO
      .fromOption(Env.get("OPENAI_API_KEY"))
      .orElseFail("OPENAI_API_KEY is not set. Embeddings need it; see .env.mcp.example.")
      .map(OpenAiEmbedder(http, _))

// On characters (code points), not UTF-16 units: a cut never splits one.
def truncateChars(text: String, maxChars: Int): String =
  if text.codePointCount(0, text.length) <= maxChars then text else text.substring(0, text.offsetByCodePoints(0, maxChars))

// ── 2. retrieval ─────────────────────────────────────────────────────────────

object Retrieval:
  // text-embedding-3 and pgvector vectors are unit-normalised, so cosine
  // distance is 1 - dot. No re-normalisation.
  def cosineDistance(a: Seq[Float], b: Seq[Float]): Double =
    1.0 - a.iterator.zip(b.iterator).map((x, y) => x.toDouble * y.toDouble).sum

  // Catches exact and near-exact copies (a headline reposted to several
  // feeds, channel-noise repeated by one source) without merging distinct
  // posts on one topic. Tuned on the RAG eval golden set
  // (eval configs/baseline-dedup-003.json).
  val DefaultDedupThreshold = 0.03

  // Walks items in input order (callers sort by ascending distance first) and
  // keeps one only if it is at least `threshold` away from everything kept.
  // Items without a vector are dropped, or kept in place with `keepNull`.
  def dedupByPairwiseCosine[T](items: List[T], vector: T => Option[Seq[Float]], threshold: Double, keepNull: Boolean)
      : List[T] =
    if threshold <= 0 then items
    else
      items
        .foldLeft((List.empty[Seq[Float]], List.empty[T])) { case ((keptVectors, kept), item) =>
          vector(item) match
            case None => (keptVectors, if keepNull then item :: kept else kept)
            case Some(v) if keptVectors.forall(cosineDistance(v, _) >= threshold) => (v :: keptVectors, item :: kept)
            case Some(_) => (keptVectors, kept)
        }
        ._2
        .reverse
