package mcp

// Telegram bot domain — the canonical Telegram surface. Incoming messages
// become signals; outgoing ones are tool calls; both land in one chat log.
//
// Sections:
//   1. config   — token, default chat, forum topic map (parsed once at boot)
//   2. bot api  — the handful of Bot API methods used, over plain HTTP
//   3. chat log — `telegram_messages` + the poller's `telegram_kv` cursor
//   4. typing   — chat-action keep-alive (the indicator only lives ~5s)
//   5. status   — one live progress bubble edited in place, self-animating
//   6. poller   — getUpdates long-poll → chat log + `telegram` signals
//   7. tools    — `telegram-send` (send only) and `telegram` (everything)

import io.getquill.*
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.SchemaType
import sttp.tapir.Validator
import zio.*
import zio.json.*
import zio.json.ast.Json

import java.time.Instant

// ── 1. config ────────────────────────────────────────────────────────────────

final case class TelegramConfig(
    // TELEGRAM_ASSISTANT_BOT_TOKEN (BotFather). Optional at boot: only the
    // calls that need it fail without it.
    botToken: Option[String] = None,
    // TELEGRAM_DEFAULT_CHAT_ID: the one chat the poller accepts messages from
    // and the default destination for agent replies.
    defaultChatId: Option[String] = None,
    // TELEGRAM_TOPICS_JSON='{"bills":42,"bank":43}' — name → forum thread id,
    // in the order the JSON lists them (the order the prompt shows them in).
    topics: List[(String, Long)] = Nil
)

object TelegramConfig:
  def fromEnv: TelegramConfig =
    def v(name: String) = Env.get(name)
    TelegramConfig(
      v("TELEGRAM_ASSISTANT_BOT_TOKEN"),
      v("TELEGRAM_DEFAULT_CHAT_ID"),
      parseTopics(v("TELEGRAM_TOPICS_JSON"))
    )

  // Lenient by design: a malformed map degrades to "no topics" rather than
  // failing boot, and non-integer ids are dropped.
  def parseTopics(raw: Option[String]): List[(String, Long)] = raw.map(_.fromJson[Json]) match
    case Some(Right(Json.Obj(fields))) =>
      fields.toList.collect {
        case (name, Json.Num(n)) if scala.util.Try(n.longValueExact).isSuccess => name -> n.longValue
      }
    case _ => Nil

// ── 2. bot api ───────────────────────────────────────────────────────────────

enum TelegramError(message: String) extends Exception(message):
  case NoToken extends TelegramError("TELEGRAM_ASSISTANT_BOT_TOKEN is not set. Add it to .env (BotFather token).")
  case Api(method: String, status: Int, description: String)
      extends TelegramError(s"Telegram $method failed ($status): $description")

  def apiDescription: String = this match
    case Api(_, _, d) => d
    case NoToken      => ""

final case class SentChat(id: Long) derives JsonDecoder
final case class SentMessage(message_id: Long, date: Long, chat: SentChat) derives JsonDecoder

final case class UpdateChat(
    id: Long,
    @jsonField("type") kind: String,
    title: Option[String] = None,
    first_name: Option[String] = None,
    last_name: Option[String] = None,
    username: Option[String] = None
) derives JsonDecoder

final case class UpdateMessage(
    message_id: Long,
    chat: UpdateChat,
    text: Option[String],
    message_thread_id: Option[Long]
) derives JsonDecoder

final case class Update(
    update_id: Long,
    message: Option[UpdateMessage] = None,
    edited_message: Option[UpdateMessage] = None,
    channel_post: Option[UpdateMessage] = None
) derives JsonDecoder

private final case class Envelope(ok: Boolean, description: Option[String], result: Option[Json]) derives JsonDecoder

enum ChatAction derives JsonEncoder, JsonDecoder:
  case typing, upload_photo, record_video, upload_video, record_voice, upload_voice, upload_document, choose_sticker,
    find_location, record_video_note, upload_video_note

object ChatAction:
  given Schema[ChatAction] = Schema.derivedEnumeration[ChatAction].defaultStringBased

