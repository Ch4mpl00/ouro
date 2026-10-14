use agent::{
    activity::*,
    trace_model::{Observation, Trace},
};
use serde_json::{Value, json};
#[test]
fn projections_match_the_typescript_reference_byte_budgets_and_provenance() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/activity-projection.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let trace: Trace = serde_json::from_value(case["trace"].clone()).unwrap();
        let observations: Vec<Observation> = serde_json::from_value(case["observations"].clone()).unwrap();
        let result = project_trace(&trace, &observations, Some("2026-10-01 00:00:01"), &ProjectionBudget::default());
        assert_eq!(result, case["expected"]);
        assert!(utf16_len(&result.to_string()) <= 12000);
        assert!(!result.to_string().contains("ARTICLE_BODY_ARTICLE_BODY"));
    }
    assert_eq!(clip(&"🦊".repeat(20), 11), fixture["clip"]);
    for case in fixture["notes"].as_array().unwrap() {
        let result = validate_note(case["raw"].as_str().unwrap());
        assert_eq!(result.is_ok(), case["expected"]["ok"]);
        if let Ok(note) = result {
            assert_eq!(note, case["expected"]["note"]);
        }
    }
    let packet = &fixture["cases"][0]["expected"];
    assert_eq!(serde_json::to_value(render_draft(packet, "note")).unwrap(), fixture["draft"]);
    assert_eq!(
        compact(
            &json!({"a":(0..20).map(|_|json!({"body":"BODY".repeat(100)})).collect::<Vec<_>>(),"x":"🦊".repeat(100)}),
            50,
            true,
            3
        ),
        fixture["compact"]
    );
}
