package mcp

// Cron-driven tasks (`scheduled_tasks` table) — the single mechanism for any
// time-triggered signal. User reminders and the system digests/dreaming are
// all rows here; the latter are seeded by a migration (db/state/V2).
//
// Sections:
//   1. storage — task rows: insert / list active / mark fired / delete
//   2. cron    — parse + next-slot math in the user's timezone
//   3. poller  — 30s tick that turns due tasks into signals
//   4. tools   — `scheduler` toolset: schedule/list/cancel + get/set timezone

import com.cronutils.model.Cron
import com.cronutils.model.CronType
import com.cronutils.model.definition.CronDefinitionBuilder
import com.cronutils.model.time.ExecutionTime
import com.cronutils.parser.CronParser
import io.getquill.*
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.json.*

import java.time.Instant
import java.time.ZoneId

// ── 1. storage ───────────────────────────────────────────────────────────────

final case class ScheduledTask(
    id: Long,
    cronExpr: String,
    recurring: Boolean,
    prompt: String,
    // None → signal source 'scheduler' (user-created).
    source: Option[String],
    // Unix seconds of the slot last fired for.
    lastRunAt: Option[Long],
    createdAt: Instant
)

// The row as schedule_task has always returned it: recurring as 0 | 1, the
// raw sqlite shape.
final case class TaskRow(
    id: Long,
    cron_expr: String,
    recurring: Int,
    prompt: String,
    source: Option[String],
    last_run_at: Option[Long],
    created_at: String
) derives JsonEncoder

object TaskRow:
  def of(t: ScheduledTask): TaskRow =
    TaskRow(t.id, t.cronExpr, if t.recurring then 1 else 0, t.prompt, t.source, t.lastRunAt, Time.sqlTime(t.createdAt))

final class ScheduledTasks(db: Db, val settings: Settings):
  private val quill = db.quill
  import quill.*

  private inline def tasks = quote(querySchema[ScheduledTask]("scheduled_tasks"))

  def insert(cronExpr: String, recurring: Boolean, prompt: String, source: Option[String]): Task[ScheduledTask] =
    run(
      tasks
        .insert(
          _.cronExpr -> lift(cronExpr),
          _.recurring -> lift(recurring),
          _.prompt -> lift(prompt),
          _.source -> lift(source)
        )
        .returning(t => t)
    )

  // Tasks that may still fire: every recurring task, plus one-shots that
  // have not fired yet.
  def listActive: Task[List[ScheduledTask]] =
    run(tasks.filter(t => t.recurring || t.lastRunAt.isEmpty).sortBy(_.id)(using Ord.asc))

  def get(id: Long): Task[Option[ScheduledTask]] = run(tasks.filter(_.id == lift(id))).map(_.headOption)

  // For a one-shot this also retires it (listActive filters on NULL).
  def markFired(id: Long, slotUnix: Long): Task[Unit] =
    run(tasks.filter(_.id == lift(id)).update(_.lastRunAt -> lift(Option(slotUnix)))).unit

  def delete(id: Long): Task[Boolean] = run(tasks.filter(_.id == lift(id)).delete).map(_ > 0)

// ── 2. cron ──────────────────────────────────────────────────────────────────

object Cron5:
  // 5 fields (minute hour dom month dow), or 6 with a leading seconds field —
  // what cron-parser and croner accepted.
  private val unix = CronParser(CronDefinitionBuilder.instanceDefinitionFor(CronType.UNIX))
  private val withSeconds = CronParser(CronDefinitionBuilder.instanceDefinitionFor(CronType.SPRING))

  def parse(expr: String): Either[String, Cron] =
    val parser = if expr.trim.split("\\s+").length == 6 then withSeconds else unix
    scala.util
      .Try(parser.parse(expr.trim).validate())
      .toEither
      .left
      .map(e => Option(e.getMessage).getOrElse(e.toString))

  // First slot strictly after `after`, evaluated on the wall clock of `tz`.
  def nextSlot(cron: Cron, tz: ZoneId, after: Instant): Option[Instant] =
    val next = ExecutionTime.forCron(cron).nextExecution(after.atZone(tz))
    if next.isPresent then Some(next.get.toInstant) else None

  def previewNextFires(cron: Cron, tz: ZoneId, count: Int, now: Instant): List[String] =
    Iterator
      .unfold(now)(cursor => nextSlot(cron, tz, cursor).map(next => (next, next)))
      .take(count)
      .map(Time.iso)
      .toList

// ── 3. poller ────────────────────────────────────────────────────────────────

