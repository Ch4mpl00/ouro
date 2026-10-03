package mcp

// Monobank Personal API: a statement on demand. No poller — reactive only.
// Auth is the personal token from MONOBANK_API_KEY in the X-Token header;
// the rate limit is one statement request per 60s per account.
//
// Sections:
//   1. client — statement fetch + normalisation to major units
//   2. tools  — `monobank` toolset: list_monobank_transactions

import java.time.Instant

import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.{description, validate}
import sttp.tapir.Validator
import zio.*
import zio.http.{Header, Headers}
import zio.json.*
import zio.json.ast.Json

// ── 1. client ────────────────────────────────────────────────────────────────

final case class RawItem(
    id: String,
    time: Long,
    description: String,
    mcc: Long,
    amount: Long,
    operationAmount: Long,
    currencyCode: Long,
    cashbackAmount: Long,
    comment: Option[String] = None,
    receiptId: Option[String] = None,
    invoiceId: Option[String] = None,
    counterEdrpou: Option[String] = None,
    counterIban: Option[String] = None,
    counterName: Option[String] = None,
) derives JsonDecoder

// Minor units (kopecks/cents) shown in major units. Every currency we expect
// uses 100. Printed the way JSON.stringify prints a number: 100, not 100.0.
opaque type Amount = Long

object Amount:
  def minor(v: Long): Amount = v
  given JsonEncoder[Amount] = JsonEncoder[Json].contramap { minor =>
    Json.Num(java.math.BigDecimal(java.math.BigDecimal.valueOf(minor, 2).stripTrailingZeros.toPlainString))
  }

final case class Transaction(
    id: String,
    time: Option[String],
    description: String,
    comment: Option[String],
    mcc: Long,
    amount: Amount,
    operationAmount: Amount,
    currency: String,
    cashbackAmount: Amount,
    receiptId: Option[String],
    invoiceId: Option[String],
    counterIban: Option[String],
    counterName: Option[String],
    counterEdrpou: Option[String],
) derives JsonEncoder

object Transaction:
  def of(raw: RawItem): Transaction = Transaction(
    raw.id,
    Some(Time.isoFromUnix(raw.time)),
    raw.description,
    raw.comment,
    raw.mcc,
    Amount.minor(raw.amount),
    Amount.minor(raw.operationAmount),
    Monobank.isoCurrency(raw.currencyCode),
    Amount.minor(raw.cashbackAmount),
    raw.receiptId,
    raw.invoiceId,
    raw.counterIban,
    raw.counterName,
    raw.counterEdrpou,
  )

final case class Statement(
    accountId: String,
    from: String,
    to: String,
    days: Int,
    count: Int,
    transactions: List[Transaction],
) derives JsonEncoder

final class Monobank(http: HttpClient, apiKey: Option[String]):
  def statement(account: String, from: Instant, to: Instant): Task[List[Transaction]] =
    val (fromS, toS) = (from.getEpochSecond, to.getEpochSecond)
    val path = s"/personal/statement/${Monobank.urlencode(account)}/$fromS/$toS"
    for
      key <- ZIO.fromOption(apiKey).orElseFail(ToolFailure("MONOBANK_API_KEY is not set in .env"))
      _ <- ZIO.when(toS - fromS > Monobank.MaxRangeSecs)(
        Tools.fail(f"Monobank statement range cannot exceed 31 days (got ${(toS - fromS) / 86400.0}%.1fd)")
      )
      reply <- http.get(s"https://api.monobank.ua$path", Headers(Header.Custom("X-Token", key)))
      _ <- ZIO.when(reply.status == 429)(
        Tools.fail("Monobank rate limit hit (1 request per 60s per account). Try again later.")
      )
      _ <- ZIO.unless(reply.ok)(Tools.fail(s"Monobank $path failed (${reply.status}): ${reply.body}"))
      raw <- ZIO.fromEither(reply.as[List[RawItem]]).mapError(ToolFailure(_))
    yield raw.map(Transaction.of)

  def recent(account: String, days: Int): Task[Statement] =
    for
      to <- Clock.instant
      from = to.minus(java.time.Duration.ofDays(days))
      transactions <- statement(account, from, to)
    yield Statement(account, Time.iso(from), Time.iso(to), days, transactions.size, transactions)

object Monobank:
  val MaxRangeSecs: Long = 31L * 24 * 60 * 60

  def fromEnv(http: HttpClient): Monobank = Monobank(http, Env.get("MONOBANK_API_KEY"))

  def isoCurrency(code: Long): String = code match
    case 980 => "UAH"
    case 840 => "USD"
    case 978 => "EUR"
    case 826 => "GBP"
    case 985 => "PLN"
    case 124 => "CAD"
    case 756 => "CHF"
    case 392 => "JPY"
    case 156 => "CNY"
    case 643 => "RUB"
    case other => other.toString

  def urlencode(segment: String): String =
    java.net.URLEncoder.encode(segment, java.nio.charset.StandardCharsets.UTF_8).replace("+", "%20")

// ── 2. tools ─────────────────────────────────────────────────────────────────

object MonobankTools:
  import Tools.*

  final case class ListParams(
      @description("Monobank account id, or '0' for default UAH. Defaults to '0'.") accountId: Option[String],
      @description("Lookback window in days (default 7, max 31).") @validate(Validator.inRange(1, 31)) days: Option[Int],
  ) derives JsonDecoder, Schema

  val tools: List[ToolDef] = List(
    tool(
      "list_monobank_transactions",
      "List Monobank transactions",
      "Fetch recent transactions for a Monobank account. accountId can be a specific account.id or '0' for the " +
        "default UAH account. days is the lookback window (default 7, max 31). Rate limit: 1 request per 60s per " +
        "account — surface 429s rather than retrying.",
    ) { (deps, p: ListParams) =>
      ZIO.when(p.days.exists(d => d < 1 || d > 31))(invalid("days must be between 1 and 31")) *>
        deps.monobank.recent(p.accountId.getOrElse("0"), p.days.getOrElse(7))
    }
  )
