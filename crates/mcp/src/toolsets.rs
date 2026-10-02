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
//   3. routers   — toolset → the domain file's tool router

use rmcp::handler::server::router::tool::ToolRouter;

use crate::server::McpTools;

// ── 1. names ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toolset {
    Gmail,
    Telegram,
    // send_telegram_message only — no history, no edits, no chat actions.
    TelegramSend,
    Monobank,
    Pdf,
    Fs,
    // fetch_url — arbitrary page fetch behind an SSRF guard. Never handed to
    // third-party clients: it would turn this server into their proxy.
    Fetch,
    Signals,
    // search_news, list_news, fetch_article. Touches no personal account,
    // which is why it is safe to hand out.
    NewsRead,
    Knowledge,
    // Historical name: only list_signals.
    Dreaming,
    Userbot,
    Scheduler,
    Skills,
    // Unified memory — shared by every agent, safe to hand out.
    Memory,
}

impl Toolset {
    pub const ALL: [Toolset; 15] = [
        Toolset::Gmail,
        Toolset::Telegram,
        Toolset::TelegramSend,
        Toolset::Monobank,
        Toolset::Pdf,
        Toolset::Fs,
        Toolset::Fetch,
        Toolset::Signals,
        Toolset::NewsRead,
        Toolset::Knowledge,
        Toolset::Dreaming,
        Toolset::Userbot,
        Toolset::Scheduler,
        Toolset::Skills,
        Toolset::Memory,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Toolset::Gmail => "gmail",
            Toolset::Telegram => "telegram",
            Toolset::TelegramSend => "telegram-send",
            Toolset::Monobank => "monobank",
            Toolset::Pdf => "pdf",
            Toolset::Fs => "fs",
            Toolset::Fetch => "fetch",
            Toolset::Signals => "signals",
            Toolset::NewsRead => "news-read",
            Toolset::Knowledge => "knowledge",
            Toolset::Dreaming => "dreaming",
            Toolset::Userbot => "userbot",
            Toolset::Scheduler => "scheduler",
            Toolset::Skills => "skills",
            Toolset::Memory => "memory",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

// The unrestricted surface, in the TS registration order. `telegram-send` is
// absent (a strict subset of `telegram`, it would double-register), and so is
// `skills`: the agent owns synthetic tools with those names, so the MCP export
// is scoped to the ChatGPT tunnel.
pub const DEFAULT_TOOLSETS: &[Toolset] = &[
    Toolset::Gmail,
    Toolset::Telegram,
    Toolset::Monobank,
    Toolset::Pdf,
    Toolset::Fs,
    Toolset::Fetch,
    Toolset::Signals,
    Toolset::NewsRead,
    Toolset::Knowledge,
    Toolset::Dreaming,
    Toolset::Userbot,
    Toolset::Scheduler,
    Toolset::Memory,
];

// ── 2. selection ─────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct ToolsetSelection {
    pub names: Vec<Toolset>,
    // True when MCP_TOOLSETS narrowed the surface. Decides things outside the
    // router: a restricted instance never attaches the gateway (namespaced
    // upstream tools can't be allow-listed) and allows several sessions.
    pub restricted: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("MCP_TOOLSETS: unknown toolset(s) {unknown}. Known: {known}")]
pub struct UnknownToolsets {
    unknown: String,
    known: String,
}

// Absent, empty or whitespace-only → the full default surface. An unknown
// name is a hard error: silently serving a different surface than intended is
// exactly what this feature exists to prevent.
pub fn parse_toolsets(raw: Option<&str>) -> Result<ToolsetSelection, UnknownToolsets> {
    let requested: Vec<&str> = raw.unwrap_or("").split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
    if requested.is_empty() {
        return Ok(ToolsetSelection { names: DEFAULT_TOOLSETS.to_vec(), restricted: false });
    }

    let unknown: Vec<&str> = requested.iter().copied().filter(|n| Toolset::from_name(n).is_none()).collect();
    if !unknown.is_empty() {
        let mut known: Vec<&str> = Toolset::ALL.iter().map(|t| t.name()).collect();
        known.sort_unstable();
        return Err(UnknownToolsets { unknown: unknown.join(", "), known: known.join(", ") });
    }

    let mut names = Vec::new();
    for toolset in requested.into_iter().filter_map(Toolset::from_name) {
        if !names.contains(&toolset) {
            names.push(toolset);
        }
    }
    Ok(ToolsetSelection { names, restricted: true })
}

// ── 3. routers ───────────────────────────────────────────────────────────────

// Port in progress: a toolset whose domain has not moved to Rust yet is
// refused at boot rather than skipped, for the same reason an unknown name is.
#[derive(Debug, thiserror::Error)]
#[error("toolset(s) not ported to Rust yet: {0}. Narrow MCP_TOOLSETS or run the TS server.")]
pub struct NotPorted(String);

fn router(toolset: Toolset) -> Option<ToolRouter<McpTools>> {
    match toolset {
        Toolset::Signals => Some(McpTools::signals_tools()),
        Toolset::Dreaming => Some(McpTools::dreaming_tools()),
        Toolset::Scheduler => Some(McpTools::scheduler_tools()),
        Toolset::Gmail
        | Toolset::Telegram
        | Toolset::TelegramSend
        | Toolset::Monobank
        | Toolset::Pdf
        | Toolset::Fs
        | Toolset::Fetch
        | Toolset::NewsRead
        | Toolset::Knowledge
        | Toolset::Userbot
        | Toolset::Skills
        | Toolset::Memory => None,
    }
}

pub fn compose_router(names: &[Toolset]) -> Result<ToolRouter<McpTools>, NotPorted> {
    let missing: Vec<&str> = names.iter().filter(|t| router(**t).is_none()).map(|t| t.name()).collect();
    if !missing.is_empty() {
        return Err(NotPorted(missing.join(", ")));
    }
    Ok(names.iter().filter_map(|t| router(*t)).fold(ToolRouter::new(), |acc, r| acc + r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names(names: &[Toolset]) -> Vec<String> {
        let mut out: Vec<String> =
            compose_router(names).unwrap().list_all().into_iter().map(|t| t.name.into_owned()).collect();
        out.sort();
        out
    }

    #[test]
    fn absent_or_blank_means_the_full_default_surface() {
        for raw in [None, Some(""), Some("   "), Some(",, ,")] {
            assert_eq!(
                parse_toolsets(raw).unwrap(),
                ToolsetSelection { names: DEFAULT_TOOLSETS.to_vec(), restricted: false }
            );
        }
    }

    #[test]
    fn parses_a_restricted_list_trimming_and_deduplicating() {
        assert_eq!(
            parse_toolsets(Some(" news-read , telegram-send ,news-read")).unwrap(),
            ToolsetSelection { names: vec![Toolset::NewsRead, Toolset::TelegramSend], restricted: true }
        );
    }

    #[test]
    fn rejects_an_unknown_toolset() {
        let err = parse_toolsets(Some("news-read,telegramm")).unwrap_err().to_string();
        assert!(err.contains("unknown toolset(s) telegramm"), "{err}");
    }

    #[test]
    fn refuses_unported_toolsets_instead_of_skipping_them() {
        let err = compose_router(&[Toolset::Signals, Toolset::Gmail]).unwrap_err().to_string();
        assert!(err.contains("gmail"), "{err}");
    }

    #[test]
    fn registers_exactly_the_ported_tools() {
        assert_eq!(tool_names(&[Toolset::Signals]), ["get_next_signal"]);
        assert_eq!(tool_names(&[Toolset::Dreaming]), ["list_signals"]);
        assert_eq!(
            tool_names(&[Toolset::Scheduler]),
            ["cancel_scheduled_task", "get_timezone", "list_scheduled_tasks", "schedule_task", "set_timezone"]
        );
    }
}