final class BotApi(http: HttpClient, token: Option[String]):
  // Long-poll window inside getUpdates, seconds. The HTTP timeout outlasts it.
  val LongPollTimeout = 25

  private def call[A: JsonDecoder](method: String, body: Json.Obj): IO[TelegramError | Throwable, A] =
    for
      token <- ZIO.fromOption(token).orElseFail(TelegramError.NoToken)
      // JSON.stringify drops `undefined`; Telegram rejects explicit nulls for
      // some fields (message_thread_id: null → "Bad Request").
      clean = Json.Obj(body.fields.filterNot(_._2 == Json.Null))
      reply <- http.postJson(
        s"https://api.telegram.org/bot$token/$method",
        clean,
        timeout = (LongPollTimeout + 15).seconds
      )
      envelope <- ZIO.fromEither(reply.as[Envelope]).mapError(e => TelegramError.Api(method, reply.status, e))
      _ <- ZIO.unless(envelope.ok)(
        ZIO.fail(TelegramError.Api(method, reply.status, envelope.description.getOrElse("unknown error")))
      )
      result <- ZIO
        .fromEither(envelope.result.getOrElse(Json.Null).as[A])
        .mapError(e => TelegramError.Api(method, reply.status, s"unexpected result shape: $e"))
    yield result

  def sendMessage(chatId: String, text: String, threadId: Option[Long]): Task[SentMessage] =
    call[SentMessage](
      "sendMessage",
      Json.Obj("chat_id" -> Json.Str(chatId), "text" -> Json.Str(text), "message_thread_id" -> num(threadId))
    )

  // "message is not modified" when the text is unchanged — an error callers
  // may ignore.
  def editMessageText(chatId: String, messageId: Long, text: String): Task[SentMessage] =
    call[SentMessage](
      "editMessageText",
      Json.Obj("chat_id" -> Json.Str(chatId), "message_id" -> Json.Num(messageId), "text" -> Json.Str(text))
    )

  // A bot may only delete its own messages, within 48h.
  def deleteMessage(chatId: String, messageId: Long): Task[Unit] =
    call[Json]("deleteMessage", Json.Obj("chat_id" -> Json.Str(chatId), "message_id" -> Json.Num(messageId))).unit

  def sendChatAction(chatId: String, action: ChatAction, threadId: Option[Long]): Task[Unit] =
    call[Json](
      "sendChatAction",
      Json.Obj(
        "chat_id" -> Json.Str(chatId),
        "action" -> Json.Str(action.toString),
        "message_thread_id" -> num(threadId)
      )
    ).unit

  def getUpdates(offset: Option[Long], timeout: Option[Int]): Task[List[Update]] =
    call[List[Update]](
      "getUpdates",
      Json.Obj(
        "offset" -> num(offset),
        "timeout" -> num(timeout.map(_.toLong)),
        "allowed_updates" -> timeout.fold(Json.Null)(_ => Json.Arr(Json.Str("message"), Json.Str("edited_message")))
      )
    )

  private def num(n: Option[Long]): Json = n.fold(Json.Null)(Json.Num(_))

// ── 3. chat log ──────────────────────────────────────────────────────────────

enum Role:
  case user, assistant

final case class TelegramMessage(
    id: Long,
    chatId: Long,
    tgMessageId: Option[Long],
    threadId: Option[Long],
    role: String,
    text: String,
    createdAt: Instant
)

final case class TelegramKv(key: String, value: String)

final case class StoredMessage(
    id: Long,
    chat_id: Long,
    tg_message_id: Option[Long],
    thread_id: Option[Long],
    role: String,
    text: String,
    created_at: String
) derives JsonEncoder

final class ChatLog(db: Db):
  private val quill = db.quill
  import quill.*

  private inline def messages = quote(querySchema[TelegramMessage]("telegram_messages"))
  private inline def kv = quote(querySchema[TelegramKv]("telegram_kv"))

  def record(chatId: Long, tgMessageId: Option[Long], threadId: Option[Long], role: Role, text: String): Task[Long] =
    run(
      messages
        .insert(
          _.chatId -> lift(chatId),
          _.tgMessageId -> lift(tgMessageId),
          _.threadId -> lift(threadId),
          _.role -> lift(role.toString),
          _.text -> lift(text)
        )
        .returning(_.id)
    )

  // Last `limit` messages, chronological. `threadId` scopes to one forum
  // topic; None means every topic interleaved.
  def history(chatId: Long, limit: Int, threadId: Option[Long]): Task[List[StoredMessage]] =
    run(
      messages
        .filter(m => m.chatId == lift(chatId) && lift(threadId).forall(t => m.threadId.contains(t)))
        .sortBy(_.id)(using Ord.desc)
        .take(lift(limit))
    ).map(_.reverse.map { m =>
      StoredMessage(m.id, m.chatId, m.tgMessageId, m.threadId, m.role, m.text, Time.sqlTime(m.createdAt))
    })

  def lastUpdateId: Task[Option[Long]] =
    run(kv.filter(_.key == "last_update_id").map(_.value)).map(_.headOption.flatMap(_.toLongOption))

  def setLastUpdateId(id: Long): Task[Unit] =
    run(
      kv.insertValue(lift(TelegramKv("last_update_id", id.toString)))
        .onConflictUpdate(_.key)((t, e) => t.value -> e.value)
    ).unit

