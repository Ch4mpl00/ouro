package mcp

// Shared plumbing for the CLI entry points in bin/: env files, logging,
// `--flag value` arguments, and the default locations they all agree on.

import zio.*
import zio.http.Client
import zio.logging.ConsoleLoggerConfig
import zio.logging.LogFilter
import zio.logging.LogFormat
import zio.logging.consoleErrLogger

object Cli:
  val EvalDir = "crates/mcp/eval"

  // Everything logs to stderr at info, like RUST_LOG=info did.
  val logging: ZLayer[Any, Nothing, Unit] =
    Runtime.removeDefaultLoggers >>>
      consoleErrLogger(ConsoleLoggerConfig(LogFormat.default, LogFilter.LogLevelByNameConfig(LogLevel.Info)))

  // `.env` like the server, plus `.env.mcp` — the CLIs run on a dev machine
  // where the container env isn't injected.
  val init: UIO[Unit] = Env.loadFiles(".env", ".env.mcp")

  def arg(args: Chunk[String], name: String): Option[String] =
    args.indexOf(s"--$name") match
      case -1 => None
      case i  => args.lift(i + 1)

  def intArg(args: Chunk[String], name: String, default: Int): Task[Int] =
    arg(args, name).fold(ZIO.succeed(default))(raw =>
      ZIO.fromOption(raw.toIntOption).orElseFail(RuntimeException(s"--$name must be a number, got $raw"))
    )

  def prompt(message: String): Task[String] = Console.print(message) *> Console.readLine.map(_.trim)

  def http: ZIO[Client, Nothing, HttpClient] = ZIO.serviceWith[Client](HttpClient.following)

  // The state database (`mcp_state`, next to the news store) — created and
  // migrated on open, like the server does.
  def openDb: ZIO[Scope, Throwable, Db] =
    PgUrl.fromEnv("DATABASE_URL").mapError(RuntimeException(_)).flatMap(Db.connect)

  // The news / memory store, migrated.
  def openPool: ZIO[Scope, Throwable, PgPool] =
    for
      url <- PgUrl.fromEnv("DATABASE_URL").mapError(RuntimeException(_))
      pool <- PgPool.make(url)
      _ <- Migrations.migrateNews(pool)
    yield pool

  def embedder: ZIO[Client, Throwable, OpenAiEmbedder] =
    http.flatMap(OpenAiEmbedder.fromEnv(_).mapError(RuntimeException(_)))

// A CLI: init, logging and an HTTP client provided; the body is the program.
trait CliApp extends ZIOAppDefault:
  def program(args: Chunk[String]): ZIO[Scope & Client, Throwable, Any]

  override val bootstrap: ZLayer[ZIOAppArgs, Any, Any] = Cli.logging

  def run: ZIO[ZIOAppArgs & Scope, Any, Any] =
    Cli.init *> getArgs.flatMap(args => program(args).provideSome[Scope](Client.default))