object SchedulerPoller:
  val DefaultSignalSource = "scheduler"

  // The slot a task's next fire is computed from: the slot it last fired
  // for, or — never fired — its creation time.
  private def anchor(task: ScheduledTask): Instant = task.lastRunAt.fold(task.createdAt)(Instant.ofEpochSecond)

  // Fires every task whose next slot (after its anchor) has passed. Restart-
  // safe and never double-fires a slot: the anchor is the *slot* fired for,
  // not the wall clock, so a late tick still advances to the following slot.
  def tick(scheduler: ScheduledTasks, signals: Signals, now: Instant): Task[Int] =
    for
      tz <- scheduler.settings.timezone
      active <- scheduler.listActive
      fired <- ZIO.foreach(active) { task =>
        Cron5.parse(task.cronExpr) match
          case Left(err)   => ZIO.logWarning(s"task ${task.id}: invalid cron ${task.cronExpr} ($err), skipping").as(0)
          case Right(cron) =>
            Cron5.nextSlot(cron, tz, anchor(task)).filterNot(_.isAfter(now)) match
              case None       => ZIO.succeed(0)
              case Some(slot) =>
                // Snapshot before stamping, so skills (dreaming) can scope
                // `since=<previous fire>` from the signal header.
                val previous = task.lastRunAt.map(Time.isoFromUnix)
                val source = task.source.getOrElse(DefaultSignalSource)
                for
                  _ <- scheduler.markFired(task.id, slot.getEpochSecond)
                  _ <- signals.record(source, renderContent(task, previous, slot, now))
                  kind = if task.recurring then "recurring" else "one-shot"
                  _ <- ZIO.logInfo(s"fired scheduled task ${task.id} ($kind) as $source, slot ${Time.iso(slot)}")
                yield 1
      }
    yield fired.sum

  // A header with the cron metadata (source skills parse the lines they
  // need), then the task's prompt verbatim.
  def renderContent(task: ScheduledTask, previous: Option[String], slot: Instant, now: Instant): String =
    List(
      s"Scheduled task #${task.id} fired.",
      s"Cron: ${task.cronExpr}",
      s"Slot: ${Time.iso(slot)}",
      s"Now: ${Time.iso(now)}",
      s"Previous fire: ${previous.getOrElse("never (this is the first run)")}",
      s"Recurring: ${if task.recurring then "yes" else "no (one-shot)"}",
      "",
      task.prompt
    ).mkString("\n")

  def run(scheduler: ScheduledTasks, signals: Signals): UIO[Nothing] =
    val once =
      Clock.instant.flatMap(tick(scheduler, signals, _)).catchAll(e => ZIO.logError(s"scheduler tick failed: $e"))
    scheduler.settings.timezone.flatMap(tz => ZIO.logInfo(s"scheduler poller started (every 30s, $tz)")) *>
      once.repeat(Schedule.spaced(30.seconds)) *> ZIO.never

// ── 4. tools ─────────────────────────────────────────────────────────────────