// ── 4. typing ────────────────────────────────────────────────────────────────

// The agent calls start_typing once; this re-sends the action every ~4s until
// send_telegram_message to the same chat/thread clears it, or a safety TTL
// passes so a crashed session can't leave the dots on forever.
final class Typing(bot: BotApi, active: Ref[Map[String, Typing.Entry]]):
  import Typing.*

  def start(chatId: String, action: ChatAction, threadId: Option[Long]): UIO[Unit] =
    for
      now <- Clock.instant
      _ <- active.update(_ + (key(chatId, threadId) -> Entry(chatId, action, threadId, now.plus(Ttl))))
      // Fire once now so the indicator shows without waiting for a tick.
      _ <- bot.sendChatAction(chatId, action, threadId).catchAll(e => ZIO.logError(s"typing: initial send failed: $e"))
    yield ()

  def stop(chatId: String, threadId: Option[Long]): UIO[Unit] = active.update(_ - key(chatId, threadId))

  def run: UIO[Nothing] =
    val tick = for
      now <- Clock.instant
      due <- active.updateAndGet(_.filter((_, e) => !e.expiresAt.isBefore(now)))
      _ <- ZIO.foreachDiscard(due) { (k, e) =>
        bot
          .sendChatAction(e.chatId, e.action, e.threadId)
          .catchAll(err => ZIO.logError(s"typing: keepalive $k failed: $err"))
      }
    yield ()
    tick.repeat(Schedule.spaced(4.seconds)) *> ZIO.never

object Typing:
  final case class Entry(chatId: String, action: ChatAction, threadId: Option[Long], expiresAt: Instant)
  val Ttl: java.time.Duration = java.time.Duration.ofMinutes(5)
  def key(chatId: String, threadId: Option[Long]) = s"$chatId:${threadId.getOrElse(0L)}"
  def make(bot: BotApi): UIO[Typing] = Ref.make(Map.empty[String, Entry]).map(Typing(bot, _))

// ── 5. status ────────────────────────────────────────────────────────────────

