package mcp

// Gmail, read-only: NashDom bills arrive as email with a PDF attached.
//
// Sections:
//   1. oauth         — consent URL, code exchange, token refresh + storage
//   2. api           — the few REST calls used (list, get, attachments)
//   3. subscriptions — what to poll and how a hit reads as a signal
//   4. poller        — per-subscription watermark loop → signals
//   5. tools         — `gmail` toolset: list_nashdom_mails, download_gmail_attachment
//
// Plain REST instead of a generated client: four endpoints and a token
// refresh don't justify the dependency. Tokens live in `integration_account`
// exactly as googleapis stored them (expires_at as an ISO string), so every
// server generation shares one authorised account.

import java.nio.file.{Files, Path}
import java.time.Instant

import io.getquill.*
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.{description, validate}
import sttp.tapir.Validator
import zio.*
import zio.http.{Header, Headers}
import zio.json.*

// ── 1. oauth ─────────────────────────────────────────────────────────────────

final case class IntegrationAccount(
    provider: String,
    accountKey: String,
    accessToken: Option[String],
    refreshToken: Option[String],
    expiresAt: Option[String],
    metadata: Option[String],
    createdAt: Instant,
    updatedAt: Instant,
)

final case class TokenResponse(access_token: Option[String], refresh_token: Option[String], expires_in: Option[Long])
    derives JsonDecoder

final case class OAuthClient(clientId: String, clientSecret: String, redirectUri: String)

object OAuthClient:
  private def requireEnv(name: String): IO[ToolFailure, String] =
    ZIO.fromOption(Env.get(name)).orElseFail(ToolFailure(s"Missing env var: $name"))

  def fromEnv: IO[ToolFailure, OAuthClient] =
    requireEnv("GOOGLE_CLIENT_ID")
      .zip(requireEnv("GOOGLE_CLIENT_SECRET"))
      .zip(requireEnv("GOOGLE_REDIRECT_URI"))
      .map(OAuthClient.apply)