object SchedulerTools:
  import Tools.*

  final case class ScheduleTaskParams(
      @description("5-field cron expression (e.g. '30 9 * * *' = 09:30 daily).") cron_expr: String,
      @description("true = keep firing on every cron match; false = fire once then deactivate.") recurring: Boolean,
      @description(
        "Free-text instruction delivered to the agent when the task fires. The agent interprets it under the " +
          "`scheduler` skill (e.g. 'remind me to take pills')."
      ) prompt: String
  ) derives JsonDecoder,
        Schema

  final case class CancelTaskParams(
      @description("Task id from list_scheduled_tasks.") @validate(Validator.min(1L)) id: Long
  ) derives JsonDecoder,
        Schema

  final case class SetTimezoneParams(@description("IANA timezone name, e.g. 'Europe/Kiev'.") tz: String)
      derives JsonDecoder,
        Schema

  final case class Failure(ok: Boolean, error: String) derives JsonEncoder
  final case class Scheduled(ok: Boolean, task: TaskRow, timezone: String, upcoming_fires: List[String])
      derives JsonEncoder
  final case class ListedTask(
      id: Long,
      cron_expr: String,
      recurring: Boolean,
      prompt: String,
      source: Option[String],
      last_run_at: Option[Long],
      created_at: String,
      last_run_at_iso: Option[String],
      upcoming_fires: List[String]
  ) derives JsonEncoder
  final case class Listed(timezone: String, count: Int, tasks: List[ListedTask]) derives JsonEncoder
  final case class Cancelled(ok: Boolean, id: Long) derives JsonEncoder
  final case class TimezoneInfo(timezone: String, local_now: String) derives JsonEncoder
  final case class TimezoneSet(ok: Boolean, timezone: String, local_now: String) derives JsonEncoder

  private def upcomingCount(recurring: Boolean) = if recurring then 3 else 1

  // A failure the agent should read as data ({ok:false}), not as an error.
  private def failure(error: String) = Failure(ok = false, error).toJsonAST.toOption.get

  val tools: List[ToolDef] = List(
    tool(
      "schedule_task",
      "Schedule an agent task",
      "Register a cron-driven task. When the cron matches, MCP enqueues a `scheduler` signal with the given prompt " +
        "and the agent acts on it (send a Telegram message, run a check, etc). Cron is standard 5-field (minute hour " +
        "day-of-month month day-of-week) evaluated in the user's configured timezone. For one-shot reminders set " +
        "`recurring: false` and use a specific cron like '30 14 12 5 *' (14:30 on May 12); the task auto-deactivates " +
        "after the first fire. For repeating tasks use a generic cron like '0 9 * * *' (every day 9:00). The agent is " +
        "responsible for converting natural-language times into cron syntax before calling this tool."
    ) { (deps, p: ScheduleTaskParams) =>
      ZIO.when(p.cron_expr.isEmpty || p.prompt.isEmpty)(invalid("cron_expr and prompt must be non-empty")) *>
        (Cron5.parse(p.cron_expr) match
          case Left(err)   => ZIO.succeed(failure(s"Invalid cron expression: $err"))
          case Right(cron) =>
            for
              tz <- deps.settings.timezone
              now <- Clock.instant
              task <- deps.scheduler.insert(p.cron_expr, p.recurring, p.prompt, None)
              upcoming = Cron5.previewNextFires(cron, tz, upcomingCount(p.recurring), now)
            yield Scheduled(ok = true, TaskRow.of(task), tz.getId, upcoming).toJsonAST.toOption.get)
    },
    tool(
      "list_scheduled_tasks",
      "List scheduled tasks",
      "Show every task that may still fire — recurring tasks (always) and one-shots that haven't been triggered yet. " +
        "Each row includes the cron expression, prompt, last fire time, and the next 1-3 upcoming fire timestamps in " +
        "the user's timezone for sanity-checking."
    ) { (deps, _: NoArgs) =>
      for
        tz <- deps.settings.timezone
        now <- Clock.instant
        active <- deps.scheduler.listActive
      yield
        val rows = active.map { t =>
          // An invalid cron surfaces as an empty `upcoming_fires`.
          val upcoming =
            Cron5.parse(t.cronExpr).fold(_ => Nil, Cron5.previewNextFires(_, tz, upcomingCount(t.recurring), now))
          ListedTask(
            t.id,
            t.cronExpr,
            t.recurring,
            t.prompt,
            t.source,
            t.lastRunAt,
            Time.sqlTime(t.createdAt),
            t.lastRunAt.map(Time.isoFromUnix),
            upcoming
          )
        }
        Listed(tz.getId, rows.size, rows)
    },
    tool(
      "cancel_scheduled_task",
      "Cancel a scheduled task",
      "Permanently remove a task by id. Use this when the user says 'forget about that reminder' or 'stop the daily " +
        "X'. Returns { ok: true, removed: <id> } on success, { ok: false } if no such task."
    ) { (deps, p: CancelTaskParams) =>
      ZIO.when(p.id < 1)(invalid("id must be a positive integer")) *>
        deps.scheduler.delete(p.id).map(Cancelled(_, p.id))
    },
    tool(
      "get_timezone",
      "Get configured timezone",
      "Return the IANA timezone driving cron evaluation and digest schedule decisions. Defaults to UTC when unset."
    ) { (deps, _: NoArgs) =>
      Clock.instant.flatMap(deps.settings.localTime).map(now => TimezoneInfo(now.tz.getId, now.display))
    },
    tool(
      "set_timezone",
      "Set the configured timezone",
      "Update the IANA timezone (e.g. 'Europe/Kiev', 'America/New_York', 'UTC'). Takes effect immediately — the next " +
        "scheduler tick, daily digest check, and any new schedule_task call all use the new value. Existing tasks keep " +
        "their cron string as-is, so their next-fire wall-clock time shifts. Invalid IANA names are rejected."
    ) { (deps, p: SetTimezoneParams) =>
      ZIO.when(p.tz.isEmpty)(invalid("tz must be non-empty")) *>
        (Settings.parseTimezone(p.tz) match
          case Left(err)   => ZIO.succeed(failure(s"Invalid timezone '${p.tz}': $err"))
          case Right(zone) =>
            for
              _ <- deps.settings.setTimezone(zone)
              now <- Clock.instant
            yield TimezoneSet(ok = true, p.tz, Settings.localTime(zone, now).display).toJsonAST.toOption.get)
    }
  )