// One progress bubble per caller-chosen id (e.g. `status:<signalId>`): the
// first call sends it, later calls edit it, empty text deletes it. On top of
// that the bubble animates itself — trailing dots cycle 0 → . → .. → ... —
// so "aliveness" is MCP's job, not the workflow's. Never written to the chat
// log: it is ephemeral progress, not conversation.
final class StatusBubbles(bot: BotApi, entries: Ref[Map[String, StatusBubbles.Entry]]):
  import StatusBubbles.*

  def send(id: String, rawText: String, chatId: String, threadId: Option[Long]): Task[Json] =
    val text = rawText.trim
    entries.get.map(_.get(id)).flatMap {
      case None if text.isEmpty           => ZIO.succeed(result("noop", id))
      case Some(existing) if text.isEmpty =>
        entries.update(_ - id) *>
          bot
            .deleteMessage(existing.chatId, existing.messageId)
            // Already gone (deleted by the user, or >48h old): cleared either way.
            .catchSome { case _: TelegramError.Api => ZIO.unit }
            .as(result("deleted", id, Some(existing.chatId), Some(existing.messageId)))
      case Some(existing) =>
        bot.editMessageText(existing.chatId, existing.messageId, render(text, 0)).either.flatMap {
          case Right(edited) =>
            refresh(id, text, resetFrame = true).as(
              result("updated", id, Some(existing.chatId), Some(edited.message_id))
            )
          case Left(err: TelegramError) if isGone(err) => entries.update(_ - id) *> create(id, text, chatId, threadId)
          case Left(err: TelegramError) if isNotModified(err) =>
            refresh(id, text, resetFrame = false).as(
              result("updated", id, Some(existing.chatId), Some(existing.messageId))
            )
          case Left(err) => ZIO.fail(err)
        }
      case None => create(id, text, chatId, threadId)
    }

  private def create(id: String, text: String, chatId: String, threadId: Option[Long]): Task[Json] =
    for
      sent <- bot.sendMessage(chatId, render(text, 0), threadId)
      now <- Clock.instant
      _ <- entries.update(
        _ + (id -> Entry(chatId, sent.message_id, text, 0, now.plus(Ttl), now, busy = false, animating = true))
      )
    yield result("created", id, Some(chatId), Some(sent.message_id), Some(threadId))

  private def refresh(id: String, text: String, resetFrame: Boolean): UIO[Unit] =
    Clock.instant.flatMap { now =>
      entries.update(_.updatedWith(id)(_.map { e =>
        val base = e.copy(baseText = text, expiresAt = now.plus(Ttl), animating = true)
        if resetFrame then base.copy(frame = 0, nextEditAt = now) else base
      }))
    }

  def run: UIO[Nothing] = tick.repeat(Schedule.spaced(1.second)) *> ZIO.never

  private[mcp] def tick: UIO[Unit] =
    for
      now <- Clock.instant
      due <- entries.modify { all =>
        val stepped = all.map { (id, e) =>
          if !e.animating then id -> e
          else if e.expiresAt.isBefore(now) then id -> e.copy(animating = false)
          else if e.busy || now.isBefore(e.nextEditAt) then id -> e
          else id -> e.copy(frame = e.frame + 1, busy = true)
        }
        val picked = stepped.filter((id, e) => e.busy && !all(id).busy)
        (picked.toList, stepped)
      }
      _ <- ZIO.foreachDiscard(due) { (id, e) =>
        bot.editMessageText(e.chatId, e.messageId, render(e.baseText, e.frame)).either.flatMap { outcome =>
          Clock.instant.flatMap { after =>
            entries.update(_.updatedWith(id)(_.map { cur =>
              val backoff = outcome match
                case Left(err: TelegramError) if isNotModified(err) => false
                case Left(_)                                        => true
                case Right(_)                                       => false
              cur.copy(busy = false, nextEditAt = if backoff then after.plus(Backoff) else cur.nextEditAt)
            }))
          }
        }
      }
    yield ()

object StatusBubbles:
  final case class Entry(
      chatId: String,
      messageId: Long,
      baseText: String,
      frame: Int,
      // Freeze (stop editing) once this passes — a forgotten bubble must not
      // edit forever.
      expiresAt: Instant,
      // Back-off after a 429 / transient error.
      nextEditAt: Instant,
      // An edit is in flight; skip the tick rather than race it.
      busy: Boolean,
      animating: Boolean
  )

  val Ttl: java.time.Duration = java.time.Duration.ofMinutes(3)
  val Backoff: java.time.Duration = java.time.Duration.ofSeconds(5)
  // Adjacent frames always differ, so an edit is never "not modified".
  private val DotCycle = 4

  def render(base: String, frame: Int): String = base + "." * (frame % DotCycle)

  def make(bot: BotApi): UIO[StatusBubbles] = Ref.make(Map.empty[String, Entry]).map(StatusBubbles(bot, _))

  // action: created · updated · deleted · noop (clear with nothing tracked).
  // messageThreadId is present (possibly null) only on `created`.
  private def result(
      action: String,
      id: String,
      chatId: Option[String] = None,
      messageId: Option[Long] = None,
      threadId: Option[Option[Long]] = None
  ): Json =
    Json.Obj(
      (List("action" -> Json.Str(action), "id" -> Json.Str(id)) ++
        chatId.map(c => "chatId" -> Json.Str(c)) ++
        messageId.map(m => "messageId" -> Json.Num(m)) ++
        threadId.map(t => "messageThreadId" -> t.fold(Json.Null)(Json.Num(_))))*
    )

  def isNotModified(err: TelegramError): Boolean = err match
    case TelegramError.Api(_, _, d) => d.toLowerCase.contains("not modified")
    case _                          => false

  def isGone(err: TelegramError): Boolean = err match
    case TelegramError.Api(_, _, d) => d.toLowerCase.contains("not found") || d.toLowerCase.contains("to edit")
    case _                          => false

