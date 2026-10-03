package mcp

import java.time.{Instant, ZoneOffset, ZonedDateTime}

import zio.*
import zio.json.*
import zio.json.ast.Json
import zio.telemetry.opentelemetry.tracing.Tracing
import zio.test.*

object CoreSpec extends ZIOSpecDefault:
  private def utc(y: Int, mo: Int, d: Int, h: Int = 0, mi: Int = 0) = ZonedDateTime.of(y, mo, d, h, mi, 0, 0, ZoneOffset.UTC).toInstant

  private def names(sets: List[Toolset]) = Toolset.compose(sets).map(_.name).sorted

  // Pins docker-compose's MCP_TOOLSETS for mcp-tunnel exactly. If this list
  // and that env var drift apart, ChatGPT silently gets a surface nobody
  // decided on — in either direction.
  private val Tunnel = List(Toolset.NewsRead, Toolset.TelegramSend, Toolset.Skills, Toolset.Memory)

  def spec = suite("core")(
    suite("time")(
      test("formats like toISOString") {
        assertTrue(Time.iso(utc(2026, 10, 2, 9)) == "2026-10-02T09:00:00.000Z")
      },
      test("parses what new Date accepted") {
        val want = utc(2026, 10, 2)
        val inputs = List("2026-10-02", "2026-10-02T00:00:00Z", "2026-10-02T03:00:00+03:00", "2026-10-02T00:00:00.000")
        assertTrue(
          inputs.forall(Time.parseJsDate(_).contains(want)),
          Time.parseJsDate("Fri, 02 Oct 2026 00:00:00 +0000").contains(want),
          Time.parseJsDate("yesterday").isEmpty,
          Time.sqlTime(Instant.parse("2026-10-02T09:00:00.123Z")) == "2026-10-02 09:00:00",
        )
      },
    ),
    suite("toolsets")(
      test("absent or blank means the full default surface") {
        assertTrue(
          List(None, Some(""), Some("   "), Some(",, ,"))
            .forall(raw => ToolsetSelection.parse(raw) == Right(ToolsetSelection(Toolset.Default, restricted = false)))
        )
      },
      test("parses a restricted list, trimming and deduplicating") {
        assertTrue(
          ToolsetSelection.parse(Some(" news-read , telegram-send ,news-read")) ==
            Right(ToolsetSelection(List(Toolset.NewsRead, Toolset.TelegramSend), restricted = true))
        )
      },
      test("rejects an unknown toolset") {
        assertTrue(ToolsetSelection.parse(Some("news-read,telegramm")).left.exists(_.contains("unknown toolset(s) telegramm")))
      },
      test("exposes exactly the tunnel surface") {
        assertTrue(
          names(Tunnel) == List(
            "append_doc", "create_project", "doc_history", "fetch_article", "get_fact", "list_memory", "list_news",
            "list_skills", "patch_doc", "read_doc", "read_skill", "recall", "remember", "revert_patch", "search_news",
            "send_telegram_message", "update_fact", "write_doc",
          )
        )
      },
      // Everything a third-party client can reach must be safe to hand out:
      // no personal account, no signal delivery, no arbitrary fetch.
      test("keeps the tunnel clear of tools that must never leave the droplet") {
        val forbidden = List(
          "get_next_signal", "list_nashdom_mails", "list_monobank_transactions", "read_file", "fetch_url",
          "schedule_task", "get_telegram_chat_history",
        )
        assertTrue(forbidden.forall(t => !names(Tunnel).contains(t)))
      },
      test("the default surface matches the TS server") {
        val all = names(Toolset.Default)
        assertTrue(
          all == List(
            "add_note", "append_doc", "cancel_scheduled_task", "create_project", "doc_history",
            "download_gmail_attachment", "edit_telegram_message", "fetch_article", "fetch_url", "find_notes", "get_fact",
            "get_next_signal", "get_telegram_chat_history", "get_timezone", "list_memory", "list_monobank_transactions",
            "list_nashdom_mails", "list_news", "list_scheduled_tasks", "list_signals", "list_userbot_dialogs", "patch_doc",
            "read_doc", "read_file", "read_pdf", "recall", "remember", "revert_patch", "schedule_task", "search_news",
            "send_telegram_chat_action", "send_telegram_message", "set_timezone", "start_typing", "telegram_send_status",
            "update_fact", "write_doc",
          ),
          // The agent owns synthetic tools with these names.
          !all.contains("list_skills") && !all.contains("read_skill"),
        )
      },
    ),
    suite("server")(
      test("newest session evicts the previous one") {
        for
          s <- Sessions.make(newestWins = true)
          a <- s.create
          b <- s.create
          aLive <- s.exists(a)
          bLive <- s.exists(b)
        yield assertTrue(!aLive, bLive)
      },
      test("multi-session keeps every session") {
        for
          s <- Sessions.make(newestWins = false)
          a <- s.create
          b <- s.create
          live <- ZIO.foreach(List(a, b))(s.exists)
        yield assertTrue(live == List(true, true))
      },
      test("handler failures become isError results, bad params a JSON-RPC error") {
        val failing = Tools.tool("boom", "Boom", "fails") { (_, _: NoArgs) =>
          ZIO.fail(ToolFailure("Telegram sendMessage failed (400): chat not found")).as(Json.Null)
        }
        val typed = Tools.tool("typed", "Typed", "takes an int") { (_, p: CoreSpec.Count) => ZIO.succeed(p.n + 1) }
        val call = (name: String, args: String) =>
          s"""{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"$name","arguments":$args}}""".fromJson[Json].toOption.get
        for
          tracing <- ZIO.service[Tracing]
          // Neither tool reaches a dependency.
          handler = McpHandler(null, List(failing, typed), None, tracing)
          failed <- handler.handle(call("boom", "{}"))
          bad <- handler.handle(call("typed", """{"n":"x"}"""))
          ok <- handler.handle(call("typed", """{"n":41}"""))
        yield assertTrue(
          failed.flatMap(_.get("result")).flatMap(_.get("isError")).contains(Json.Bool(true)),
          bad.flatMap(_.get("error")).flatMap(_.get("code")).contains(Json.Num(-32602)),
          ok.flatMap(_.get("result")).flatMap(_.get("content")).toString.contains("42"),
        )
      },
      test("input schemas carry docs, bounds, enums and required fields") {
        val schema = TelegramTools.full.find(_.name == "get_telegram_chat_history").get.inputSchema
        val limit = schema.get("properties").flatMap(_.get("limit"))
        assertTrue(
          schema.get("required").contains(Json.Arr(Json.Str("chatId"))),
          limit.flatMap(_.get("minimum")).contains(Json.Num(1)),
          limit.flatMap(_.get("maximum")).contains(Json.Num(500)),
          limit.flatMap(_.get("description")).contains(Json.Str("Max messages. Default 50.")),
        )
      },
      test("Host validation accepts host and host:port entries") {
        val allowed = List("mcp", "mcp:3000", "localhost")
        assertTrue(
          Transport.hostAllowed(allowed, Some("mcp:3000")),
          Transport.hostAllowed(allowed, Some("localhost:3999")),
          !Transport.hostAllowed(allowed, Some("evil.com")),
          Transport.hostAllowed(Nil, Some("anything")),
        )
      },
    ),
    suite("env")(
      test("parses dotenv lines like dotenv does") {
        val parsed = Env.parse(List("# comment", "A=1", "export B = two ", "C=\"quoted # not a comment\"", "D=x # trailing", "broken"))
        assertTrue(parsed == List("A" -> "1", "B" -> "two", "C" -> "quoted # not a comment", "D" -> "x"))
      }
    ),
  ).provideLayer(Telemetry.noop)

  final case class Count(n: Int) derives JsonDecoder, sttp.tapir.Schema
