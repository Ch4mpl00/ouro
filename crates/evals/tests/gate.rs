// A port of packages/agent/src/judging/gate.test.ts: the patch glued onto a
// recorded prompt, and the per-axis gate verdict.
use agent::skills::{PATCH_MARKER, append_patch};
use evals::gate::{ChatMessage, GateVerdict, grade_axis, patch_messages};
use evals::noise::NoiseAxis;
use serde_json::json;

fn close(a: f64, b: f64, places: i32) -> bool {
    (a - b).abs() < 10f64.powi(-places) / 2.0
}

// ── appendPatch ──

#[test]
fn appends_the_patch_after_the_body_with_a_marker() {
    assert_eq!(append_patch("BODY", "extra rule"), format!("BODY\n\n{PATCH_MARKER}\nextra rule\n"));
}

#[test]
fn is_a_no_op_for_an_empty_or_whitespace_patch() {
    assert_eq!(append_patch("BODY", ""), "BODY");
    assert_eq!(append_patch("BODY", "   \n "), "BODY");
}

#[test]
fn trims_trailing_whitespace_on_the_body_so_the_marker_lands_cleanly() {
    assert_eq!(append_patch("BODY\n\n", "p"), format!("BODY\n\n{PATCH_MARKER}\np\n"));
}

// ── patchMessages ──

fn msg(role: &str, content: serde_json::Value) -> ChatMessage {
    ChatMessage { role: role.into(), content }
}

#[test]
fn appends_to_the_first_system_message_only_leaving_the_rest_untouched() {
    let msgs = vec![msg("system", json!("CONTRACT")), msg("user", json!("candidates"))];
    let out = patch_messages(&msgs, "be terse").unwrap();
    assert_eq!(out[0].content, json!(format!("CONTRACT\n\n{PATCH_MARKER}\nbe terse\n")));
    assert_eq!(out[1], msg("user", json!("candidates")));
    // The original is not mutated.
    assert_eq!(msgs[0].content, json!("CONTRACT"));
}

#[test]
fn stringifies_non_string_system_content_before_appending() {
    let out = patch_messages(&[msg("system", json!({ "a": 1 }))], "p").unwrap();
    assert_eq!(out[0].content, json!(format!("{{\"a\":1}}\n\n{PATCH_MARKER}\np\n")));
}

#[test]
fn throws_when_there_is_no_system_message() {
    let err = patch_messages(&[msg("user", json!("x"))], "p").unwrap_err();
    assert!(err.to_string().contains("no system message"), "{err}");
}

// ── gradeAxis ──

const K: f64 = 2.0;

#[test]
fn calls_a_clear_lift_above_the_threshold_an_improvement() {
    // before = single stored score; after = S=2 fresh judgements.
    let g = grade_axis(NoiseAxis::Composition, Some(0.6), &[0.9, 0.9], Some(0.02), K);
    assert!(close(g.delta.unwrap(), 0.3, 4));
    // 2·0.02·√(1.5) ≈ 0.049
    assert!(close(g.threshold.unwrap(), 0.049, 3));
    assert_eq!(g.before_n, 1);
    assert_eq!(g.verdict, GateVerdict::Improve);
}

#[test]
fn calls_a_drop_below_minus_threshold_a_regression() {
    let g = grade_axis(NoiseAxis::Composition, Some(0.9), &[0.6, 0.6], Some(0.02), K);
    assert_eq!(g.verdict, GateVerdict::Regress);
}

#[test]
fn treats_a_delta_within_the_threshold_as_noise() {
    // |Δ| = 0.01 < 0.12 threshold
    let g = grade_axis(NoiseAxis::Process, Some(0.71), &[0.73, 0.71], Some(0.05), K);
    assert_eq!(g.verdict, GateVerdict::Noise);
}

#[test]
fn abstains_when_no_sigma_baseline_is_available() {
    let g = grade_axis(NoiseAxis::Composition, Some(0.6), &[0.9], None, K);
    assert_eq!(g.verdict, GateVerdict::NoBaseline);
    assert!(close(g.delta.unwrap(), 0.3, 4));
    assert_eq!(g.threshold, None);
}

#[test]
fn is_na_when_the_before_score_or_the_after_samples_are_missing() {
    assert_eq!(grade_axis(NoiseAxis::Coverage, None, &[0.5], Some(0.05), K).verdict, GateVerdict::NotApplicable);
    assert_eq!(grade_axis(NoiseAxis::Coverage, Some(0.5), &[], Some(0.05), K).verdict, GateVerdict::NotApplicable);
}

#[test]
fn widens_the_threshold_as_s_shrinks() {
    let s1 = grade_axis(NoiseAxis::Coverage, Some(0.5), &[0.6], Some(0.05), K).threshold.unwrap(); // S=1 → √2
    let s2 = grade_axis(NoiseAxis::Coverage, Some(0.5), &[0.6, 0.6], Some(0.05), K).threshold.unwrap(); // S=2 → √1.5
    assert!(s1 > s2);
    assert!(close(s1, 2.0 * 0.05 * 2f64.sqrt(), 3));
}