// ── 6. poller ────────────────────────────────────────────────────────────────

object TelegramPoller:
  // getUpdates allows one consumer per bot, so this lives in the MCP process —
  // nothing else polls. Messages from any chat but the default are ignored:
  // without a default chat the bot would accept anyone, so it stays off.
  def run(bot: BotApi, log: ChatLog, signals: Signals, config: TelegramConfig): UIO[Unit] =
    config.defaultChatId.map(raw => raw -> raw.toLongOption) match
      case None =>
        ZIO.logWarning("TELEGRAM_DEFAULT_CHAT_ID is not set — telegram poller disabled (would accept anyone)")
      case Some((raw, None)) =>
        ZIO.logWarning(s"TELEGRAM_DEFAULT_CHAT_ID is not a number ($raw) — telegram poller disabled")
      case Some((_, Some(allowed))) =>
        val once = for
          offset <- log.lastUpdateId
            .map(_.map(_ + 1))
            .catchAll(e => ZIO.logError(s"telegram poller: reading cursor failed: $e").as(None))
          updates <- bot.getUpdates(offset, Some(bot.LongPollTimeout))
          _ <- ingest(updates, allowed, log, signals)
        yield ()
        ZIO.logInfo(s"telegram poller started (chat $allowed, timeout ${bot.LongPollTimeout}s)") *>
          once
            .catchAll(err => ZIO.logError(s"telegram poll error: ${Results.describe(err)}") *> ZIO.sleep(5.seconds))
            .forever

  def ingest(updates: List[Update], allowedChat: Long, log: ChatLog, signals: Signals): Task[Unit] =
    ZIO.foreachDiscard(updates) { update =>
      val store = update.message.orElse(update.edited_message) match
        case Some(msg) if msg.chat.id != allowedChat =>
          ZIO.logWarning(s"ignoring message from chat ${msg.chat.id}: not the default one")
        case Some(msg) =>
          ZIO.foreachDiscard(msg.text.filter(_.nonEmpty)) { text =>
            log.record(msg.chat.id, Some(msg.message_id), msg.message_thread_id, Role.user, text) *>
              signals.record("telegram", signalContent(msg.chat.id, msg.message_thread_id, text)) *>
              ZIO.logInfo(s"stored message ${msg.message_id} + signal queued")
          }
        case None => ZIO.unit
      store *> log.setLastUpdateId(update.update_id)
    }

  // Signal content is DATA, not instructions: the planner knows how to reply
  // (send step + chat id from envContext). Text is JSON-quoted so a message
  // can't masquerade as more header lines.
  def signalContent(chatId: Long, threadId: Option[Long], text: String): String =
    val topic = threadId.fold("")(t => s" (forum topic thread_id=$t)")
    s"Telegram message in chat $chatId$topic.\nText: ${Json.Str(text).toJson}"

// ── 7. tools ─────────────────────────────────────────────────────────────────

// Everything the telegram tools reach, bundled so `Deps` carries one field.
final case class TelegramModule(
    config: TelegramConfig,
    bot: BotApi,
    log: ChatLog,
    typing: Typing,
    status: StatusBubbles
)

object TelegramModule:
  def make(config: TelegramConfig, http: HttpClient, db: Db): UIO[TelegramModule] =
    val bot = BotApi(http, config.botToken)
    for
      typing <- Typing.make(bot)
      status <- StatusBubbles.make(bot)
    yield TelegramModule(config, bot, ChatLog(db), typing, status)

// Models send chat ids as strings and as numbers. Accept both, normalise to
// one string so the typing keep-alive key can't split into two entries.
opaque type ChatId = String

object ChatId:
  def apply(s: String): ChatId = s
  extension (c: ChatId) def value: String = c
  given JsonDecoder[ChatId] = JsonDecoder[Json].mapOrFail {
    case Json.Str(s) => Right(s)
    case Json.Num(n) => Right(n.toBigInteger.toString)
    case other       => Left(s"chatId must be a string or an integer, got $other")
  }
  given Schema[ChatId] =
    Schema(SchemaType.SCoproduct[ChatId](List(Schema.string, Schema.schemaForLong), None)(_ => None))

