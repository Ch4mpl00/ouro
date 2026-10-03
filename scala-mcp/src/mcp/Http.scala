package mcp

// Outbound HTTP for every REST API this server speaks — Gmail, Telegram Bot
// API, Monobank, OpenAI, HN/Habr. A thin layer over the ZIO HTTP client: the
// domains build requests, this sends them and hands back status + body.

import zio.*
import zio.http.*
import zio.json.*
import zio.json.ast.Json

final case class HttpReply(status: Int, body: String, headers: Headers):
  def ok: Boolean = status >= 200 && status < 300

  def json: Either[String, Json] = body.fromJson[Json]

  def as[A: JsonDecoder]: Either[String, A] = body.fromJson[A]

final class HttpClient(client: Client):
  def send(request: Request, timeout: Duration = 30.seconds): Task[HttpReply] =
    client
      .batched(request)
      .flatMap(res => res.body.asString.map(HttpReply(res.status.code, _, res.headers)))
      .timeoutFail(RuntimeException(s"request timed out after ${timeout.toMillis}ms"))(timeout)

  def get(url: String, headers: Headers = Headers.empty, timeout: Duration = 30.seconds): Task[HttpReply] =
    HttpClient.url(url).flatMap(u => send(Request.get(u).addHeaders(headers), timeout))

  def postJson(
      url: String,
      body: Json,
      headers: Headers = Headers.empty,
      timeout: Duration = 30.seconds
  ): Task[HttpReply] =
    HttpClient.url(url).flatMap { u =>
      val request = Request
        .post(u, Body.fromString(body.toJson))
        .addHeader(Header.ContentType(MediaType.application.json))
        .addHeaders(headers)
      send(request, timeout)
    }

  def postForm(url: String, form: Seq[(String, String)], timeout: Duration = 30.seconds): Task[HttpReply] =
    HttpClient.url(url).flatMap { u =>
      val body = Body.fromURLEncodedForm(Form(form.map((k, v) => FormField.simpleField(k, v))*))
      send(Request.post(u, body), timeout)
    }

object HttpClient:
  def url(raw: String): Task[URL] =
    ZIO.fromEither(URL.decode(raw)).mapError(e => RuntimeException(s"invalid URL $raw: $e"))

  // Redirects followed like reqwest/fetch do by default (up to 10 hops).
  def following(client: Client): HttpClient =
    HttpClient(client @@ ZClientAspect.followRedirects(10)((res, _) => ZIO.succeed(res)))

  // "?a=1&b=2" with each value percent-encoded.
  def query(params: (String, String)*): String =
    if params.isEmpty then ""
    else
      params
        .map((k, v) => s"$k=${java.net.URLEncoder.encode(v, java.nio.charset.StandardCharsets.UTF_8)}")
        .mkString("?", "&", "")
