package mcp

// The personal Telegram account, read-only, over MTProto (Mtproto.scala). It
// reads the channels the user is subscribed to; the news poller harvests them.
//
// Sections:
//   1. session — the stored credential, in gramjs StringSession format
//   2. client  — lazy connection, dialogs, channel history
//   3. login   — the one-time interactive flow behind `userbot:auth`
//   4. tools   — `userbot` toolset: list_userbot_dialogs
//
// The session string is the long-lived credential: whoever has it has the
// account. It lives only in the `mcp_state` database, in the exact format
// gramjs wrote ("1" + base64(dc, address, port, 256-byte key)), so every
// server generation reads the same row and a switch needs no re-login.

import io.getquill.*
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import sttp.tapir.Schema.annotations.validate
import sttp.tapir.Validator
import zio.*
import zio.json.*
import zio.json.ast.Json

import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets.UTF_8
import java.time.Instant
import java.util.Base64

// ── 1. session ───────────────────────────────────────────────────────────────

final case class StringSession(dcId: Int, address: String, port: Int, authKey: Array[Byte]):
  def encode: String =
    val addr = address.getBytes(UTF_8)
    val buf = ByteBuffer.allocate(1 + 2 + addr.length + 2 + 256)
    buf.put(dcId.toByte).putShort(addr.length.toShort).put(addr).putShort(port.toShort).put(authKey)
    "1" + Base64.getEncoder.encodeToString(buf.array)

  override def equals(other: Any): Boolean = other match
    case s: StringSession =>
      s.dcId == dcId && s.address == address && s.port == port && java.util.Arrays.equals(s.authKey, authKey)
    case _ => false

  override def hashCode: Int = (dcId, address, port, java.util.Arrays.hashCode(authKey)).##

object StringSession:
  def decode(raw: String): Either[String, StringSession] =
    if !raw.startsWith("1") then Left("unsupported session string version")
    else
      scala.util.Try(Base64.getDecoder.decode(raw.drop(1).trim)).toEither.left.map(_.getMessage).flatMap { bytes =>
        if bytes.length <= 3 + 2 + 256 then Left("session string too short")
        else
          val buf = ByteBuffer.wrap(bytes)
          val dc = buf.get() & 0xff
          val len = buf.getShort() & 0xffff
          if bytes.length != 3 + len + 2 + 256 then Left("auth key must be 256 bytes")
          else
            val addr = Array.ofDim[Byte](len)
            buf.get(addr)
            val port = buf.getShort() & 0xffff
            val key = Array.ofDim[Byte](256)
            buf.get(key)
            Right(StringSession(dc, String(addr, UTF_8), port, key))
      }

final case class ApiCredentials(apiId: Int, apiHash: String)

object ApiCredentials:
  def fromEnv: IO[ToolFailure, ApiCredentials] =
    def v(name: String) = ZIO.fromOption(Env.get(name)).orElseFail(ToolFailure(s"Missing env var: $name"))
    for
      raw <- v("TELEGRAM_APP_ID")
      id <- ZIO.fromOption(raw.toIntOption).orElseFail(ToolFailure(s"TELEGRAM_APP_ID must be numeric, got $raw"))
      hash <- v("TELEGRAM_APP_API_HASH")
    yield ApiCredentials(id, hash)

// ── 2. client ────────────────────────────────────────────────────────────────

final case class Dialog(id: String, kind: String, title: String, username: Option[String], unreadCount: Int)

object Dialog:
  given JsonEncoder[Dialog] = JsonEncoder[Json].contramap { d =>
    Json.Obj(
      (List("id" -> Json.Str(d.id), "type" -> Json.Str(d.kind), "title" -> Json.Str(d.title)) ++
        d.username.map(u => "username" -> Json.Str(u)) ++ List("unreadCount" -> Json.Num(d.unreadCount)))*
    )
  }

final case class ChannelHandle(
    // Bare channel id, as gramjs's `entity.id` — the key every stored channel
    // post's metadata.chat_id and watermark uses.
    chatId: String,
    title: Option[String],
    username: Option[String],
    // inputPeerChannel(channel_id, access_hash)
    channelId: Long,
    accessHash: Long
)

final case class ChannelMessage(id: Long, date: Instant, text: String, views: Option[Long], forwards: Option[Long])

