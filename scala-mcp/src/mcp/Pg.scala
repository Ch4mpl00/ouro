package mcp

// Postgres: the news / RAG store and unified memory.
//
// Sections:
//   1. urls    — libpq URLs (what compose hands us) → JDBC
//   2. pool    — Hikari + Quill, built once in the composition root
//   3. migrate — Flyway, adopting a database drizzle (TS) or Rust migrated
//   4. vectors — pgvector values as text literals
//
// The migration files are the Rust crate's, byte for byte, renamed to
// Flyway's V<n>__ scheme. A database the TS or Rust server already migrated
// carries drizzle's journal (`drizzle.__drizzle_migrations`) instead of
// Flyway's history; Flyway adopts it with a baseline at the journal's length,
// so nothing already applied runs twice.

import com.zaxxer.hikari.HikariConfig
import com.zaxxer.hikari.HikariDataSource
import io.getquill.SnakeCase
import io.getquill.jdbczio.Quill
import org.flywaydb.core.Flyway
import zio.*

import java.net.URI
import java.net.URLDecoder
import java.nio.charset.StandardCharsets.UTF_8
import java.sql.Connection
import java.sql.PreparedStatement
import java.sql.ResultSet
import java.sql.Types
import java.time.Instant
import java.time.OffsetDateTime
import java.time.ZoneOffset
import javax.sql.DataSource

// ── 1. urls ──────────────────────────────────────────────────────────────────

final case class PgUrl(
    host: String,
    port: Int,
    database: String,
    user: Option[String],
    password: Option[String],
    // Extra JDBC properties (currentSchema for tests).
    options: Map[String, String] = Map.empty
):
  def jdbcUrl: String =
    val query = options.map((k, v) => s"$k=$v").mkString("&")
    s"jdbc:postgresql://$host:$port/$database" + (if query.isEmpty then "" else s"?$query")

  def withDatabase(name: String): PgUrl = copy(database = name)

object PgUrl:
  // postgres://user:pass@host:5432/db — what DATABASE_URL has always been.
  def parse(raw: String): Either[String, PgUrl] =
    scala.util
      .Try(URI(raw.trim))
      .toEither
      .left
      .map(_.getMessage)
      .flatMap { uri =>
        if uri.getScheme != "postgres" && uri.getScheme != "postgresql" then
          Left(s"not a postgres:// URL: ${uri.getScheme}")
        else
          val userInfo = Option(uri.getRawUserInfo).map(_.split(":", 2).map(URLDecoder.decode(_, UTF_8)))
          Right(
            PgUrl(
              host = Option(uri.getHost).getOrElse("localhost"),
              port = if uri.getPort > 0 then uri.getPort else 5432,
              database = Option(uri.getPath).map(_.stripPrefix("/")).filter(_.nonEmpty).getOrElse("postgres"),
              user = userInfo.flatMap(_.headOption),
              password = userInfo.flatMap(_.lift(1))
            )
          )
      }

  def fromEnv(name: String): IO[String, PgUrl] =
    ZIO
      .fromOption(Env.get(name))
      .orElseFail(
        s"$name is not set. The mcp container needs Postgres for the news/RAG store; see .env.postgres.example."
      )
      .flatMap(raw => ZIO.fromEither(parse(raw)).mapError(err => s"$name: $err"))

// ── 2. pool ──────────────────────────────────────────────────────────────────

// One connection pool and the Quill context over it. Domains `import
// pool.quill.*` to write queries; raw SQL goes through `sql"..."` there too.
final class PgPool(val ds: HikariDataSource):
  val quill: Quill.Postgres[SnakeCase] = new Quill.Postgres(SnakeCase, ds)

  // A plain JDBC connection for the few things Quill has no word for: DDL,
  // advisory locks, pgvector operators (`<=>` over a `::vector` cast).
  def withConnection[A](f: Connection => A): Task[A] =
    ZIO.scoped(ZIO.fromAutoCloseable(ZIO.attemptBlocking(ds.getConnection)).flatMap(c => ZIO.attemptBlocking(f(c))))

  // One transaction: commit when `f` returns, roll back when it throws.
  def transaction[A](f: Connection => A): Task[A] = withConnection { c =>
    c.setAutoCommit(false)
    try
      val out = f(c)
      c.commit()
      out
    catch
      case e: Throwable =>
        c.rollback()
        throw e
    finally c.setAutoCommit(true)
  }

  def query[A](sql: String, params: Any*)(read: ResultSet => A): Task[List[A]] =
    withConnection(Sql.query(_, sql, params*)(read))

  def update(sql: String, params: Any*): Task[Int] = withConnection(Sql.update(_, sql, params*))

