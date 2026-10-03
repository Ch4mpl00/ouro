package mcp

// The MCP endpoint: the tool vocabulary every domain file declares its tools
// with, the JSON-RPC protocol over them, and the two transports.
//
// Sections:
//   1. deps      — everything a tool handler may reach, built once in Main
//   2. tools     — `ToolDef`, the `tool(...)` constructor, input schemas
//   3. results   — the JSON-text result shape and how failures surface
//   4. protocol  — JSON-RPC: initialize, tools/list, tools/call (+ gateway)
//   5. sessions  — "newest wins" policy for the single-session instance
//   6. transport — stdio and Streamable HTTP (Tapir on ZIO HTTP)
//
// The protocol is implemented here rather than through an SDK: the JVM has
// no ZIO-native MCP library, and the server side of it is small — three
// methods, JSON-RPC framing, a session header.

import java.nio.file.Path

import sttp.model.StatusCode
import sttp.tapir.{Schema, SchemaType, Validator}
import sttp.tapir.ztapir.*
import sttp.tapir.server.ziohttp.ZioHttpInterpreter
import zio.*
import zio.json.*
import zio.json.ast.Json
import zio.stream.{ZPipeline, ZStream}
import zio.telemetry.opentelemetry.tracing.Tracing

// Optional fields serialize as `null`, like serde did; the agent and the
// skills read `signal: null` and friends.
given JsonCodecConfiguration = JsonCodecConfiguration(explicitNulls = true)

// ── 1. deps ──────────────────────────────────────────────────────────────────

// Built once in the composition root and shared by every session. A
// handler's reach is exactly this — no globals, no service locators.
final case class Deps(
    settings: Settings,
    signals: Signals,
    scheduler: ScheduledTasks,
    telegram: TelegramModule,
    gmail: GmailModule,
    monobank: Monobank,
    userbot: Userbot,
    skills: SkillCatalog,
    // The SSRF-guarded client behind fetch_url (Fetch.scala).
    fetcher: Fetcher,
    // Where downloaded attachments land (STORAGE_DIR, default ./storage).
    storageDir: Path,
    news: NewsRepository,
    knowledge: KnowledgeRepository,
    memory: MemoryService,
    // Stamped onto every memory write: who the instance writes as. Audit
    // metadata, never access control (one shared space).
    memoryActor: String,
)

// ── 2. tools ─────────────────────────────────────────────────────────────────

// A tool is plain data: its listing and a function of the deps and the raw
// arguments. Domain files declare them as values, so a toolset's surface is
// known (and tested) without building any dependency.
final case class ToolDef(
    name: String,
    title: String,
    description: String,
    inputSchema: Json.Obj,
    run: (Deps, Json) => Task[Json],
):
  def listing: Json = Json.Obj(
    "name" -> Json.Str(name),
    "title" -> Json.Str(title),
    "description" -> Json.Str(description),
    "inputSchema" -> inputSchema,
  )

// Bad arguments: a JSON-RPC error (-32602), not a tool failure — the client
// sent something the schema forbids.
final class InvalidParams(message: String) extends Exception(message)

// A tool failing on its own terms (an API error, a missing file): becomes an
// `isError` result the model can read.
final class ToolFailure(message: String) extends Exception(message)

final case class NoArgs() derives JsonDecoder, Schema

object Tools:
  // `tool(name, title, description) { (deps, args: Params) => ... }` — the
  // argument type drives decoding and the advertised JSON Schema; the result
  // type's encoder renders the reply.
  def tool[A: JsonDecoder: Schema, R: JsonEncoder](name: String, title: String, description: String)(
      run: (Deps, A) => Task[R]
  ): ToolDef =
    ToolDef(
      name,
      title,
      description,
      InputSchema.of[A],
      (deps, raw) =>
        ZIO
          .fromEither(raw.as[A])
          .mapError(err => InvalidParams(s"failed to deserialize parameters: $err"))
          .flatMap(args => run(deps, args))
          .flatMap(out => ZIO.fromEither(out.toJsonAST).mapError(err => RuntimeException(err))),
    )

  def fail(message: String): IO[ToolFailure, Nothing] = ZIO.fail(ToolFailure(message))

  def invalid(message: String): IO[InvalidParams, Nothing] = ZIO.fail(InvalidParams(message))

  // Either[String, A] from a validation → a tool failure carrying the message.
  def orFail[A](either: Either[String, A]): IO[ToolFailure, A] = ZIO.fromEither(either).mapError(ToolFailure(_))

