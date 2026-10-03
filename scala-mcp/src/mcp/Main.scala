package mcp

// Composition root of the MCP server — the only place that knows the whole
// graph. Builds every long-lived dependency once, threads it into the tool
// handler and the pollers, and owns shutdown: everything long-running is a
// fiber in the app's scope, interrupted on SIGTERM.

import java.nio.file.Path

import zio.*
import zio.http.Client
import zio.telemetry.opentelemetry.tracing.Tracing

object Main extends ZIOAppDefault:
  private val DefaultGatewayConfig = "crates/mcp/gateway.config.json"
  private val DefaultAllowedHosts = "localhost,127.0.0.1,::1"

  // Logs go to stderr: on the stdio transport stdout is the MCP channel.
  override val bootstrap: ZLayer[ZIOAppArgs, Any, Any] = Cli.logging

  def run: ZIO[ZIOAppArgs & Scope, Any, Any] =
    server.provideSome[Scope](Client.default, Fetcher.layer, Telemetry.layer)

  private def server: ZIO[Scope & Client & Fetcher & Tracing, Throwable, Unit] =
    for
      _ <- Env.loadFiles(".env")
      // MCP_TOOLSETS narrows the surface for an instance serving one audience
      // (the ChatGPT tunnel). Unset → everything.
      selection <- ZIO.fromEither(ToolsetSelection.parse(Env.get("MCP_TOOLSETS"))).mapError(RuntimeException(_))
      _ <- ZIO.when(selection.restricted)(ZIO.logInfo(s"restricted tool surface: ${selection.names.map(_.name).mkString(",")}"))
      // MCP_NO_POLLERS: tools only. The Telegram getUpdates poll is exclusive
      // per bot, so a second poller against the same bot would 409 the droplet.
      pollers = !Env.get("MCP_NO_POLLERS").contains("1")

      http <- ZIO.serviceWith[Client](HttpClient.following)
      // Both databases must be up and migrated before any handler or poller
      // touches them: the news / memory store, and `mcp_state` beside it.
      newsUrl <- PgUrl.fromEnv("DATABASE_URL").mapError(RuntimeException(_))
      pool <- PgPool.make(newsUrl)
      _ <- Migrations.migrateNews(pool)
      db <- Db.connect(newsUrl)
      embedder <- OpenAiEmbedder.fromEnv(http).mapError(RuntimeException(_))
      news = NewsRepository(pool, embedder)
      settings = Settings(db)
      signals = Signals(db)
      scheduler = ScheduledTasks(db, settings)
      telegram <- TelegramModule.make(TelegramConfig.fromEnv, http, db)
      gmail = GmailModule(db, http)
      userbot <- Userbot.make(db)
      fetcher <- ZIO.service[Fetcher]
      deps = Deps(
        settings = settings,
        signals = signals,
        scheduler = scheduler,
        telegram = telegram,
        gmail = gmail,
        monobank = Monobank.fromEnv(http),
        userbot = userbot,
        skills = SkillCatalog(Path.of("skills"), Path.of("skills.default")),
        fetcher = fetcher,
        storageDir = Path.of(Env.get("STORAGE_DIR").getOrElse("./storage")),
        news = news,
        knowledge = KnowledgeRepository(pool, embedder),
        memory = MemoryService.make(PgMemoryStore(pool), embedder),
        // Who this instance writes to shared memory as — a property of the
        // instance, not something a client declares.
        memoryActor = Env.get("MCP_MEMORY_ACTOR").getOrElse("mcp"),
      )
      tools = Toolset.compose(selection.names)
      // A restricted instance never fronts upstreams: their tools arrive
      // namespaced at runtime and can't be expressed in the allow-list.
      gateway <-
        if selection.restricted then ZIO.none
        else
          GatewayConfig
            .load(Path.of(Env.get("GATEWAY_CONFIG").getOrElse(DefaultGatewayConfig)), Env.get)
            .flatMap(upstreams =>
              if upstreams.isEmpty then ZIO.none else Gateway.connect(upstreams, tools.map(_.name).toSet, http).map(Some(_))
            )
      tracing <- ZIO.service[Tracing]
      handler = McpHandler(deps, tools, gateway, tracing)

      // Keep-alives behind start_typing / telegram_send_status: part of the
      // tools, not pollers, so they run on every instance.
      _ <- telegram.typing.run.forkScoped
      _ <- telegram.status.run.forkScoped
      _ <-
        if pollers then
          val providers = List(HackerNews(http), Habr(http), TelegramChannels(userbot, news))
          TelegramPoller.run(telegram.bot, telegram.log, signals, telegram.config).forkScoped *>
            gmail.runPoller(signals).forkScoped *>
            SchedulerPoller.run(scheduler, signals).forkScoped *>
            NewsPoller.run(providers, news).forkScoped
        else ZIO.logInfo("MCP_NO_POLLERS=1 — tools only, pollers disabled")

      _ <- Env.get("MCP_TRANSPORT").getOrElse("stdio").toLowerCase match
        case "http" =>
          for
            port <- ZIO
              .fromOption(Env.get("MCP_PORT").fold(Some(3000))(_.toIntOption))
              .orElseFail(RuntimeException("MCP_PORT must be a port number"))
            // "*" turns Host validation off — for a listener only reachable
            // inside the compose network, where the caller's Host header isn't
            // ours to predict (tunnel-client).
            raw = Env.get("MCP_ALLOWED_HOSTS").getOrElse(DefaultAllowedHosts)
            allowed = if raw.trim == "*" then Nil else raw.split(',').map(_.trim).filter(_.nonEmpty).toList
            _ <- Transport.serveHttp(handler, Transport.HttpOptions(port, allowed, multiSession = selection.restricted))
          yield ()
        case _ => Transport.serveStdio(handler)
    yield ()
