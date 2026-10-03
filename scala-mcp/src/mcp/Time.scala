package mcp

// Timestamps as the agent sees them. Every date this server hands out has
// always been a JS `Date#toISOString()` ("2026-10-02T09:00:00.000Z"), and
// every date it accepts went through `new Date(input)`; skills and the
// planner depend on both shapes, so they are reproduced here once.

import java.time.format.DateTimeFormatter
import java.time.{Instant, LocalDate, LocalDateTime, OffsetDateTime, ZoneOffset}
import scala.util.Try

object Time:
  private val IsoMillis = DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH:mm:ss.SSS'Z'").withZone(ZoneOffset.UTC)
  private val SqlFormat = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss").withZone(ZoneOffset.UTC)

  def iso(t: Instant): String = IsoMillis.format(t)

  def isoFromUnix(secs: Long): String = iso(Instant.ofEpochSecond(secs))

  def isoFromUnixMs(ms: Long): String = iso(Instant.ofEpochMilli(ms))

  // "2026-10-02 09:00:00", UTC — sqlite's datetime('now'), which is what
  // created_at / consumed_at have always looked like to the agent.
  def sqlTime(t: Instant): String = SqlFormat.format(t)

  // The inputs `new Date(...)` accepted in practice: full ISO with an offset,
  // ISO without one (the process runs in UTC, so local == UTC), a bare date,
  // and RFC 2822 (RSS pubDate, Gmail Date headers).
  def parseJsDate(input: String): Option[Instant] =
    val s = input.trim
    Try(OffsetDateTime.parse(s).toInstant)
      .orElse(Try(LocalDateTime.parse(s.replace(' ', 'T')).toInstant(ZoneOffset.UTC)))
      .orElse(Try(LocalDate.parse(s).atStartOfDay.toInstant(ZoneOffset.UTC)))
      .orElse(Try(OffsetDateTime.parse(s, DateTimeFormatter.RFC_1123_DATE_TIME).toInstant))
      .toOption

  // For tool filters: an unparseable date is the caller's mistake and must say
  // so, the way `new Date("garbage")` blew up in the query builder.
  def requireJsDate(field: String, input: String): Either[String, Instant] =
    parseJsDate(input).toRight(s"""$field: invalid date "$input"""")

// Every Instant this server serialises goes out in the JS shape.
given zio.json.JsonEncoder[Instant] = zio.json.JsonEncoder.string.contramap(Time.iso)