// Tool input schemas, derived from the parameter case class's Tapir Schema
// (field docs via @description, bounds via @validate).
object InputSchema:
  def of[A](using schema: Schema[A]): Json.Obj = render(schema) match
    case obj: Json.Obj => obj
    case _ => Json.Obj("type" -> Json.Str("object"))

  private def render(schema: Schema[?]): Json =
    val base: List[(String, Json)] = schema.schemaType match
      case SchemaType.SString() => List("type" -> Json.Str("string"))
      case SchemaType.SInteger() => List("type" -> Json.Str("integer"))
      case SchemaType.SNumber() => List("type" -> Json.Str("number"))
      case SchemaType.SBoolean() => List("type" -> Json.Str("boolean"))
      case SchemaType.SOption(inner) => fields(render(inner))
      case SchemaType.SArray(inner) => List("type" -> Json.Str("array"), "items" -> render(inner))
      case p: SchemaType.SProduct[?] =>
        val props = p.fields.map(f => f.name.encodedName -> render(f.schema))
        val required = p.fields.filterNot(_.schema.isOptional).map(f => Json.Str(f.name.encodedName))
        List("type" -> Json.Str("object"), "properties" -> Json.Obj(props*)) ++
          (if required.isEmpty then Nil else List("required" -> Json.Arr(required*)))
      case SchemaType.SOpenProduct(_, _) => List("type" -> Json.Str("object"))
      case c: SchemaType.SCoproduct[?] => List("anyOf" -> Json.Arr(c.subtypes.map(render)*))
      case _ => Nil
    val doc = schema.description.map(d => "description" -> Json.Str(d)).toList
    Json.Obj((base ++ doc ++ constraints(schema.validator))*)

  private def fields(json: Json): List[(String, Json)] = json match
    case Json.Obj(fs) => fs.toList
    case _ => Nil

  private def constraints(v: Validator[?]): List[(String, Json)] = v match
    case Validator.All(vs) => vs.toList.flatMap(constraints)
    case Validator.Min(value, _) => List("minimum" -> number(value))
    case Validator.Max(value, _) => List("maximum" -> number(value))
    case Validator.MinLength(n, _) => List("minLength" -> Json.Num(n))
    case Validator.MaxLength(n, _) => List("maxLength" -> Json.Num(n))
    case Validator.MinSize(n) => List("minItems" -> Json.Num(n))
    case Validator.MaxSize(n) => List("maxItems" -> Json.Num(n))
    case e: Validator.Enumeration[?] => List("enum" -> Json.Arr(e.possibleValues.map(x => Json.Str(x.toString))*))
    case _ => Nil

  private def number(value: Any): Json = value match
    case n: Int => Json.Num(n)
    case n: Long => Json.Num(n)
    case n: Double => Json.Num(n)
    case other => Json.Str(other.toString)

// ── 3. results ───────────────────────────────────────────────────────────────

object Results:
  // A single text block of pretty JSON — what every tool has always returned,
  // and what clients without structured-content support can still read.
  def success(value: Json): Json = result(value.toJsonPretty, isError = false)

  // A failure inside a handler comes back as an `isError` result carrying
  // the message: the model reads it and adapts; a JSON-RPC error would be
  // invisible to it.
  def failure(message: String): Json = result(message, isError = true)

  def text(text: String, isError: Boolean): Json = result(text, isError)

  private def result(text: String, isError: Boolean): Json = Json.Obj(
    "content" -> Json.Arr(Json.Obj("type" -> Json.Str("text"), "text" -> Json.Str(text))),
    "isError" -> Json.Bool(isError),
  )

  // "outer: cause: root" — the whole chain, like anyhow's `{:#}`.
  def describe(err: Throwable): String =
    Iterator
      .iterate(err)(_.getCause)
      .takeWhile(_ != null)
      .take(5)
      .map(e => Option(e.getMessage).getOrElse(e.getClass.getSimpleName))
      .toList
      .distinct
      .mkString(": ")

