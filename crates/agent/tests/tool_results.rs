// Port of src/tool-results.test.ts: exact values, UTF-8 thresholds, fresh refs.
use agent::{
    session::{Format, WorkingMemory},
    tool_results::*,
};
#[test]
fn small_results_are_exact_and_format_is_detected() {
    for (text, format) in [
        ("", Format::Text),
        ("plain text ${key}", Format::Text),
        ("{\"answer\":42}", Format::Json),
        ("null", Format::Json),
        ("{broken JSON", Format::Text),
    ] {
        let memory = WorkingMemory::default();
        let result = store_tool_result(&memory, text);
        assert_eq!(result.shown, text);
        assert_eq!(result.format, format);
        assert!(!result.truncated);
        assert_eq!(result.size_bytes, text.len());
        assert_eq!(memory.get(&result.memory_key).unwrap(), text);
        assert!(result.to_json().get("preview").is_none());
    }
}
#[test]
fn byte_threshold_and_unicode_preview() {
    let memory = WorkingMemory::default();
    assert!(!store_tool_result(&memory, &"x".repeat(TOOL_RESULT_INLINE_MAX_BYTES)).truncated);
    for value in ["x".repeat(8001), "я🦊".repeat(2000)] {
        let result = store_tool_result(&memory, &value);
        assert!(result.truncated);
        assert!(result.to_json().get("content").is_none());
        assert!(result.shown.len() <= TOOL_RESULT_PREVIEW_MAX_BYTES);
        assert!(!result.shown.contains('�'));
        assert!(value.starts_with(&result.shown));
        assert_eq!(memory.get(&result.memory_key).unwrap(), value);
    }
}
#[test]
fn distinct_keys_and_error_detection() {
    let memory = WorkingMemory::default();
    let keys: std::collections::HashSet<_> = (0..20).map(|_| store_tool_result(&memory, "same").memory_key).collect();
    assert_eq!(keys.len(), 20);
    assert_eq!(memory.list().len(), 20);
    assert!(is_tool_error("[tool error] bad"));
    assert!(is_tool_error("[write_skill error] bad"));
    assert!(!is_tool_error("a [tool error] bad"));
    assert!(!is_tool_error("[tool error\n] bad"));
}
