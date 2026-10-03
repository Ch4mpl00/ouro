package mcp

// The signal queue (`signals` table). Pollers enqueue rows; the agent pops
// them one at a time via `get_next_signal`. The agent never polls external
// systems itself — every event it sees was built and queued by a poller in
// this process.
//
// Sections:
//   1. queue       — record / pop / count / list
//   2. env context — the per-signal environment note for the agent's prompt
//   3. tools       — `signals` toolset (get_next_signal),
//                    `dreaming` toolset (list_signals)

import io.getquill.*
import io.getquill.extras.*
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.json.*

import java.time.Instant


// ── 1. queue ─────────────────────────────────────────────────────────────────

final case class SignalRecord(
    id: Long,
    source: String,
    content: String,
    createdAt: Instant,
    consumedAt: Option[Instant]
)

// Timestamps go out as "YYYY-MM-DD HH:MM:SS" UTC, the shape the agent has
// always been handed.
final case class PendingSignal(id: Long, source: String, content: String, created_at: String)

final case class SignalRow(id: Long, source: String, content: String, created_at: String, consumed_at: Option[String])
    derives JsonEncoder

final case class ListSignals(
    // Exclusive lower bound on created_at.
    since: Option[Instant] = None,
    source: Option[String] = None,
    limit: Option[Int] = None
)

final class Signals(db: Db):
  private val quill = db.quill
  import quill.*

  private inline def signals = quote(querySchema[SignalRecord]("signals"))

  def record(source: String, content: String): Task[Long] =
    run(signals.insert(_.source -> lift(source), _.content -> lift(content)).returning(_.id))

  // Atomically pops the oldest pending signal. SKIP LOCKED: a concurrent
  // popper takes the next row instead of waiting, never the same one.
  def popNext: Task[Option[PendingSignal]] =
    run(
      signals
        .filter(s =>
          s.id == sql"""(SELECT id FROM signals WHERE consumed_at IS NULL
                          ORDER BY id ASC LIMIT 1 FOR UPDATE SKIP LOCKED)""".as[Long]
        )
        .update(_.consumedAt -> sql"now()".as[Option[Instant]])
        .returningMany(s => (s.id, s.source, s.content, s.createdAt))
    ).map(_.headOption.map((id, source, content, created) => PendingSignal(id, source, content, Time.sqlTime(created))))

  def countPending: Task[Long] = run(signals.filter(_.consumedAt.isEmpty).size)

  // Read-only view; never pops. The dreaming session reviews what happened
  // since its previous fire with this. A dynamic query, so an unset filter is
  // left out of the SQL: the static `lift(opt).forall(...)` form binds a NULL
  // timestamptz for `? IS NULL`, which pgjdbc sends untyped and Postgres can't
  // plan.
  def list(filter: ListSignals): Task[List[SignalRow]] =
    run(
      signals.dynamic
        .filterOpt(filter.since)((s, since) => quote(s.createdAt > since))
        .filterOpt(filter.source)((s, source) => quote(s.source == unquote(source)))
        .sortBy(_.id)
        .take(filter.limit.getOrElse(Signals.DefaultListLimit))
    ).map(_.map(r => SignalRow(r.id, r.source, r.content, Time.sqlTime(r.createdAt), r.consumedAt.map(Time.sqlTime))))

object Signals:
  val DefaultListLimit = 200

  // ── 2. env context ─────────────────────────────────────────────────────────

  // Attached to every popped signal so the agent knows where to send
  // notifications and which forum topics exist. Skill text is not attached —
  // the agent loads `skills/<source>.md` itself.
  def envContext(telegram: TelegramConfig): Option[String] =
    val chat = telegram.defaultChatId.map(id => s"Default Telegram chat id: $id.").toList
    val topics =
      if telegram.topics.isEmpty then Nil
      else
        "Available Telegram forum topics (name → thread_id):" ::
          telegram.topics.map((name, id) => s"  - $name: $id") :::
          List(
            "When sending a message that semantically belongs to one of these topics, " +
              "pass the matching messageThreadId to send_telegram_message."
          )
    val lines = chat ++ topics
    Option.when(lines.nonEmpty)(("" :: "## Environment" :: lines).mkString("\n"))

// ── 3. tools ─────────────────────────────────────────────────────────────────

object SignalTools:
  import Tools.*

  // The popped signal, flattened, plus its environment note.
  final case class NextSignal(id: Long, source: String, content: String, created_at: String, envContext: Option[String])
      derives JsonEncoder
  final case class NextSignalResult(signal: Option[NextSignal], pendingAfter: Long) derives JsonEncoder

  val signals: List[ToolDef] = List(
    tool(
      "get_next_signal",
      "Get the next pending signal",
      "Atomically pop the oldest pending signal from the MCP queue. Returns `{ signal, pendingAfter }`. The signal " +
        "carries `content` (the user-message payload for the agent) and `envContext` (a short note describing the " +
        "default Telegram chat id and the configured forum topics — agent prepends this to the system prompt). Skill " +
        "instructions are NOT attached; the agent loads `skills/<source>.md` itself. Returns `signal: null` when the " +
        "queue is empty. This is the agent's only way to learn about external events."
    ) { (deps, _: NoArgs) =>
      deps.signals.popNext.flatMap {
        case None    => ZIO.succeed(NextSignalResult(None, 0))
        case Some(s) =>
          val env = Signals.envContext(deps.telegram.config)
          deps.signals.countPending.map(n =>
            NextSignalResult(Some(NextSignal(s.id, s.source, s.content, s.created_at, env)), n)
          )
      }
    }
  )

  final case class ListSignalsParams(
      @description("ISO timestamp. Only signals with created_at > since are returned.") since: Option[String],
      @description("Restrict to a single signal source.") source: Option[String],
      @description("Max rows. Default 200.") @validate(Validator.inRange(1, 2000)) limit: Option[Int]
  ) derives JsonDecoder,
        Schema

  final case class ListSignalsResult(count: Int, signals: List[SignalRow]) derives JsonEncoder

  // "dreaming" is a historical toolset name: only list_signals lives in it.
  val dreaming: List[ToolDef] = List(
    tool(
      "list_signals",
      "List past signals",
      "Read-only view of past signals (does not pop or mutate the queue). Optional filters: `since` (ISO timestamp, " +
        "returns signals created after this), `source` (e.g. 'telegram', 'nashdom-bill'). Default limit 200. Used by " +
        "the dreaming skill to review what happened since the previous reflection."
    ) { (deps, p: ListSignalsParams) =>
      for
        _ <- ZIO.when(p.limit.exists(l => l < 1 || l > 2000))(invalid("limit must be between 1 and 2000"))
        // "YYYY-MM-DD HH:MM:SS" and full ISO both parse, so a `since` copied
        // from a signal or from a "Previous fire:" header both work.
        since <- ZIO.foreach(p.since)(s => orFail(Time.requireJsDate("since", s)))
        rows <- deps.signals.list(ListSignals(since, p.source, p.limit))
      yield ListSignalsResult(rows.size, rows)
    }
  )
