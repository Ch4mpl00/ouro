package mcp

// MCP-owned state: the `mcp_state` database in the same Postgres cluster as
// the news store. OAuth tokens and the userbot session, the signal queue,
// scheduled tasks, settings, the Telegram chat log and poller cursors.
//
// Sections:
//   1. handle — `Db`, an injected pool on `mcp_state`
//   2. locate — which database, and creating it on first boot
//   3. schema — Flyway (resources/db/state), adopting a Rust-made database
//
// A separate database rather than tables next to news_items: different
// lifecycle (small, hot, transactional vs. large and append-mostly), and it
// can be backed up or moved on its own.
//
// The schema (V1) and the system-task seed (V2) are the Rust server's
// migration and seed, as Flyway files. The Rust server recorded its version in
// `state_migrations`; a database that has it is baselined at V2.

import java.sql.{DriverManager, SQLException}

import zio.*

// ── 1. handle ────────────────────────────────────────────────────────────────

final class Db(val pool: PgPool):
  export pool.quill

object Db:
  // Creates the database if missing, migrates and seeds it.
  def connect(newsUrl: PgUrl): ZIO[Scope, Throwable, Db] =
    for
      url <- stateUrl(newsUrl)
      _ <- ensureDatabase(newsUrl, url.database)
      db <- open(url, schema = None)
    yield db

  def open(url: PgUrl, schema: Option[String]): ZIO[Scope, Throwable, Db] =
    for
      pool <- PgPool.make(url, maxSize = 5)
      rust <- pool.withConnection(c => Migrations.countIfExists(c, "state_migrations"))
      // Version 1 there is schema + seed here.
      applied <- Migrations.run(pool.ds, "db/state", baseline = rust.map(_ => 2), schema = schema)
      _ <- ZIO.when(applied > 0)(ZIO.logInfo(s"state migrations applied: $applied"))
    yield Db(pool)

  // ── 2. locate ──────────────────────────────────────────────────────────────

  val DefaultName = "mcp_state"

  // STATE_DATABASE_URL wins; otherwise DATABASE_URL with the database name
  // swapped, so compose needs no extra secret.
  def stateUrl(newsUrl: PgUrl): Task[PgUrl] =
    Env.get("STATE_DATABASE_URL") match
      case Some(raw) => ZIO.fromEither(PgUrl.parse(raw)).mapError(err => RuntimeException(s"STATE_DATABASE_URL: $err"))
      case None => ZIO.succeed(newsUrl.withDatabase(DefaultName))

  // CREATE DATABASE through the news database's connection. Both MCP
  // instances may try at once; the loser's duplicate_database is success.
  private def ensureDatabase(admin: PgUrl, name: String): Task[Unit] =
    ZIO.scoped {
      ZIO
        .fromAutoCloseable(ZIO.attemptBlocking {
          DriverManager.getConnection(admin.jdbcUrl, admin.user.orNull, admin.password.orNull)
        })
        .flatMap { c =>
          ZIO.attemptBlocking {
            val check = c.prepareStatement("SELECT 1 FROM pg_database WHERE datname = ?")
            check.setString(1, name)
            if check.executeQuery().next() then false
            else
              try
                c.createStatement().execute(s"""CREATE DATABASE "${name.replace("\"", "\"\"")}"""")
                true
              catch case e: SQLException if e.getSQLState == "42P04" => false
          }
        }
        .flatMap(created => ZIO.when(created)(ZIO.logInfo(s"created state database $name")).unit)
    }
