package mcp

// Third-party MCP upstreams, re-exposed through this server: the agent sees
// one merged tool list, upstream tools namespaced as `${prefix}__${tool}`
// (`tavily__tavily_search`). Onboarding is config + secret + skill
// frontmatter, no code (.claude/tasks/mcp-gateway.md).
//
// Sections:
//   1. config  — gateway.config.json, ${VAR} secrets resolved from the env
//   2. clients — one Streamable HTTP client per upstream, reconnect-once
//   3. gateway — merged tool list + routing, failures isolated per upstream
//
// The handler in Server.scala asks this module only for the tools it doesn't
// own, so own tools keep their names and always win a collision.

import zio.*
import zio.http.Header
import zio.http.Headers
import zio.http.MediaType
import zio.json.*
import zio.json.ast.Json

import java.nio.file.Files
import java.nio.file.NoSuchFileException
import java.nio.file.Path

// ── 1. config ────────────────────────────────────────────────────────────────

final case class Upstream(name: String, url: String, prefix: String, headers: List[(String, String)])

object GatewayConfig:
  final private case class UpstreamConfig(
      name: String,
      // Only "http": a stdio upstream would spawn a child inside the poller
      // process, out of scope until the gateway has its own container.
      transport: String = "http",
      url: String,
      prefix: Option[String] = None,
      headers: Map[String, String] = Map.empty,
      enabled: Boolean = true
  ) derives JsonDecoder

  final private case class ConfigFile(upstreams: List[UpstreamConfig] = Nil) derives JsonDecoder

  private val Var = """\$\{([A-Z0-9_]+)\}""".r

  def identOk(s: String): Boolean = s.nonEmpty && s.forall(c => (c.isLetterOrDigit && c < 128) || c == '_' || c == '-')

  // ${VAR} → env. Missing variables are returned so the caller can skip the
  // upstream rather than connect with an empty credential.
  private def interpolate(value: String, env: String => Option[String]): (String, List[String]) =
    val missing = Var.findAllMatchIn(value).map(_.group(1)).filter(v => env(v).forall(_.isEmpty)).toList
    (Var.replaceAllIn(value, m => java.util.regex.Matcher.quoteReplacement(env(m.group(1)).getOrElse(""))), missing)

  // The enabled, fully resolvable upstreams. A missing file means none — the
  // gateway is a no-op. One misconfigured upstream is logged and skipped,
  // never fatal.
  def load(path: Path, env: String => Option[String]): Task[List[Upstream]] =
    ZIO
      .attemptBlocking(Some(Files.readString(path)))
      .catchSome { case _: NoSuchFileException => ZIO.none }
      .flatMap {
        case None      => ZIO.succeed(Nil)
        case Some(raw) =>
          for
            file <- ZIO.fromEither(raw.fromJson[ConfigFile]).mapError(e => RuntimeException(s"gateway config: $e"))
            upstreams <- ZIO.foreach(file.upstreams)(resolve(_, env))
          yield upstreams.flatten
      }

  private def resolve(u: UpstreamConfig, env: String => Option[String]): Task[Option[Upstream]] =
    for
      _ <- ZIO.unless(identOk(u.name))(
        ZIO.fail(RuntimeException(s"gateway: name must be [a-zA-Z0-9_-], got \"${u.name}\""))
      )
      _ <- ZIO.unless(u.prefix.forall(identOk))(ZIO.fail(RuntimeException("gateway: prefix must be [a-zA-Z0-9_-]")))
      _ <- ZIO.unless(u.transport == "http")(
        ZIO.fail(RuntimeException(s"gateway: upstream ${u.name} has unsupported transport \"${u.transport}\""))
      )
      resolved <-
        if !u.enabled then ZIO.none
        else
          val (url, missingUrl) = interpolate(u.url, env)
          val headers = u.headers.toList.map((k, v) => k -> interpolate(v, env))
          val missing = (missingUrl ++ headers.flatMap(_._2._2)).distinct.sorted
          if missing.nonEmpty then
            ZIO
              .logWarning(s"skipping gateway upstream ${u.name}: unresolved env var(s) ${missing.mkString(", ")}")
              .as(None)
          else ZIO.some(Upstream(u.name, url, u.prefix.getOrElse(u.name), headers.map((k, v) => k -> v._1)))
    yield resolved

// ── 2. clients ───────────────────────────────────────────────────────────────

