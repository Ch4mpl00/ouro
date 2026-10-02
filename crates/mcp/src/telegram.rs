// Telegram bot domain. Only the config section is ported so far — the Bot API
// client, chat log, long-poll poller and the tools follow in later steps.
//
// Sections:
//   1. config — default chat + forum topic map, parsed once at boot

// ── 1. config ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct TelegramConfig {
    // TELEGRAM_DEFAULT_CHAT_ID: the one chat the poller accepts messages from
    // and the default destination for agent replies.
    pub default_chat_id: Option<String>,
    // TELEGRAM_TOPICS_JSON='{"bills":42,"bank":43}' — name → forum thread id.
    // Bot API has no method to list forum topics, so this static map is the
    // only way the agent learns which topics exist. Kept in the order the
    // JSON lists them, which is the order the prompt shows them in.
    pub topics: Vec<(String, i64)>,
}

impl TelegramConfig {
    pub fn from_env() -> Self {
        Self {
            default_chat_id: std::env::var("TELEGRAM_DEFAULT_CHAT_ID").ok().filter(|v| !v.is_empty()),
            topics: parse_topics(std::env::var("TELEGRAM_TOPICS_JSON").ok().as_deref()),
        }
    }
}

// Lenient by design, like the TS original: a malformed map degrades to "no
// topics" rather than failing boot, and non-integer ids are dropped.
fn parse_topics(raw: Option<&str>) -> Vec<(String, i64)> {
    let Some(raw) = raw else { return Vec::new() };
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str(raw) else {
        if !raw.trim().is_empty() {
            tracing::warn!("TELEGRAM_TOPICS_JSON is not a JSON object, ignoring");
        }
        return Vec::new();
    };
    entries.into_iter().filter_map(|(name, id)| id.as_i64().map(|id| (name, id))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_topics_leniently() {
        assert!(parse_topics(None).is_empty());
        assert!(parse_topics(Some("[1,2]")).is_empty());
        assert!(parse_topics(Some("not json")).is_empty());
        let topics = parse_topics(Some(r#"{"news":7,"bank":"43","bills":42}"#));
        assert_eq!(topics, vec![("news".into(), 7), ("bills".into(), 42)]);
    }
}
