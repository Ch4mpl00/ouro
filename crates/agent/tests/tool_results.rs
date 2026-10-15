// Port of src/tool-results.test.ts, one test per case.
use agent::{
    session::{Format, MemoryEntry, WorkingMemory},
    tool_results::*,
};

#[test]
fn preserves_the_exact_small_result() {
    for (value, format) in [
        ("", Format::Text),
        ("plain text ${key}", Format::Text),
        ("{\"answer\":42}", Format::Json),
        ("null", Format::Json),
        ("{broken JSON", Format::Text),
    ] {
        let store = WorkingMemory::default();
        let reply = store_tool_result(&store, value);
        assert_eq!(reply.shown, value);
        assert!(!reply.truncated);
        assert_eq!(reply.size_bytes, value.len());
        assert_eq!(reply.format, format);
        assert_eq!(store.get(&reply.memory_key).unwrap(), value);
        assert_eq!(
            store.list(),
            vec![MemoryEntry { key: reply.memory_key.clone(), format: reply.format, size_bytes: reply.size_bytes }]
        );
        let json = reply.to_json();
        assert_eq!(json["content"], value);
        assert!(json.get("preview").is_none());
    }
}

#[test]
fn keeps_the_exact_byte_threshold_inline_and_hides_larger_content() {
    let store = WorkingMemory::default();
    assert!(!store_tool_result(&store, &"x".repeat(TOOL_RESULT_INLINE_MAX_BYTES)).truncated);
    let value = "x".repeat(TOOL_RESULT_INLINE_MAX_BYTES) + "hidden tail";
    let reply = store_tool_result(&store, &value);
    assert!(reply.truncated);
    let json = reply.to_json();
    assert!(json.get("content").is_none());
    assert_eq!(json["preview"], "x".repeat(TOOL_RESULT_PREVIEW_MAX_BYTES));
    assert_eq!(store.get(&reply.memory_key).unwrap(), value);
}

#[test]
fn measures_utf8_bytes_and_never_cuts_a_preview_inside_a_character() {
    let store = WorkingMemory::default();
    let value = "я🦊".repeat(2_000);
    let reply = store_tool_result(&store, &value);
    assert!(reply.truncated);
    assert_eq!(reply.size_bytes, 12_000);
    assert!(reply.shown.len() <= TOOL_RESULT_PREVIEW_MAX_BYTES);
    assert!(!reply.shown.contains('\u{FFFD}'));
    assert!(value.starts_with(&reply.shown));
    assert_eq!(store.get(&reply.memory_key).unwrap(), value);
}

#[test]
fn allocates_distinct_keys_for_identical_outputs() {
    let store = WorkingMemory::default();
    let keys: std::collections::HashSet<_> = (0..20).map(|_| store_tool_result(&store, "same").memory_key).collect();
    assert_eq!(keys.len(), 20);
    assert_eq!(store.list().len(), 20);
}

// Rust-only: the error-reply shape the loop and the workflow branch on.
#[test]
fn recognises_a_tool_error_reply_only_at_the_start() {
    assert!(is_tool_error("[tool error] bad"));
    assert!(is_tool_error("[write_skill error] bad"));
    assert!(!is_tool_error("a [tool error] bad"));
    assert!(!is_tool_error("[tool error\n] bad"));
}