final class GmailModule(db: Db, http: HttpClient):
  import GmailModule.*

  private val quill = db.quill
  import quill.*

  private inline def accounts = quote(querySchema[IntegrationAccount]("integration_account"))
  private inline def kv = quote(querySchema[TelegramKv]("gmail_kv"))

  def authUrl: Task[String] = OAuthClient.fromEnv.map { oauth =>
    "https://accounts.google.com/o/oauth2/v2/auth" + HttpClient.query(
      "access_type" -> "offline",
      "prompt" -> "consent",
      "scope" -> Scopes.mkString(" "),
      "response_type" -> "code",
      "client_id" -> oauth.clientId,
      "redirect_uri" -> oauth.redirectUri,
    )
  }

  final private case class UserInfo(email: Option[String]) derives JsonDecoder

  def exchangeCodeAndPersist(code: String): Task[String] =
    for
      oauth <- OAuthClient.fromEnv
      tokens <- tokenRequest(
        "grant_type" -> "authorization_code",
        "code" -> code,
        "client_id" -> oauth.clientId,
        "client_secret" -> oauth.clientSecret,
        "redirect_uri" -> oauth.redirectUri,
      )
      access <- ZIO.fromOption(tokens.access_token).orElseFail(ToolFailure("token response had no access_token"))
      reply <- http.get("https://www.googleapis.com/oauth2/v2/userinfo", Headers(Header.Authorization.Bearer(access)))
      _ <- ZIO.unless(reply.ok)(Tools.fail(s"Google userinfo failed (${reply.status}): ${reply.body}"))
      info <- ZIO.fromEither(reply.as[UserInfo]).mapError(ToolFailure(_))
      account <- ZIO
        .fromOption(info.email)
        .orElseFail(ToolFailure("Could not resolve account email from Google userinfo response"))
      _ <- persistTokens(account, tokens)
    yield account

  private def tokenRequest(form: (String, String)*): Task[TokenResponse] =
    for
      reply <- http.postForm("https://oauth2.googleapis.com/token", form)
      _ <- ZIO.unless(reply.ok)(Tools.fail(s"Google token request failed (${reply.status}): ${reply.body}"))
      tokens <- ZIO.fromEither(reply.as[TokenResponse]).mapError(ToolFailure(_))
    yield tokens

  // Overwrites a stored value only with a fresh non-null one: refresh tokens
  // are not always re-issued, and losing one means re-consenting.
  private def persistTokens(account: String, tokens: TokenResponse): Task[Unit] =
    for
      existing <- storedTokens(account)
      now <- Clock.instant
      expiresAt = tokens.expires_in match
        case Some(secs) => Some(Time.iso(now.plusSeconds(secs)))
        case None => existing.flatMap(_.expiresAt)
      row = IntegrationAccount(
        Provider,
        account,
        tokens.access_token.orElse(existing.flatMap(_.accessToken)),
        tokens.refresh_token.orElse(existing.flatMap(_.refreshToken)),
        expiresAt,
        None,
        now,
        now,
      )
      _ <- run(
        accounts
          .insertValue(lift(row))
          .onConflictUpdate(_.provider, _.accountKey)(
            (t, e) => t.accessToken -> e.accessToken,
            (t, e) => t.refreshToken -> e.refreshToken,
            (t, e) => t.expiresAt -> e.expiresAt,
            (t, e) => t.updatedAt -> e.updatedAt,
          )
      )
    yield ()

  private def storedTokens(account: String): Task[Option[IntegrationAccount]] =
    run(accounts.filter(a => a.provider == lift(Provider) && a.accountKey == lift(account))).map(_.headOption)

  // GMAIL_ACCOUNT_KEY wins; otherwise the most recently authorised account.
  def resolveAccountKey: Task[Option[String]] =
    Env.get("GMAIL_ACCOUNT_KEY") match
      case Some(key) => ZIO.some(key)
      case None =>
        run(accounts.filter(_.provider == "gmail").sortBy(_.createdAt)(using Ord.desc).map(_.accountKey).take(1))
          .map(_.headOption)

  def requireAccountKey: Task[String] =
    resolveAccountKey.someOrFail(ToolFailure("No authorized Gmail account; run `pnpm gmail:auth`."))

  private def accessToken(account: String, forceRefresh: Boolean): Task[String] =
    for
      stored <- storedTokens(account).someOrFail(
        ToolFailure(s"""No Gmail account "$account". Run `pnpm gmail:auth` to authorize.""")
      )
      refresh <- ZIO
        .fromOption(stored.refreshToken)
        .orElseFail(ToolFailure(s"""Gmail account "$account" has no refresh token. Re-authorize."""))
      now <- Clock.instant
      fresh = stored.expiresAt.flatMap(Time.parseJsDate).exists(_.minusMillis(EagerRefreshMs).isAfter(now))
      token <- stored.accessToken.filter(_ => fresh && !forceRefresh) match
        case Some(access) => ZIO.succeed(access)
        case None =>
          for
            oauth <- OAuthClient.fromEnv
            tokens <- tokenRequest(
              "grant_type" -> "refresh_token",
              "refresh_token" -> refresh,
              "client_id" -> oauth.clientId,
              "client_secret" -> oauth.clientSecret,
            )
            _ <- persistTokens(account, tokens)
            access <- ZIO.fromOption(tokens.access_token).orElseFail(ToolFailure("token refresh returned no access_token"))
          yield access
    yield token

  // ── 2. api ─────────────────────────────────────────────────────────────────

  private def get[A: JsonDecoder](account: String, path: String, query: (String, String)*): Task[A] =
    def attempt(forceRefresh: Boolean): Task[A] =
      for
        token <- accessToken(account, forceRefresh)
        reply <- http.get(
          s"$Api$path${HttpClient.query(query*)}",
          Headers(Header.Authorization.Bearer(token)),
          timeout = 60.seconds,
        )
        out <-
          // A revoked/rotated access token: refresh once and retry.
          if reply.status == 401 && !forceRefresh then attempt(forceRefresh = true)
          else if !reply.ok then Tools.fail(s"Gmail $path failed (${reply.status}): ${reply.body}")
          else ZIO.fromEither(reply.as[A]).mapError(ToolFailure(_))
      yield out
    attempt(forceRefresh = false)

  final private case class IdOnly(id: Option[String]) derives JsonDecoder
  final private case class ListResponse(messages: Option[List[IdOnly]], nextPageToken: Option[String]) derives JsonDecoder

  def listMessages(account: String, query: String, maxResults: Int, pageToken: Option[String]): Task[Page] =
    for
      list <- get[ListResponse](
        account,
        "/messages",
        (List("q" -> query, "maxResults" -> maxResults.toString) ++ pageToken.map("pageToken" -> _))*
      )
      ids = list.messages.getOrElse(Nil).flatMap(_.id)
      summaries <- ZIO.foreachPar(ids)(summary(account, _))
    yield Page(summaries, list.nextPageToken)

  private def summary(account: String, id: String): Task[MessageSummary] =
    get[RawMessage](
      account,
      s"/messages/$id",
      "format" -> "metadata",
      "metadataHeaders" -> "From",
      "metadataHeaders" -> "To",
      "metadataHeaders" -> "Subject",
      "metadataHeaders" -> "Date",
    ).flatMap(raw => Tools.orFail(raw.summary))

  // The full MIME tree — what attachment discovery walks.
  def rawMessage(account: String, id: String): Task[RawMessage] = get[RawMessage](account, s"/messages/$id", "format" -> "full")

  final private case class AttachmentBody(data: Option[String]) derives JsonDecoder

  def attachmentData(account: String, messageId: String, attachmentId: String): Task[Array[Byte]] =
    for
      body <- get[AttachmentBody](account, s"/messages/$messageId/attachments/$attachmentId")
      data <- ZIO.fromOption(body.data).orElseFail(ToolFailure(s"Empty attachment data: $attachmentId"))
      // Gmail emits base64url, sometimes padded and sometimes not; the JDK
      // decoder takes both.
      bytes <- ZIO.attempt(java.util.Base64.getUrlDecoder.decode(data.trim))
    yield bytes

  // ── 4. poller ──────────────────────────────────────────────────────────────

  private def watermark(sub: Subscription): Task[Option[String]] =
    run(kv.filter(_.key == lift(watermarkKey(sub))).map(_.value)).map(_.headOption)

  private def setWatermark(sub: Subscription, value: Long): Task[Unit] =
    run(kv.insertValue(lift(TelegramKv(watermarkKey(sub), value.toString))).onConflictUpdate(_.key)((t, e) => t.value -> e.value)).unit

  // First run on a fresh install sets the watermark to now and emits
  // nothing, rather than flooding the queue with years of old mail.
  def poll(sub: Subscription, account: String, signals: Signals): Task[Unit] =
    watermark(sub).flatMap {
      case None =>
        Clock.instant.flatMap(now => setWatermark(sub, now.toEpochMilli)) *>
          ZIO.logInfo(s"bootstrapping gmail watermark for ${sub.name}, no emit")
      case Some(raw) =>
        val watermarkMs = raw.toLongOption.getOrElse(0L)
        def internal(m: MessageSummary) = m.internalDate.flatMap(_.toLongOption).getOrElse(0L)
        for
          page <- listMessages(account, s"${sub.query} after:${watermarkMs / 1000}", 50, None)
          // Chronological, so signals arrive in order and the watermark only
          // advances. `after:` is second-granular and non-strict: dedupe here.
          fresh = page.messages.sortBy(internal).filter(internal(_) > watermarkMs)
          _ <- ZIO.foreachDiscard(fresh) { m =>
            // Attachment refs inline, so the signal is self-contained.
            rawMessage(account, m.id).flatMap(raw =>
              signals.record(sub.signalSource, sub.buildContent(m, findAttachments(raw)))
            )
          }
          newest = (watermarkMs :: fresh.map(internal)).max
          _ <- ZIO.when(newest != watermarkMs)(setWatermark(sub, newest))
          _ <- ZIO.logInfo(s"gmail poll done: ${sub.name}, emitted ${fresh.size}")
        yield ()
    }

  def runPoller(signals: Signals): UIO[Unit] =
    ZIO
      .foreachParDiscard(Subscriptions) { sub =>
        val once = resolveAccountKey.flatMap {
          case None => ZIO.logWarning(s"no Gmail account authorized — skipping ${sub.name}")
          case Some(account) => poll(sub, account, signals)
        }
        ZIO.logInfo(s"gmail poller started: ${sub.name} every ${sub.interval.toSeconds}s as ${sub.signalSource}") *>
          once
            .catchAll(err => ZIO.logError(s"gmail poll failed (${sub.name}): ${Results.describe(err)}"))
            .repeat(Schedule.spaced(sub.interval))
      }