// A remote MCP session over Streamable HTTP. Sessions are stateful upstream:
// a restart there invalidates ours, so a lost connection is reopened once and
// the call retried.
final class RemoteClient(val upstream: Upstream, http: HttpClient, session: Ref.Synchronized[RemoteClient.Session]):
  import RemoteClient.*

  private def headers(s: Session): Headers =
    Headers(upstream.headers.map((k, v) => Header.Custom(k, v))*) ++
      Headers(Header.Accept(MediaType.application.json, MediaType.text.`event-stream`)) ++
      Headers(s.id.map(id => Header.Custom("Mcp-Session-Id", id)).toList*) ++
      Headers(s.protocol.map(v => Header.Custom("MCP-Protocol-Version", v)).toList*)

  // One JSON-RPC request; the reply may come as JSON or as an SSE stream
  // carrying it.
  private def request(s: Session, method: String, params: Json): Task[Json] =
    val body =
      Json.Obj("jsonrpc" -> Json.Str("2.0"), "id" -> Json.Num(1), "method" -> Json.Str(method), "params" -> params)
    http.postJson(upstream.url, body, headers(s), CallTimeout).flatMap { reply =>
      if !reply.ok then ZIO.fail(RuntimeException(s"HTTP ${reply.status}: ${reply.body.take(300)}"))
      else ZIO.fromEither(rpcResult(reply)).mapError(RuntimeException(_))
    }

  def listTools: Task[List[Json.Obj]] =
    def page(cursor: Option[String], acc: List[Json.Obj]): Task[List[Json.Obj]] =
      session.get
        .flatMap(s => request(s, "tools/list", cursor.fold(Json.Obj())(c => Json.Obj("cursor" -> Json.Str(c)))))
        .flatMap { result =>
          val tools = result
            .get("tools")
            .collect { case Json.Arr(ts) => ts.toList.collect { case o: Json.Obj => o } }
            .getOrElse(Nil)
          result.get("nextCursor").flatMap(_.asString) match
            case Some(next) if acc.size < 1000 => page(Some(next), acc ++ tools)
            case _                             => ZIO.succeed(acc ++ tools)
        }
    page(None, Nil)

  def call(name: String, args: Json): Task[Json] =
    val params = Json.Obj("name" -> Json.Str(name), "arguments" -> args)
    session.get.flatMap { used =>
      request(used, "tools/call", params).catchSome {
        case err if connectionLost(Results.describe(err)) =>
          ZIO.logWarning(s"gateway upstream ${upstream.name}: connection lost, reconnecting") *>
            session
              .updateSomeAndGetZIO {
                // Single flight: another call may have reconnected already.
                case current if current eq used => open(upstream, http)
              }
              .flatMap(request(_, "tools/call", params))
      }
    }

object RemoteClient:
  final case class Session(id: Option[String], protocol: Option[String])

  val CallTimeout: Duration = 30.seconds
  val ConnectTimeout: Duration = 15.seconds

  def connect(upstream: Upstream, http: HttpClient): Task[RemoteClient] =
    open(upstream, http)
      .timeoutFail(
        RuntimeException(s"upstream '${upstream.name}' connect timed out after ${ConnectTimeout.toSeconds}s")
      )(
        ConnectTimeout
      )
      .flatMap(Ref.Synchronized.make(_))
      .map(RemoteClient(upstream, http, _))

  // initialize → the session id and protocol version → notifications/initialized.
  private def open(upstream: Upstream, http: HttpClient): Task[Session] =
    val custom = Headers(upstream.headers.map((k, v) => Header.Custom(k, v))*)
    val accept = Headers(Header.Accept(MediaType.application.json, MediaType.text.`event-stream`))
    val init = Json.Obj(
      "jsonrpc" -> Json.Str("2.0"),
      "id" -> Json.Num(0),
      "method" -> Json.Str("initialize"),
      "params" -> Json.Obj(
        "protocolVersion" -> Json.Str("2025-06-18"),
        "capabilities" -> Json.Obj(),
        "clientInfo" -> Json.Obj(
          "name" -> Json.Str(McpHandler.ServerName),
          "version" -> Json.Str(McpHandler.ServerVersion)
        )
      )
    )
    for
      reply <- http.postJson(upstream.url, init, custom ++ accept, CallTimeout)
      _ <- ZIO.unless(reply.ok)(
        ZIO.fail(RuntimeException(s"initialize: HTTP ${reply.status}: ${reply.body.take(300)}"))
      )
      result <- ZIO.fromEither(rpcResult(reply)).mapError(RuntimeException(_))
      id = reply.headers.get("mcp-session-id")
      version = result.get("protocolVersion").flatMap(_.asString)
      session = Session(id, version)
      notice = Json.Obj("jsonrpc" -> Json.Str("2.0"), "method" -> Json.Str("notifications/initialized"))
      sessionHeaders = Headers(id.map(v => Header.Custom("Mcp-Session-Id", v)).toList*) ++
        Headers(version.map(v => Header.Custom("MCP-Protocol-Version", v)).toList*)
      _ <- http.postJson(upstream.url, notice, custom ++ accept ++ sessionHeaders, CallTimeout)
    yield session

  // The JSON-RPC `result` out of a plain JSON body or the `data:` lines of
  // an SSE stream; an `error` becomes Left.
  def rpcResult(reply: HttpReply): Either[String, Json] =
    val messages: List[Json] =
      if reply.headers.get("content-type").exists(_.contains("text/event-stream")) then
        reply.body
          .split("\r?\n")
          .toList
          .filter(_.startsWith("data:"))
          .map(_.drop(5).trim)
          .flatMap(_.fromJson[Json].toOption)
      else reply.json.toOption.toList
    messages.find(m => m.get("result").isDefined || m.get("error").isDefined) match
      case None    => Left(s"no JSON-RPC response in reply: ${reply.body.take(300)}")
      case Some(m) =>
        m.get("result") match
          case Some(result) => Right(result)
          case None => Left(m.get("error").flatMap(_.get("message")).flatMap(_.asString).getOrElse("upstream error"))

  def connectionLost(err: String): Boolean =
    List(
      "No valid session id",
      "Session not found",
      "HTTP 404",
      "Not connected",
      "terminated",
      "onnection refused",
      "closed"
    )
      .exists(err.contains)

