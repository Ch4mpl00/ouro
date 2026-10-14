// Port of src/session-context.test.ts: literal keys, exact text and isolation.
mod support;
use agent::session::{Format, WorkingMemory, format_local_time, render_context};
use proptest::prelude::*;
#[test]
fn owns_environment_and_independent_memory() {
    let first = support::context();
    let second = support::context();
    assert_eq!(first.id, "telegram:42");
    assert_eq!(first.env.timezone, "Europe/Chisinau");
    first.memory.put("news", "one", Format::Text).unwrap();
    assert!(second.memory.list().is_empty());
    assert_eq!(first.memory.get("news").unwrap(), "one");
}
#[test]
fn literal_keys_duplicate_rejection_and_delete() {
    let memory = WorkingMemory::default();
    for key in ["news", "news.raw", "news[0]", "__proto__", "constructor", " news "] {
        memory.put(key, key, Format::Text).unwrap();
    }
    assert!(memory.put("", "value", Format::Text).is_err());
    let before = memory.list();
    assert!(memory.put("news", "overwrite", Format::Json).is_err());
    assert_eq!(memory.list(), before);
    assert_eq!(memory.get("News").unwrap_err().to_string(), "Key \"News\" not found");
    assert_eq!(memory.get("news.raw").unwrap(), "news.raw");
    assert!(!memory.delete("missing"));
    assert!(memory.delete("news"));
    assert!(!memory.delete("news"));
    memory.put("news", "", Format::Json).unwrap();
    assert_eq!(memory.get("news").unwrap(), "");
    assert_eq!(memory.list().last().unwrap().format, Format::Json);
    let mut metadata = memory.list();
    metadata.clear();
    assert_eq!(memory.list().len(), 6);
}
proptest! {
    #[test]
    fn preserves_arbitrary_strings(value in ".{0,500}") {
        let memory = WorkingMemory::default();
        memory.put("key", &value, Format::Json).unwrap();
        prop_assert_eq!(memory.get("key").unwrap(), value.clone());
        prop_assert_eq!(memory.list()[0].size_bytes, value.len());
        prop_assert_eq!(memory.list()[0].format, Format::Json);
    }
}
#[test]
fn context_rendering_has_timezone_watermarks_and_email() {
    let mut env = support::context().env.clone();
    env.user_email = Some("user@example.test".into());
    env.news_last_read_at = Some("yesterday".into());
    let text = render_context(&env).unwrap();
    assert!(text.contains("2026-09-06 15:00 (Europe/Chisinau)"));
    assert!(text.contains("User email: user@example.test"));
    assert!(text.contains("News last read at: yesterday"));
    assert!(text.contains("Tech last read at: never (bootstrap with now - 24h)"));
    assert_eq!(format_local_time("2026-10-14T00:05:00Z".parse().unwrap(), "UTC").unwrap(), "2026-10-14 00:05");
}

#[test]
fn invalid_timezone_is_rejected_like_intl() {
    assert!(format_local_time("2026-10-14T00:05:00Z".parse().unwrap(), "Bad/Zone").is_err());
}

struct BrokenMemory;
impl agent::db::MemoryStore for BrokenMemory {
    fn get(&self, _: &str) -> anyhow::Result<Option<String>> {
        anyhow::bail!("storage failed")
    }
    fn set(&self, _: &str, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("storage failed")
    }
}
#[tokio::test]
async fn environment_does_not_replace_storage_failures_with_bootstrap_watermarks() {
    let deps = agent::session::EnvDeps {
        mcp: std::sync::Arc::new(support::FakeMcp::default()),
        memory: std::sync::Arc::new(BrokenMemory),
        user_email: None,
    };
    let error = agent::session::gather_env_data(&deps, &tokio_util::sync::CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("storage failed"));
}