object GmailModule:
  val Provider = "gmail"
  val Scopes = List("https://www.googleapis.com/auth/gmail.readonly", "https://www.googleapis.com/auth/userinfo.email")
  val Api = "https://gmail.googleapis.com/gmail/v1/users/me"
  // google-auth-library refreshes this long before expiry.
  val EagerRefreshMs: Long = 5 * 60 * 1000

  def watermarkKey(sub: Subscription) = s"subscription.${sub.name}.last_internal_date_ms"

  // ── 3. subscriptions ───────────────────────────────────────────────────────

  // Adding an email-driven signal type = one entry here + a matching
  // `skills/<signalSource>.md`. The poller is generic.
  final case class Subscription(
      // Internal id; keys the watermark.
      name: String,
      query: String,
      // signal.source → skills/<signalSource>.md
      signalSource: String,
      interval: Duration,
      buildContent: (MessageSummary, List[AttachmentRef]) => String,
  )

  // All NashDom mail, by sender or subject. Real bills come from
  // nashdom*@gmail.com with a Cyrillic subject and a PDF; other NashDom mail
  // (announcements, replies) is surfaced too.
  val NashdomQuery = "from:nashdom OR subject:nashdom"

  val Subscriptions: List[Subscription] =
    List(Subscription("nashdom-bill", NashdomQuery, "nashdom-bill", 60.seconds, nashdomContent))

  private def formatAttachment(a: AttachmentRef): String =
    s"  - attachmentId: ${a.attachmentId}\n    filename: ${a.filename}\n    mimeType: ${a.mimeType}\n    sizeBytes: ${a.sizeBytes}"

  def nashdomContent(m: MessageSummary, attachments: List[AttachmentRef]): String =
    val meta = List(
      s"Subject: ${m.subject.getOrElse("(без темы)")}",
      s"From: ${m.from.getOrElse("(неизвестно)")}",
      s"Date: ${m.date.orElse(m.internalDate).getOrElse("(неизвестно)")}",
      s"messageId: ${m.id}",
    )
    val pdfs = attachments.filter(isPdf)
    val lines =
      if pdfs.isEmpty then
        List(
          "Пришло новое письмо от NashDom без вложений — перешли пользователю в Telegram subject и краткое содержание.",
          "",
        ) ++ meta ++ List(s"Snippet: ${m.snippet}", "", "Шаг: send_telegram_message(text).")
      else
        List(
          "Пришла новая квитанция NashDom. Скачай PDF, прочитай его и отправь пользователю в Telegram короткую сводку " +
            "(тип квитанции, период, 2–5 ключевых позиций, итого).",
          "",
        ) ++ meta ++ ("Attachments:" :: pdfs.map(formatAttachment)) ++ List(
          "",
          "Шаги: download_gmail_attachment(messageId, attachmentId) → read_pdf(filePath) → send_telegram_message(text).",
        )
    lines.mkString("\n")

  // Every MIME part carrying an attachmentId; inline body parts are skipped.
  def findAttachments(message: RawMessage): List[AttachmentRef] =
    def walk(part: MessagePart): List[AttachmentRef] =
      val own = part.body.flatMap(_.attachmentId).map { id =>
        AttachmentRef(
          id,
          part.filename.filter(_.nonEmpty).getOrElse("untitled"),
          part.mimeType.filter(_.nonEmpty).getOrElse("application/octet-stream"),
          part.body.flatMap(_.size).getOrElse(0L),
        )
      }
      own.toList ++ part.parts.getOrElse(Nil).flatMap(walk)
    message.payload.toList.flatMap(walk)

  def isPdf(a: AttachmentRef): Boolean = a.mimeType == "application/pdf" || a.filename.toLowerCase.endsWith(".pdf")

  private def sanitize(name: String): String =
    val cleaned = name.replaceAll("[/\\\\\u0000\n\r]+", "_").trim
    if cleaned.isEmpty then "untitled" else cleaned

  def attachmentPath(storage: Path, account: String, messageId: String, attachmentId: String, filename: Option[String])
      : Path =
    val prefix = attachmentId.filter(c => c.isLetterOrDigit && c < 128).take(12)
    val name = sanitize(filename.getOrElse("attachment.pdf"))
    storage.resolve("gmail").resolve(sanitize(account)).resolve(messageId).resolve(s"${prefix}_$name")