// ── 3. gateway ───────────────────────────────────────────────────────────────

final class Gateway(clients: Vector[RemoteClient], val tools: List[Json], routes: Map[String, (Int, String)]):
  def hasTool(name: String): Boolean = routes.contains(name)

  // An upstream failure is a tool error for that call; the gateway stays up.
  def call(name: String, args: Json): IO[RpcError, Json] =
    routes.get(name) match
      case None                    => ZIO.succeed(Results.failure(s"[gateway] unknown tool: $name"))
      case Some((index, original)) =>
        val client = clients(index)
        client
          .call(original, args)
          .catchAll(err =>
            ZIO.succeed(
              Results.failure(s"[gateway] upstream '${client.upstream.name}' failed: ${Results.describe(err)}")
            )
          )

object Gateway:
  val Separator = "__"

  // What OpenAI-compatible clients accept as a tool name; a namespaced name
  // that breaks it is dropped, not surfaced and then rejected mid-session.
  def exposedNameOk(name: String): Boolean = name.length >= 1 && name.length <= 64 && GatewayConfig.identOk(name)

  // Connects every upstream concurrently and lists its tools once (the agent
  // lists tools once; deploys are lockstep). Unreachable upstreams and
  // colliding names are skipped with a log.
  def connect(upstreams: List[Upstream], ownTools: Set[String], http: HttpClient): UIO[Gateway] =
    for
      connected <- ZIO.foreachPar(upstreams) { u =>
        RemoteClient
          .connect(u, http)
          .flatMap(c => c.listTools.map(ts => Some(c -> ts)))
          .catchAll(err =>
            ZIO.logWarning(s"gateway upstream ${u.name} unavailable, skipping: ${Results.describe(err)}").as(None)
          )
      }
      reachable = connected.flatten
      (tools, routes) <- ZIO.foldLeft(reachable.zipWithIndex)((List.empty[Json], Map.empty[String, (Int, String)])) {
        case ((tools, routes), ((client, listed), index)) =>
          ZIO.foldLeft(listed)((tools, routes)) { case ((ts, rs), tool) =>
            val original = tool.get("name").flatMap(_.asString).getOrElse("")
            val exposed = s"${client.upstream.prefix}$Separator$original"
            if !exposedNameOk(exposed) then
              ZIO.logWarning(s"dropping gateway tool $original: invalid exposed name $exposed").as((ts, rs))
            else if rs.contains(exposed) || ownTools.contains(exposed) then
              ZIO.logWarning(s"dropping gateway tool $exposed: name already taken").as((ts, rs))
            else
              val renamed = Json.Obj(tool.fields.map((k, v) => if k == "name" then k -> Json.Str(exposed) else k -> v)*)
              ZIO.succeed((ts :+ renamed, rs + (exposed -> (index, original))))
          }
      }
      _ <- ZIO.logInfo(
        s"gateway aggregated ${tools.size} upstream tools from ${reachable.map(_._1.upstream.name).mkString(", ")}"
      )
    yield Gateway(reachable.map(_._1).toVector, tools, routes)
