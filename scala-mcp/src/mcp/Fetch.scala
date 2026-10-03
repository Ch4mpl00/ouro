package mcp

// fetch_url: GET a page and return its RAW body (HTML / JSON / text) — for
// parsing a <table> in a code_agent step, or a URL the search provider can't
// retrieve. Complements tavily_extract, which returns cleaned prose.
//
// Sections:
//   1. ssrf guard — only public addresses, enforced at DNS resolution
//   2. client     — the guarded HTTP client, redirects followed by hand
//   3. tools      — `fetch` toolset (never handed to third-party clients:
//                   it would make this server their proxy)
//
// The check lives in the client's DNS resolver, so every connection —
// including each redirect hop and a DNS answer that changed since validation —
// is checked, not just the URL the model typed.

import java.net.{Inet4Address, Inet6Address, InetAddress, URI, UnknownHostException}

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.{description, validate}
import sttp.tapir.Validator
import zio.*
import zio.http.*
import zio.http.netty.NettyConfig
import zio.json.*

// ── 1. ssrf guard ────────────────────────────────────────────────────────────

object Ssrf:
  // Loopback, RFC1918, link-local / cloud metadata (169.254.169.254), the
  // unspecified address, IPv6 loopback / link-local / unique-local. The JDK
  // already turns IPv4-mapped IPv6 (::ffff:10.0.0.1) into an Inet4Address.
  def isPrivate(ip: InetAddress): Boolean = ip match
    case v4: Inet4Address =>
      val Array(a, b, _, _) = v4.getAddress.map(_ & 0xff)
      a == 0 || a == 10 || a == 127 || (a == 169 && b == 254) || (a == 172 && b >= 16 && b <= 31) ||
      (a == 192 && b == 168)
    case v6: Inet6Address =>
      val bytes = v6.getAddress.map(_ & 0xff)
      val first = (bytes(0) << 8) | bytes(1)
      v6.isLoopbackAddress || v6.isAnyLocalAddress || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
    case _ => true

  private def literalIp(host: String): Option[InetAddress] =
    val bare = host.stripPrefix("[").stripSuffix("]")
    val looksLiteral = bare.contains(':') || bare.matches("""\d{1,3}(\.\d{1,3}){3}""")
    if looksLiteral then scala.util.Try(InetAddress.getByName(bare)).toOption else None

  def assertPublicUrl(raw: String): IO[ToolFailure, URI] =
    for
      uri <- ZIO
        .attempt(URI(raw.trim))
        .filterOrFail(u => u.isAbsolute && u.getHost != null || u.getScheme != null)(())
        .orElseFail(ToolFailure(s"invalid URL: $raw"))
      scheme = Option(uri.getScheme).getOrElse("").toLowerCase
      _ <- ZIO.when(scheme != "http" && scheme != "https")(ZIO.fail(ToolFailure(s"""unsupported scheme "$scheme:" — only http/https""")))
      host <- ZIO.fromOption(Option(uri.getHost)).orElseFail(ToolFailure(s"invalid URL: $raw"))
      addrs <- literalIp(host) match
        case Some(ip) => ZIO.succeed(List(ip))
        case None =>
          ZIO.attemptBlocking(InetAddress.getAllByName(host).toList).orElseFail(ToolFailure(s"""could not resolve host "$host""""))
      _ <- ZIO.foreachDiscard(addrs.find(isPrivate))(ip =>
        ZIO.fail(ToolFailure(s"refusing to fetch private/loopback address ($host → ${ip.getHostAddress})"))
      )
    yield uri

  // Every connection the guarded client opens resolves through this.
  object PublicOnlyResolver extends DnsResolver:
    def resolve(host: String)(implicit trace: Trace): ZIO[Any, UnknownHostException, Chunk[InetAddress]] =
      ZIO
        .attemptBlocking(Chunk.fromArray(InetAddress.getAllByName(host)))
        .refineToOrDie[UnknownHostException]
        .flatMap { addrs =>
          addrs.find(isPrivate) match
            case Some(bad) =>
              ZIO.fail(UnknownHostException(s"refusing to fetch private/loopback address ($host → ${bad.getHostAddress})"))
            case None => ZIO.succeed(addrs)
        }

// ── 2. client ────────────────────────────────────────────────────────────────

final case class Fetched(
    url: String,
    status: Int,
    contentType: String,
    bytes: Option[Long] = None,
    truncated: Option[Boolean] = None,
    content: Option[String] = None,
    binary: Option[Boolean] = None,
    note: Option[String] = None,
)

object Fetched:
  // Only the fields a reply carries: a text page or the binary notice.
  given JsonEncoder[Fetched] = JsonEncoder[zio.json.ast.Json].contramap { f =>
    import zio.json.ast.Json
    val base = List("url" -> Json.Str(f.url), "status" -> Json.Num(f.status), "contentType" -> Json.Str(f.contentType))
    val rest = f.binary match
      case Some(_) => List("binary" -> Json.Bool(true), "note" -> Json.Str(f.note.getOrElse("")))
      case None =>
        List(
          "bytes" -> Json.Num(f.bytes.getOrElse(0L)),
          "truncated" -> Json.Bool(f.truncated.getOrElse(false)),
          "content" -> Json.Str(f.content.getOrElse("")),
        )
    Json.Obj((base ++ rest)*)
  }

final class Fetcher(client: Client):
  import Fetcher.*

  // The same guarded client, following redirects — for fetch_article, which
  // wants a page, not a raw hop. Each hop still resolves through the guard.
  val http: HttpClient = HttpClient.following(client)

  def fetch(raw: String, cap: Int): Task[Fetched] =
    Ssrf.assertPublicUrl(raw).flatMap(uri => hop(uri.toString, cap, redirects = 0)).timeoutFail(
      ToolFailure(s"fetch failed: timed out after ${Timeout.toMillis}ms")
    )(Timeout)

  // Redirects by hand, so each hop's scheme and address are vetted too.
  private def hop(url: String, cap: Int, redirects: Int): Task[Fetched] =
    ZIO.scoped {
      for
        target <- HttpClient.url(url)
        request = Request.get(target).addHeader(Header.Accept(MediaType.any)).addHeader(Header.Custom("User-Agent", BrowserUa))
        res <- ZClient.streaming(request).provideSomeEnvironment[Scope](_.add(client)).mapError(err =>
          ToolFailure(s"fetch failed: ${Results.describe(err)}")
        )
        out <- res.header(Header.Location) match
          case Some(location) if res.status.isRedirection =>
            if redirects >= 10 then Tools.fail("fetch failed: too many redirects")
            else
              val next = URI(url).resolve(location.renderedValue).toString
              Ssrf.assertPublicUrl(next).mapError(e => ToolFailure(s"fetch failed: redirect: ${e.getMessage}")) *>
                hop(next, cap, redirects + 1)
          case _ => read(url, res, cap)
      yield out
    }

  private def read(url: String, res: Response, cap: Int): Task[Fetched] =
    val contentType = res.header(Header.ContentType).map(_.renderedValue).getOrElse("")
    if isBinary(contentType) then
      ZIO.succeed(
        Fetched(url, res.status.code, contentType, binary = Some(true), note = Some("binary content not returned; for a PDF use read_pdf"))
      )
    else
      res.body.asStream.take(cap.toLong + 1).runCollect.map { bytes =>
        val kept = bytes.take(cap)
        Fetched(
          url,
          res.status.code,
          contentType,
          bytes = Some(bytes.size.toLong),
          truncated = Some(bytes.size > cap),
          content = Some(String(kept.toArray, java.nio.charset.StandardCharsets.UTF_8)),
        )
      }

object Fetcher:
  val DefaultMaxBytes = 2_000_000
  val HardMaxBytes = 8_000_000
  val Timeout: Duration = 20.seconds
  // Some sites 403 a missing/unknown User-Agent.
  val BrowserUa =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"

  private val Binary = "(?i)^(image|audio|video)/|application/(pdf|zip|octet-stream|x-)".r

  def isBinary(contentType: String): Boolean = Binary.findFirstIn(contentType).isDefined

  // A client whose every connection goes through the public-only resolver.
  val layer: ZLayer[Any, Throwable, Fetcher] =
    (ZLayer.succeed(ZClient.Config.default.addUserAgentHeader(false)) ++
      ZLayer.succeed(NettyConfig.defaultWithFastShutdown) ++
      ZLayer.succeed[DnsResolver](Ssrf.PublicOnlyResolver)) >>> Client.live >>> ZLayer.fromFunction(Fetcher(_))

// ── 3. tools ─────────────────────────────────────────────────────────────────

object FetchTools:
  import Tools.*

  final case class FetchParams(
      @description("Absolute http(s) URL.") url: String,
      @description("Cap on bytes returned (default 2000000).") @validate(Validator.inRange(1, 8_000_000)) maxBytes: Option[Int],
  ) derives JsonDecoder, Schema

  val tools: List[ToolDef] = List(
    tool(
      "fetch_url",
      "Fetch a URL's raw content",
      "GET a URL and return its RAW body (HTML / JSON / text). Use when you already have a link and need the raw " +
        "content — e.g. the raw HTML to parse a <table> in a code_agent step, or a page the search/extract provider " +
        "can't retrieve. Complements tavily_extract (which returns cleaned prose). Follows redirects; binary content " +
        "(PDF/image/zip) is not returned — use read_pdf for PDFs.",
    ) { (deps, p: FetchParams) =>
      ZIO.when(p.maxBytes.exists(m => m < 1 || m > Fetcher.HardMaxBytes))(invalid("maxBytes must be between 1 and 8000000")) *>
        deps.fetcher.fetch(p.url, p.maxBytes.getOrElse(Fetcher.DefaultMaxBytes).min(Fetcher.HardMaxBytes))
    }
  )