// ── 2. api (shapes) ──────────────────────────────────────────────────────────

final case class GmailHeader(name: Option[String], value: Option[String]) derives JsonDecoder
final case class PartBody(attachmentId: Option[String], size: Option[Long]) derives JsonDecoder
final case class MessagePart(
    mimeType: Option[String] = None,
    filename: Option[String] = None,
    headers: Option[List[GmailHeader]] = None,
    body: Option[PartBody] = None,
    parts: Option[List[MessagePart]] = None,
) derives JsonDecoder

final case class RawMessage(
    id: Option[String],
    threadId: Option[String],
    snippet: Option[String],
    internalDate: Option[String],
    labelIds: Option[List[String]],
    payload: Option[MessagePart],
) derives JsonDecoder:
  def header(name: String): Option[String] =
    payload.flatMap(_.headers).getOrElse(Nil).find(_.name.exists(_.equalsIgnoreCase(name))).flatMap(_.value)

  def summary: Either[String, MessageSummary] = (id, threadId) match
    case (Some(id), Some(thread)) =>
      Right(
        MessageSummary(
          id,
          thread,
          snippet.getOrElse(""),
          header("From"),
          header("To"),
          header("Subject"),
          header("Date"),
          internalDate,
          labelIds.getOrElse(Nil),
        )
      )
    case _ => Left("Gmail returned a message without id/threadId")