object TelegramTools:
  import Tools.*

  val NoChatTarget =
    "No chat target. Pass chatId, or set TELEGRAM_DEFAULT_CHAT_ID in .env (find your id with `pnpm telegram:get-chat-id`)."

  private def checkText(text: String, allowEmpty: Boolean): IO[InvalidParams, Unit] =
    // UTF-16 code units — what Telegram counts.
    ZIO.when((!allowEmpty && text.isEmpty) || text.length > 4096)(invalid("text must be 1–4096 characters")).unit

  final case class SendParams(
      text: String,
      @description("Telegram chat id. Falls back to TELEGRAM_DEFAULT_CHAT_ID if omitted.")
      chatId: Option[ChatId],
      @description(
        "Forum topic thread_id. Required to reply inside a topic; omit for non-topic chats or the General topic."
      )
      messageThreadId: Option[Long]
  ) derives JsonDecoder,
        Schema

  final case class Delivered(
      delivered: Boolean,
      chatId: String,
      messageId: Long,
      messageThreadId: Option[Long],
      date: String
  ) derives JsonEncoder

  val send: List[ToolDef] = List(
    tool(
      "send_telegram_message",
      "Send Telegram message",
      "Send a Telegram message via the assistant bot. If chatId is omitted, TELEGRAM_DEFAULT_CHAT_ID env is used. " +
        "Pass messageThreadId to send into a specific forum topic (replies to a topic message must keep the same " +
        "messageThreadId so they land in the same topic; the system prompt lists configured topic name → thread_id " +
        "pairs if available). The returned messageId can be persisted and passed to edit_telegram_message later " +
        "(e.g. to mark a bill as paid). The outgoing message is also recorded in the local Telegram chat log so the " +
        "conversation history stays in sync."
    ) { (deps, p: SendParams) =>
      val tg = deps.telegram
      for
        _ <- checkText(p.text, allowEmpty = false)
        target <- ZIO
          .fromOption(p.chatId.map(_.value).orElse(tg.config.defaultChatId))
          .orElseFail(ToolFailure(NoChatTarget))
        sent <- tg.bot.sendMessage(target, p.text, p.messageThreadId)
        // The outgoing message clears the indicator client-side; stop the
        // keep-alive so it doesn't bleed into the next session.
        _ <- tg.typing.stop(target, p.messageThreadId)
        _ <- tg.log.record(sent.chat.id, Some(sent.message_id), p.messageThreadId, Role.assistant, p.text)
      yield Delivered(true, target, sent.message_id, p.messageThreadId, Time.isoFromUnix(sent.date))
    }
  )

  final case class EditParams(
      @description("Telegram chat id (the one the original message was sent to).") chatId: ChatId,
      @description("messageId returned by send_telegram_message.") messageId: Long,
      text: String
  ) derives JsonDecoder,
        Schema

  final case class Edited(edited: Boolean, chatId: String, messageId: Long, date: String) derives JsonEncoder

  final case class StatusParams(
      @description(
        "Stable per-workflow status id, e.g. `status:<signalId>`. Same id edits the same message."
      ) id: String,
      @description("Status text. Empty string deletes the status message.") text: String,
      @description("Telegram chat id. Falls back to TELEGRAM_DEFAULT_CHAT_ID if omitted.") chatId: Option[ChatId],
      @description("Forum topic thread_id (used when first creating the bubble).") messageThreadId: Option[Long]
  ) derives JsonDecoder,
        Schema

  final case class StartTypingParams(
      @description("Telegram chat id.") chatId: ChatId,
      @description("Chat action to display. Defaults to 'typing'.") action: Option[ChatAction],
      @description("Forum topic thread_id (display the indicator inside a specific topic).") messageThreadId: Option[
        Long
      ]
  ) derives JsonDecoder,
        Schema

  final case class ChatActionParams(
      @description("Telegram chat id.") chatId: ChatId,
      @description("Chat action to display.") action: ChatAction,
      messageThreadId: Option[Long]
  ) derives JsonDecoder,
        Schema

  final case class HistoryParams(
      @description("Telegram chat id.") chatId: ChatId,
      @description("Max messages. Default 50.") @validate(Validator.inRange(1, 500)) limit: Option[Int],
      @description("Forum topic thread_id. Restricts results to a single topic. Omit for unfiltered history.")
      threadId: Option[Long]
  ) derives JsonDecoder,
        Schema

  final case class Started(started: Boolean) derives JsonEncoder
  final case class Sent(sent: Boolean) derives JsonEncoder
  final case class History(messages: List[StoredMessage]) derives JsonEncoder

  val full: List[ToolDef] = List(
    tool(
      "edit_telegram_message",
      "Edit Telegram message",
      "Edit a previously-sent Telegram message in place. Use to update a bill notification when status changes " +
        "(e.g. mark as PAID). messageId is the value returned by send_telegram_message; chatId is the same chat the " +
        "message was sent to."
    ) { (deps, p: EditParams) =>
      checkText(p.text, allowEmpty = false) *>
        deps.telegram.bot
          .editMessageText(p.chatId.value, p.messageId, p.text)
          .map(e => Edited(true, p.chatId.value, e.message_id, Time.isoFromUnix(e.date)))
    },
    tool(
      "telegram_send_status",
      "Send / update / clear a live status message",
      "Show live progress in a SINGLE Telegram message edited in place, instead of posting a new message per step. " +
        "Call with the same `id` repeatedly to update the same bubble: the first call sends it, later calls edit it. " +
        "Call with an EMPTY `text` to delete the bubble when the work is done. Use a stable per-workflow id like " +
        "`status:<signalId>`. chatId falls back to TELEGRAM_DEFAULT_CHAT_ID. Status messages are ephemeral — they are " +
        "NOT written to the chat log; ship the real answer with send_telegram_message."
    ) { (deps, p: StatusParams) =>
      val tg = deps.telegram
      for
        _ <- ZIO.when(p.id.isEmpty)(invalid("id must be non-empty"))
        _ <- checkText(p.text, allowEmpty = true)
        target <- ZIO
          .fromOption(p.chatId.map(_.value).orElse(tg.config.defaultChatId))
          .orElseFail(ToolFailure(NoChatTarget))
        out <- tg.status.send(p.id, p.text, target, p.messageThreadId)
      yield out
    },
    tool(
      "start_typing",
      "Start typing indicator (auto-refresh until reply)",
      "Show a chat action indicator (typing by default) in a Telegram chat and keep it alive. MCP re-sends the " +
        "action every ~4s in the background — call this ONCE at the start of a session, no need to ping it on every " +
        "reasoning round. The indicator clears automatically when your `send_telegram_message` to the same chat/" +
        "thread is delivered. A safety TTL stops the keep-alive after 5 minutes if no message ever ships."
    ) { (deps, p: StartTypingParams) =>
      deps.telegram.typing
        .start(p.chatId.value, p.action.getOrElse(ChatAction.typing), p.messageThreadId)
        .as(Started(true))
    },
    // An escape hatch for one-off non-typing actions (an `upload_photo` blip
    // before posting an image); start_typing is the self-refreshing one.
    tool(
      "send_telegram_chat_action",
      "Send a one-shot Telegram chat action (no auto-refresh)",
      "Send a single chat action ping (~5s lifespan, no keep-alive). Prefer `start_typing` for the common 'show " +
        "typing while I work' case — this one is for one-off non-typing actions like a brief `upload_photo` before " +
        "sending an image."
    ) { (deps, p: ChatActionParams) =>
      deps.telegram.bot.sendChatAction(p.chatId.value, p.action, p.messageThreadId).as(Sent(true))
    },
    tool(
      "get_telegram_chat_history",
      "Get Telegram chat history",
      "Read the last N messages of a Telegram chat from the local log, in chronological order. Pass threadId to " +
        "scope to a single forum topic (reply context for a topic message). Omit threadId to see all topics " +
        "interleaved."
    ) { (deps, p: HistoryParams) =>
      for
        _ <- ZIO.when(p.limit.exists(l => l < 1 || l > 500))(invalid("limit must be between 1 and 500"))
        chatId <- ZIO
          .fromOption(p.chatId.value.trim.toLongOption)
          .orElseFail(ToolFailure(s"chatId must be numeric, got ${p.chatId.value}"))
        messages <- deps.telegram.log.history(chatId, p.limit.getOrElse(50), p.threadId)
      yield History(messages)
    }
  )

  def chatLabel(chat: UpdateChat): String =
    chat.title.getOrElse(
      List(chat.first_name, chat.last_name, chat.username.map("@" + _)).flatten.filter(_.nonEmpty).mkString(" ")
    )