// Positional `?` parameters over JDBC. Option unwraps to its value or NULL,
// Instant binds as timestamptz, a Seq[String] as text[].
object Sql:
  def query[A](c: Connection, sql: String, params: Any*)(read: ResultSet => A): List[A] =
    val st = prepare(c, sql, params)
    try
      val rs = st.executeQuery()
      val out = List.newBuilder[A]
      while rs.next() do out += read(rs)
      out.result()
    finally st.close()

  def update(c: Connection, sql: String, params: Any*): Int =
    val st = prepare(c, sql, params)
    try st.executeUpdate()
    finally st.close()

  private def prepare(c: Connection, sql: String, params: Seq[Any]): PreparedStatement =
    val st = c.prepareStatement(sql)
    params.zipWithIndex.foreach((p, i) => bind(c, st, i + 1, p))
    st

  private def bind(c: Connection, st: PreparedStatement, i: Int, value: Any): Unit = value match
    case None | null => st.setNull(i, Types.NULL)
    case Some(v)     => bind(c, st, i, v)
    case t: Instant  => st.setObject(i, t.atOffset(ZoneOffset.UTC))
    case xs: Seq[?]  => st.setArray(i, c.createArrayOf("text", xs.map(_.toString).toArray[AnyRef]))
    case v           => st.setObject(i, v)

// Nullable column readers for raw-SQL rows: `import Rows.*`.
object Rows:
  extension (rs: ResultSet)
    def optString(col: String): Option[String] = Option(rs.getString(col))
    def optLong(col: String): Option[Long] =
      val v = rs.getLong(col); if rs.wasNull() then None else Some(v)
    def optInt(col: String): Option[Int] =
      val v = rs.getInt(col); if rs.wasNull() then None else Some(v)
    def optDouble(col: String): Option[Double] =
      val v = rs.getDouble(col); if rs.wasNull() then None else Some(v)
    def instant(col: String): Instant = rs.getObject(col, classOf[OffsetDateTime]).toInstant
    def optInstant(col: String): Option[Instant] = Option(rs.getObject(col, classOf[OffsetDateTime])).map(_.toInstant)

object PgPool:
  def make(url: PgUrl, maxSize: Int = 10): ZIO[Scope, Throwable, PgPool] =
    ZIO
      .fromAutoCloseable(ZIO.attemptBlocking {
        val config = HikariConfig()
        config.setJdbcUrl(url.jdbcUrl)
        url.user.foreach(config.setUsername)
        url.password.foreach(config.setPassword)
        config.setMaximumPoolSize(maxSize)
        config.setMinimumIdle(1)
        config.setPoolName(s"pg-${url.database}")
        HikariDataSource(config)
      })
      .map(PgPool(_))

// ── 3. migrate ───────────────────────────────────────────────────────────────

object Migrations:
  // News store: CREATE EXTENSION stays outside the migration files so they
  // never assume the right to run it.
  def migrateNews(pool: PgPool): Task[Unit] =
    for
      _ <- pool.withConnection(_.createStatement().execute("CREATE EXTENSION IF NOT EXISTS vector"))
      drizzle <- pool.withConnection(c => countIfExists(c, "drizzle.__drizzle_migrations"))
      applied <- run(pool.ds, "db/news", baseline = drizzle)
      _ <- ZIO.logInfo(s"pg migrations applied ($applied new), store ready")
    yield ()

  // Flyway runs under its own Postgres advisory lock, so `mcp` and
  // `mcp-tunnel` booting together never race through the same DDL.
  def run(ds: DataSource, location: String, baseline: Option[Int], schema: Option[String] = None): Task[Int] =
    ZIO.attemptBlocking {
      val config = Flyway.configure().dataSource(ds).locations(s"classpath:$location")
      schema.foreach(s => config.schemas(s).defaultSchema(s))
      baseline.foreach(n => config.baselineOnMigrate(true).baselineVersion(n.toString))
      config.load().migrate().migrationsExecuted
    }

  // Rows in a table that may not exist; None when it doesn't (or Flyway has
  // already taken over, in which case there is nothing to adopt).
  def countIfExists(c: Connection, table: String): Option[Int] =
    def exists(name: String) =
      val rs = c.createStatement().executeQuery(s"SELECT to_regclass('$name') IS NOT NULL")
      rs.next() && rs.getBoolean(1)
    if !exists(table) || exists("flyway_schema_history") then None
    else
      val rs = c.createStatement().executeQuery(s"SELECT count(*) FROM $table")
      rs.next()
      Some(rs.getInt(1))

// ── 4. vectors ───────────────────────────────────────────────────────────────

// pgvector accepts and prints `[1,2,3]`; passing it as text with a `::vector`
// cast keeps the driver free of a pgvector-specific type binding.
object Vectors:
  def literal(v: Seq[Float]): String = v.map(formatFloat).mkString("[", ",", "]")

  // 3.0f → "3", like Rust's f32 Display (and what pgvector prints back).
  private def formatFloat(x: Float): String =
    if x == x.floor && !x.isInfinite && math.abs(x) < 1e15 then x.toLong.toString else x.toString

  def parse(s: String): Option[Vector[Float]] =
    val t = s.trim
    if !t.startsWith("[") || !t.endsWith("]") then None
    else
      val inner = t.drop(1).dropRight(1)
      if inner.trim.isEmpty then Some(Vector.empty)
      else
        val parts = inner.split(',').map(p => p.trim.toFloatOption)
        if parts.forall(_.isDefined) then Some(parts.flatten.toVector) else None