// ── 4. protocol ──────────────────────────────────────────────────────────────

final case class RpcError(code: Int, message: String)

object RpcError:
  val ParseError = -32700
  val InvalidRequest = -32600
  val MethodNotFound = -32601
  val InvalidParamsCode = -32602
  val Internal = -32603

// Own tools come first and win any name collision; gateway tools follow,
// already namespaced (`tavily__tavily_search`). The agent sees one list.
final class McpHandler(deps: Deps, tools: List[ToolDef], gateway: Option[Gateway], tracing: Tracing):
  private val byName = tools.map(t => t.name -> t).toMap

  def toolNames: List[String] = tools.map(_.name)

  // One JSON-RPC message in; a response out, or None for a notification (and
  // for a client's response to us — this server never asks anything).
  def handle(message: Json): UIO[Option[Json]] = message match
    case obj: Json.Obj =>
      val id = obj.get("id").filterNot(_ == Json.Null)
      val method = obj.get("method").flatMap(_.asString)
      val params = obj.get("params").getOrElse(Json.Obj())
      (id, method) match
        case (Some(id), Some(m)) => dispatch(m, params).either.map(reply => Some(McpHandler.response(id, reply)))
        case (None, Some(_)) => ZIO.none
        case (Some(_), None) => ZIO.none
        case (None, None) =>
          ZIO.some(McpHandler.response(Json.Null, Left(RpcError(RpcError.InvalidRequest, "invalid request"))))
    case _ => ZIO.some(McpHandler.response(Json.Null, Left(RpcError(RpcError.InvalidRequest, "invalid request"))))

  private def dispatch(method: String, params: Json): IO[RpcError, Json] = method match
    case "initialize" => ZIO.succeed(McpHandler.initializeResult(params))
    case "ping" => ZIO.succeed(Json.Obj())
    case "tools/list" => ZIO.succeed(Json.Obj("tools" -> Json.Arr((ownListings ++ gatewayListings)*)))
    case "tools/call" => call(params)
    case other => ZIO.fail(RpcError(RpcError.MethodNotFound, s"method not found: $other"))

  private val ownListings: List[Json] = tools.map(_.listing)
  private def gatewayListings: List[Json] = gateway.toList.flatMap(_.tools)

  private def call(params: Json): IO[RpcError, Json] =
    val name = params.get("name").flatMap(_.asString).getOrElse("")
    val args = params.get("arguments").filterNot(_ == Json.Null).getOrElse(Json.Obj())
    byName.get(name) match
      case Some(tool) => runOwn(tool, args)
      case None =>
        gateway.filter(_.hasTool(name)) match
          case Some(gw) => gw.call(name, args)
          case None => ZIO.fail(RpcError(RpcError.InvalidParamsCode, "tool not found"))

  private def runOwn(tool: ToolDef, args: Json): IO[RpcError, Json] =
    tracing
      .span(s"tools/call ${tool.name}")(tool.run(deps, args))
      .map(Results.success)
      .catchAll {
        case err: InvalidParams => ZIO.fail(RpcError(RpcError.InvalidParamsCode, err.getMessage))
        case err =>
          val text = Results.describe(err)
          ZIO.logWarning(s"tool ${tool.name} failed: $text").as(Results.failure(text))
      }
      .catchAllDefect(defect => ZIO.logError(s"tool ${tool.name} crashed: $defect").as(Results.failure(s"$defect")))

