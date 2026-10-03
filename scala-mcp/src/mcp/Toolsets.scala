package mcp

// Named tool groups an instance can register (MCP_TOOLSETS). One MCP process
// serves one audience: the droplet supervisor gets everything, a third-party
// client (ChatGPT over the Secure MCP Tunnel) a hand-picked subset. Selection
// is by *registration*: an unselected tool never appears in tools/list, so it
// is invisible rather than merely forbidden
// (.claude/tasks/mcp-auth-and-tool-scoping.md, A2).
//
// Sections:
//   1. names     — the toolset vocabulary and the default surface
//   2. selection — parsing MCP_TOOLSETS
//   3. tools     — toolset → the domain file's tool list

// ── 1. names ─────────────────────────────────────────────────────────────────

enum Toolset(val name: String):
  case Gmail extends Toolset("gmail")
  case Telegram extends Toolset("telegram")
  // send_telegram_message only — no history, no edits, no chat actions.
  case TelegramSend extends Toolset("telegram-send")
  case Monobank extends Toolset("monobank")
  case Pdf extends Toolset("pdf")
  case Fs extends Toolset("fs")
  // fetch_url — arbitrary page fetch behind an SSRF guard. Never handed to
  // third-party clients: it would turn this server into their proxy.
  case Fetch extends Toolset("fetch")
  case Signals extends Toolset("signals")
  // search_news, list_news, fetch_article. Touches no personal account,
  // which is why it is safe to hand out.
  case NewsRead extends Toolset("news-read")
  case Knowledge extends Toolset("knowledge")
  // Historical name: only list_signals.
  case Dreaming extends Toolset("dreaming")
  case Userbot extends Toolset("userbot")
  case Scheduler extends Toolset("scheduler")
  case Skills extends Toolset("skills")
  // Unified memory — shared by every agent, safe to hand out.
  case Memory extends Toolset("memory")

  // ── 3. tools ───────────────────────────────────────────────────────────────

  def tools: List[ToolDef] = this match
    case Gmail => GmailTools.tools
    // The full telegram surface includes the send slice.
    case Telegram     => TelegramTools.send ++ TelegramTools.full
    case TelegramSend => TelegramTools.send
    case Monobank     => MonobankTools.tools
    case Pdf          => PdfTools.tools
    case Fs           => FsTools.tools
    case Fetch        => FetchTools.tools
    case Signals      => SignalTools.signals
    case NewsRead     => NewsTools.tools
    case Knowledge    => KnowledgeTools.tools
    case Dreaming     => SignalTools.dreaming
    case Userbot      => UserbotTools.tools
    case Scheduler    => SchedulerTools.tools
    case Skills       => SkillsTools.tools
    case Memory       => MemoryTools.tools

object Toolset:
  // The unrestricted surface, in the TS registration order. `telegram-send` is
  // absent (a strict subset of `telegram`, it would double-register), and so
  // is `skills`: the agent owns synthetic tools with those names, so the MCP
  // export is scoped to the ChatGPT tunnel.
  val Default: List[Toolset] =
    List(Gmail, Telegram, Monobank, Pdf, Fs, Fetch, Signals, NewsRead, Knowledge, Dreaming, Userbot, Scheduler, Memory)

  def fromName(name: String): Option[Toolset] = values.find(_.name == name)

  def compose(selected: List[Toolset]): List[ToolDef] = selected.flatMap(_.tools)

// ── 2. selection ─────────────────────────────────────────────────────────────

final case class ToolsetSelection(
    names: List[Toolset],
    // True when MCP_TOOLSETS narrowed the surface. Decides things outside the
    // tool list: a restricted instance never attaches the gateway (namespaced
    // upstream tools can't be allow-listed) and allows several sessions.
    restricted: Boolean
)

object ToolsetSelection:
  // Absent, empty or whitespace-only → the full default surface. An unknown
  // name is a hard error: silently serving a different surface than intended
  // is exactly what this feature exists to prevent.
  def parse(raw: Option[String]): Either[String, ToolsetSelection] =
    val requested = raw.getOrElse("").split(',').map(_.trim).filter(_.nonEmpty).toList
    if requested.isEmpty then Right(ToolsetSelection(Toolset.Default, restricted = false))
    else
      requested.filter(Toolset.fromName(_).isEmpty) match
        case Nil     => Right(ToolsetSelection(requested.flatMap(Toolset.fromName).distinct, restricted = true))
        case unknown =>
          val known = Toolset.values.map(_.name).sorted.mkString(", ")
          Left(s"MCP_TOOLSETS: unknown toolset(s) ${unknown.mkString(", ")}. Known: $known")
