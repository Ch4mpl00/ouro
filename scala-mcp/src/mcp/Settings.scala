package mcp

// User-facing settings KV (`settings` table). Today it holds `timezone`, the
// IANA name every cron evaluation and "what day is it" decision reads. Read
// on every tick — a PK lookup on a tiny table — so a `set_timezone` takes
// effect on the next scheduler tick without a restart.
//
// Sections:
//   1. store    — raw get/set
//   2. timezone — validated IANA zone + the user's local wall clock

import io.getquill.*
import zio.*

import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

final case class SettingRow(key: String, value: String, updatedAt: Instant)

// ── 1. store ─────────────────────────────────────────────────────────────────

final class Settings(db: Db):
  private val quill = db.quill
  import quill.*

  private inline def settings = quote(querySchema[SettingRow]("settings"))

  def get(key: String): Task[Option[String]] = run(settings.filter(_.key == lift(key)).map(_.value)).map(_.headOption)

  def set(key: String, value: String): Task[Unit] =
    Clock.instant.flatMap { now =>
      run(
        settings
          .insertValue(lift(SettingRow(key, value, now)))
          .onConflictUpdate(_.key)((t, e) => t.value -> e.value, (t, e) => t.updatedAt -> e.updatedAt)
      ).unit
    }

  // ── 2. timezone ────────────────────────────────────────────────────────────

  // Never fails: an unset, unreadable or (hand-edited) unparseable value
  // falls back to UTC, so a poller tick always has a usable zone.
  def timezone: UIO[ZoneId] =
    get(Settings.TimezoneKey).foldZIO(
      err => ZIO.logWarning(s"reading timezone failed, using UTC: $err").as(Settings.Utc),
      {
        case None       => ZIO.succeed(Settings.Utc)
        case Some(name) =>
          ZIO
            .fromEither(Settings.parseTimezone(name))
            .orElse(ZIO.logWarning(s"stored timezone $name is not a valid IANA name, using UTC").as(Settings.Utc))
      }
    )

  def setTimezone(tz: ZoneId): Task[Unit] = set(Settings.TimezoneKey, tz.getId)

  def localTime(at: Instant): UIO[LocalTime] = timezone.map(Settings.localTime(_, at))

object Settings:
  val TimezoneKey = "timezone"
  val Utc: ZoneId = ZoneId.of("UTC")

  // IANA region names only (and "UTC"), validated before storing so a bogus
  // name never reaches the pollers. ZoneId alone would also take "+03:00".
  def parseTimezone(name: String): Either[String, ZoneId] =
    if name == "UTC" || ZoneId.getAvailableZoneIds.contains(name) then Right(ZoneId.of(name))
    else Left(s"Invalid time zone specified: $name")

  def localTime(tz: ZoneId, at: Instant): LocalTime =
    val local = at.atZone(tz)
    LocalTime(
      local.format(DateTimeFormatter.ofPattern("yyyy-MM-dd")),
      local.format(DateTimeFormatter.ofPattern("HH:mm")),
      tz
    )

final case class LocalTime(
    // YYYY-MM-DD in the user's zone.
    date: String,
    // HH:MM, 24h.
    hm: String,
    tz: ZoneId
):
  // The `local_now` shape the timezone tools return: "2026-10-02 09:05".
  def display: String = s"$date $hm"