object McpHandler:
  val ServerName = "mcp-tools"
  val ServerVersion = "0.1.0"
  private val Supported = List("2025-06-18", "2025-03-26", "2024-11-05")

  // Echo the client's protocol version when we speak it, else offer ours.
  def initializeResult(params: Json): Json =
    val requested = params.get("protocolVersion").flatMap(_.asString)
    Json.Obj(
      "protocolVersion" -> Json.Str(requested.filter(Supported.contains).getOrElse(Supported.head)),
      "capabilities" -> Json.Obj("tools" -> Json.Obj()),
      "serverInfo" -> Json.Obj("name" -> Json.Str(ServerName), "version" -> Json.Str(ServerVersion)),
    )

  def response(id: Json, reply: Either[RpcError, Json]): Json = reply match
    case Right(result) => Json.Obj("jsonrpc" -> Json.Str("2.0"), "id" -> id, "result" -> result)
    case Left(err) =>
      Json.Obj(
        "jsonrpc" -> Json.Str("2.0"),
        "id" -> id,
        "error" -> Json.Obj("code" -> Json.Num(err.code), "message" -> Json.Str(err.message)),
      )

  def isInitialize(message: Json): Boolean = message.get("method").flatMap(_.asString).contains("initialize")

extension (json: Json)
  // Field lookup on an object; None for anything else.
  def get(key: String): Option[Json] = json match
    case Json.Obj(fields) => fields.collectFirst { case (k, v) if k == key => v }
    case _ => None

// ── 5. sessions ──────────────────────────────────────────────────────────────

// The full instance serves exactly one client — the supervisor — and a fresh
// `initialize` means the previous one is gone (it restarted after an unclean
// death and never closed its session). So the newest session evicts the rest
// instead of being refused: refusing is what crash-looped the TS server in
// production (2026-06-15, 2026-08-23 — 78 restarts).
//
// A restricted instance (MCP_TOOLSETS set) has no signal delivery to race on
// and must hold several sessions at once — tunnel-client keeps a probe session
// of its own, so a ChatGPT client is always the second connection.
final class Sessions(live: Ref[Set[String]], newestWins: Boolean):
  def create: UIO[String] =
    for
      id <- Random.nextUUID.map(_.toString)
      evicted <- live.modify(old => (old, if newestWins then Set(id) else old + id))
      _ <- ZIO.foreachDiscard(evicted.filter(_ => newestWins))(old =>
        ZIO.logInfo(s"evicting previous session $old: newest wins")
      )
    yield id

  def exists(id: String): UIO[Boolean] = live.get.map(_.contains(id))

  def close(id: String): UIO[Boolean] = live.modify(s => (s.contains(id), s - id))

object Sessions:
  def make(newestWins: Boolean): UIO[Sessions] = Ref.make(Set.empty[String]).map(Sessions(_, newestWins))

// ── 6. transport ─────────────────────────────────────────────────────────────

