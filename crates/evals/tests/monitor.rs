// A port of packages/agent/src/judging/monitor.test.ts: the live-trend revert
// decision and the patch-lesson surgery.
use evals::monitor::{
    Baseline, RevertDecision, budget_exceeded, count_lessons, decide_revert, remove_lesson, split_lessons,
};

const BASELINE: Baseline = Baseline { mean: 0.7, n: 10.0 };
const K: f64 = 2.0;
const MIN: usize = 5;

#[test]
fn is_insufficient_until_enough_post_ship_traces_accumulate() {
    let v = decide_revert(BASELINE, &[0.1, 0.1, 0.1], Some(0.05), K, MIN); // n=3 < 5
    assert_eq!(v.decision, RevertDecision::Insufficient);
    assert_eq!(v.post_n, 3);
}

#[test]
fn keeps_a_ship_whose_live_trend_holds_at_or_above_baseline() {
    let v = decide_revert(BASELINE, &[0.72, 0.71, 0.73, 0.7, 0.74], Some(0.05), K, MIN);
    assert_eq!(v.decision, RevertDecision::Keep);
}

#[test]
fn keeps_a_flat_trend_a_small_dip_within_the_noise_band_is_not_a_regression() {
    // Post mean ≈ 0.69; drop 0.01 well under 2·0.05·√(1/5+1/10) ≈ 0.055.
    let v = decide_revert(BASELINE, &[0.69; 5], Some(0.05), K, MIN);
    assert_eq!(v.decision, RevertDecision::Keep);
    assert!((v.margin.unwrap() - 2.0 * 0.05 * (1.0f64 / 5.0 + 1.0 / 10.0).sqrt()).abs() < 0.0005);
}

#[test]
fn reverts_a_ship_that_confidently_fell_below_baseline() {
    let v = decide_revert(BASELINE, &[0.4, 0.45, 0.42, 0.38, 0.41], Some(0.05), K, MIN);
    assert_eq!(v.decision, RevertDecision::Revert);
}

#[test]
fn with_no_sigma_baseline_reverts_on_any_below_baseline_mean() {
    assert_eq!(decide_revert(BASELINE, &[0.69; 5], None, K, MIN).decision, RevertDecision::Revert);
    assert_eq!(decide_revert(BASELINE, &[0.7; 5], None, K, MIN).decision, RevertDecision::Keep);
}

const TWO: &str = "- lesson one\n  with a second bullet\n\n- lesson two";

#[test]
fn splits_append_only_blocks_on_the_blank_line_ignoring_empties() {
    assert!(split_lessons("").is_empty());
    assert!(split_lessons("   \n  ").is_empty());
    assert_eq!(count_lessons(TWO), 2);
    assert_eq!(split_lessons(TWO)[0], "- lesson one\n  with a second bullet");
}

#[test]
fn budget_is_exceeded_once_the_count_reaches_the_cap() {
    assert!(!budget_exceeded(TWO, 3));
    assert!(budget_exceeded(TWO, 2));
}

#[test]
fn remove_lesson_drops_one_block_and_keeps_the_rest() {
    assert_eq!(remove_lesson(TWO, "- lesson one\n  with a second bullet"), "- lesson two\n");
}

#[test]
fn remove_lesson_returns_empty_when_the_removed_lesson_was_the_only_one() {
    assert_eq!(remove_lesson("- only lesson\n", "- only lesson"), "");
}

#[test]
fn remove_lesson_is_a_no_op_when_the_lesson_isnt_present() {
    assert_eq!(remove_lesson(TWO, "- not here"), "- lesson one\n  with a second bullet\n\n- lesson two\n");
}