// Connected on first use, so the server boots before the userbot is ever
// authorised (auth is a one-time interactive step). The connection lives in
// the server's scope; a dropped one is reopened on the next call.
final class Userbot(db: Db, client: Ref.Synchronized[Option[MtprotoClient]], scope: Scope):
  private val quill = db.quill
  import quill.*

  private inline def accounts = quote(querySchema[IntegrationAccount]("integration_account"))

  def savedSession: Task[Option[(String, String)]] =
    run(
      accounts
        .filter(_.provider == lift(Userbot.Provider))
        .sortBy(_.createdAt)(using Ord.desc)
        .map(a => (a.accountKey, a.accessToken))
        .take(1)
    ).map(_.headOption.flatMap((key, token) => token.map(key -> _)))

  def hasSession: UIO[Boolean] = savedSession.map(_.isDefined).orElseSucceed(false)

  def saveSession(accountKey: String, session: String, metadata: Json): Task[Unit] =
    Clock.instant.flatMap { now =>
      val row =
        IntegrationAccount(Userbot.Provider, accountKey, Some(session), None, None, Some(metadata.toJson), now, now)
      run(
        accounts
          .insertValue(lift(row))
          .onConflictUpdate(_.provider, _.accountKey)(
            (t, e) => t.accessToken -> e.accessToken,
            (t, e) => t.metadata -> e.metadata,
            (t, e) => t.updatedAt -> e.updatedAt
          )
      ).unit
    }

  private def connected: Task[MtprotoClient] =
    client.modifyZIO {
      case Some(c) => ZIO.succeed(c -> Some(c))
      case None    =>
        for
          saved <- savedSession.someOrFail(
            ToolFailure("Telegram userbot is not authorized. Run `pnpm userbot:auth` once to log in.")
          )
          creds <- ApiCredentials.fromEnv
          session <- Tools.orFail(StringSession.decode(saved._2))
          c <- scope.extend(MtprotoClient.connect(session, creds))
        yield c -> Some(c)
    }

  // A transport failure drops the cached connection; the next call reconnects.
  private def withClient[A](f: MtprotoClient => Task[A]): Task[A] =
    connected.flatMap(f).tapError {
      case _: java.io.IOException => client.set(None)
      case _                      => ZIO.unit
    }

  def listDialogs(limit: Int): Task[List[Dialog]] = withClient(_.dialogs(limit))

  def listChannels: Task[List[ChannelHandle]] = withClient(_.channels(500))

  // Newest-first, at most `limit`, only ids above `since` (exclusive) —
  // gramjs's `minId` semantics. Messages without text are skipped.
  def fetchMessages(channel: ChannelHandle, since: Option[Long], limit: Int): Task[List[ChannelMessage]] =
    withClient(_.history(channel, since, limit))

object Userbot:
  val Provider = "telegram_userbot"

  def make(db: Db): ZIO[Scope, Nothing, Userbot] =
    for
      scope <- ZIO.scope
      client <- Ref.Synchronized.make(Option.empty[MtprotoClient])
    yield Userbot(db, client, scope)

  // Accepts "tginsider", "@tginsider", "https://t.me/tginsider/".
  def normalizeHandle(handle: String): String =
    val h = handle.trim
    val bare = h.stripPrefix("https://t.me/").stripPrefix("http://t.me/")
    bare.dropWhile(_ == '@').reverse.dropWhile(_ == '/').reverse

// ── 3. login ─────────────────────────────────────────────────────────────────

object UserbotLogin:
  // Phone → code → (2FA password) → the session saved under the account id.
  // Returns (account id, username).
  def run(userbot: Userbot, prompts: MtLogin.Prompts): ZIO[Scope, Throwable, (String, Option[String])] =
    for
      creds <- ApiCredentials.fromEnv
      done <- MtLogin.run(creds, prompts)
      user = done.user
      account = user.long("id").fold("unknown")(_.toString)
      metadata = Json.Obj(
        "username" -> user.str("username").fold(Json.Null)(Json.Str(_)),
        "firstName" -> user.str("first_name").fold(Json.Null)(Json.Str(_)),
        "phone" -> user.str("phone").fold(Json.Null)(Json.Str(_))
      )
      _ <- userbot.saveSession(account, done.session.encode, metadata)
    yield (account, user.str("username"))

// ── 4. tools ─────────────────────────────────────────────────────────────────

enum DialogFilter derives JsonDecoder:
  case channel, group, user, all

object DialogFilter:
  given Schema[DialogFilter] = Schema.derivedEnumeration[DialogFilter].defaultStringBased

object UserbotTools:
  import Tools.*

  final case class ListDialogsParams(
      @description(
        "Filter dialogs by type. 'channel' is the right choice for the news digest (broadcast channels). Default 'all'."
      ) @jsonField("type") @sttp.tapir.Schema.annotations.encodedName("type") kind: Option[DialogFilter],
      @description("Max dialogs to return. Default 100.") @validate(Validator.inRange(1, 500)) limit: Option[Int]
  ) derives JsonDecoder,
        Schema

  final case class Dialogs(count: Int, dialogs: List[Dialog]) derives JsonEncoder

  val tools: List[ToolDef] = List(
    tool(
      "list_userbot_dialogs",
      "List userbot dialogs (chats / channels)",
      "List the personal Telegram account's dialogs (channels, groups, private chats) the userbot is subscribed to. " +
        "Mostly useful for discovery / debugging — to read channel posts use `list_news` with source='channel'; the " +
        "news poller harvests every subscribed channel in the background. Requires `pnpm userbot:auth` to have been " +
        "run once."
    ) { (deps, p: ListDialogsParams) =>
      for
        _ <- ZIO.when(p.limit.exists(l => l < 1 || l > 500))(invalid("limit must be between 1 and 500"))
        dialogs <- deps.userbot.listDialogs(p.limit.getOrElse(100))
        wanted = p.kind.filterNot(_ == DialogFilter.all).map(_.toString)
        filtered = dialogs.filter(d => wanted.forall(_ == d.kind))
      yield Dialogs(filtered.size, filtered)
    }
  )