object Transport:
  // Newline-delimited JSON-RPC on stdin/stdout. Logs go to stderr (Main),
  // because stdout is the channel. Requests run concurrently, like rmcp.
  def serveStdio(handler: McpHandler): Task[Unit] =
    ZStream
      .fromInputStream(java.lang.System.in)
      .via(ZPipeline.utf8Decode >>> ZPipeline.splitLines)
      .filter(_.trim.nonEmpty)
      .mapZIOParUnordered(16)(line => handleLine(handler, line))
      .collectSome
      .foreach(out => ZIO.succeed { java.lang.System.out.println(out); java.lang.System.out.flush() })

  private def handleLine(handler: McpHandler, line: String): UIO[Option[String]] =
    line.fromJson[Json] match
      case Left(err) =>
        ZIO.some(McpHandler.response(Json.Null, Left(RpcError(RpcError.ParseError, s"parse error: $err"))).toJson)
      case Right(json) => handler.handle(json).map(_.map(_.toJson))

  final case class HttpOptions(
      port: Int,
      // Hosts (`host` or `host:port`) accepted in the Host header — the
      // DNS-rebinding guard. Empty = no validation.
      allowedHosts: List[String],
      multiSession: Boolean,
  )

  private type Reply = (StatusCode, Option[String], String)

  private def rpcFailure(status: StatusCode, message: String): Reply =
    val body = McpHandler.response(Json.Null, Left(RpcError(-32000, message))).toJson
    (status, None, body)

  def hostAllowed(allowed: List[String], host: Option[String]): Boolean =
    allowed.isEmpty || host.exists { h =>
      val bare = if h.startsWith("[") then h.takeWhile(_ != ']') + "]" else h.takeWhile(_ != ':')
      allowed.contains(h) || allowed.contains(bare)
    }

  // POST carries every client message. JSON responses, not SSE: the spec
  // allows either, and this server never streams progress or asks back.
  def routes(handler: McpHandler, sessions: Sessions, options: HttpOptions) =
    val post = endpoint.post
      .in("mcp")
      .in(header[Option[String]]("Host"))
      .in(header[Option[String]]("Mcp-Session-Id"))
      .in(stringBody)
      .out(statusCode)
      .out(header[Option[String]]("Mcp-Session-Id"))
      .out(stringJsonBody)
      .zServerLogic[Any] { case (host, sessionId, body) =>
        handlePost(handler, sessions, options, host, sessionId, body)
      }

    val delete = endpoint.delete
      .in("mcp")
      .in(header[Option[String]]("Host"))
      .in(header[Option[String]]("Mcp-Session-Id"))
      .out(statusCode)
      .zServerLogic[Any] { case (host, sessionId) =>
        if !hostAllowed(options.allowedHosts, host) then ZIO.succeed(StatusCode.Forbidden)
        else
          ZIO
            .foreach(sessionId)(sessions.close)
            .map(closed => if closed.contains(true) then StatusCode.Ok else StatusCode.NotFound)
      }

    // No server-initiated stream: nothing here ever pushes.
    val get = endpoint.get.in("mcp").out(statusCode).zServerLogic[Any](_ => ZIO.succeed(StatusCode.MethodNotAllowed))

    ZioHttpInterpreter().toHttp(List(post, delete, get))

  private def handlePost(
      handler: McpHandler,
      sessions: Sessions,
      options: HttpOptions,
      host: Option[String],
      sessionId: Option[String],
      body: String,
  ): UIO[Reply] =
    if !hostAllowed(options.allowedHosts, host) then
      ZIO.succeed(rpcFailure(StatusCode.Forbidden, "Forbidden: Host header is not allowed"))
    else
      body.fromJson[Json] match
        case Left(err) =>
          ZIO.succeed(
            (StatusCode.BadRequest, None, McpHandler.response(Json.Null, Left(RpcError(RpcError.ParseError, err))).toJson)
          )
        case Right(message) =>
          sessionId match
            case None if McpHandler.isInitialize(message) =>
              for
                id <- sessions.create
                reply <- handler.handle(message)
              yield (StatusCode.Ok, Some(id), reply.fold("")(_.toJson))
            case None => ZIO.succeed(rpcFailure(StatusCode.BadRequest, "Bad Request: Mcp-Session-Id header is required"))
            case Some(id) =>
              sessions.exists(id).flatMap {
                case false => ZIO.succeed(rpcFailure(StatusCode.NotFound, "Session not found"))
                case true =>
                  handler.handle(message).map {
                    case Some(reply) => (StatusCode.Ok, Some(id), reply.toJson)
                    case None => (StatusCode.Accepted, Some(id), "")
                  }
              }

  def serveHttp(handler: McpHandler, options: HttpOptions): Task[Unit] =
    for
      sessions <- Sessions.make(newestWins = !options.multiSession)
      _ <- ZIO.logInfo(s"mcp http listening on :${options.port} (multi_session=${options.multiSession})")
      _ <- zio.http.Server
        .serve(routes(handler, sessions, options))
        .provide(zio.http.Server.defaultWithPort(options.port))
    yield ()