final case class MessageSummary(
    id: String,
    threadId: String,
    snippet: String,
    from: Option[String],
    to: Option[String],
    subject: Option[String],
    date: Option[String],
    internalDate: Option[String],
    labelIds: List[String],
) derives JsonEncoder

final case class Page(messages: List[MessageSummary], nextPageToken: Option[String])

final case class AttachmentRef(attachmentId: String, filename: String, mimeType: String, sizeBytes: Long)
    derives JsonEncoder

// ── 5. tools ─────────────────────────────────────────────────────────────────

object GmailTools:
  import Tools.*

  // Bills dated before this are already settled — tracking began here. Said
  // in the tool description so the model never suggests paying an old invoice.
  val PaymentTrackingSince = "2026-05"

  final case class ListParams(
      @validate(Validator.inRange(1, 100)) limit: Option[Int],
      @description("Continuation token from a previous call's nextPageToken.") pageToken: Option[String],
  ) derives JsonDecoder, Schema

  final case class DownloadParams(
      messageId: String,
      attachmentId: String,
      @description("Suggested filename for the saved file. Defaults to 'attachment.pdf'.") filename: Option[String],
  ) derives JsonDecoder, Schema

  final case class Mail(
      messageId: String,
      subject: Option[String],
      from: Option[String],
      date: Option[String],
      snippet: String,
      attachments: List[AttachmentRef],
  ) derives JsonEncoder

  final case class Mails(
      accountKey: String,
      query: String,
      paymentTrackingSince: String,
      messages: List[Mail],
      nextPageToken: Option[String],
  ) derives JsonEncoder

  final case class Saved(filePath: String, sizeBytes: Int) derives JsonEncoder

  val tools: List[ToolDef] = List(
    tool(
      "list_nashdom_mails",
      "List NashDom mails",
      "List ALL NashDom-related emails (sender or subject match), newest first. Returns message metadata (subject, " +
        "from, date, snippet) and PDF attachment refs if any. Most utility bills will have a PDF attachment, but " +
        "non-bill mail (announcements, replies) is also returned with an empty `attachments` array. The billing " +
        "period is in the subject (Ukrainian) and `date` field; deduce from there which bill is which. No side " +
        "effects — call download_gmail_attachment to fetch a specific PDF. IMPORTANT: payment tracking started " +
        "2026-05; bills with an earlier billing period are considered already settled — do NOT suggest the user pay " +
        "them. Pagination: pass `pageToken` from a previous response's `nextPageToken` to get the next page.",
    ) { (deps, p: ListParams) =>
      val gmail = deps.gmail
      for
        _ <- ZIO.when(p.limit.exists(l => l < 1 || l > 100))(invalid("limit must be between 1 and 100"))
        account <- gmail.requireAccountKey
        page <- gmail.listMessages(account, GmailModule.NashdomQuery, p.limit.getOrElse(25), p.pageToken)
        mails <- ZIO.foreachPar(page.messages) { m =>
          gmail.rawMessage(account, m.id).map { raw =>
            Mail(m.id, m.subject, m.from, m.date, m.snippet, GmailModule.findAttachments(raw).filter(GmailModule.isPdf))
          }
        }
      yield Mails(account, GmailModule.NashdomQuery, PaymentTrackingSince, mails, page.nextPageToken)
    },
    tool(
      "download_gmail_attachment",
      "Download a Gmail attachment",
      "Save a Gmail attachment to local storage and return the absolute filePath. Use after list_nashdom_mails to " +
        "fetch a specific PDF, then read it with the Read tool to extract bill fields.",
    ) { (deps, p: DownloadParams) =>
      for
        account <- deps.gmail.requireAccountKey
        bytes <- deps.gmail.attachmentData(account, p.messageId, p.attachmentId)
        path = GmailModule.attachmentPath(deps.storageDir, account, p.messageId, p.attachmentId, p.filename)
        _ <- ZIO.attemptBlocking {
          Files.createDirectories(path.getParent)
          Files.write(path, bytes)
        }
      yield Saved(path.toAbsolutePath.normalize.toString, bytes.length)
    },
  )
