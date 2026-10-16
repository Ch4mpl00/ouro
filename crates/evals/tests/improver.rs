// A port of packages/agent/src/judging/improver.test.ts: candidate selection,
// the dominant failure mode, and the ship decision.
use std::collections::HashSet;

use agent::db::{JudgementRecord, Scores};
use evals::gate::{AxisGrade, GateVerdict, GatedNode, NodeGateResult};
use evals::improver::{FailureMode, SelectOpts, Taxonomy, decide_ship, dominant_mode, select_candidates};
use evals::noise::NoiseAxis;
use evals::schema::NodeKind;
use serde_json::json;

fn rec(id: &str, coverage: Option<f64>, started_at: &str) -> JudgementRecord {
    JudgementRecord {
        trace_id: format!("t-{id}"),
        observation_id: format!("o-{id}"),
        node_kind: "compose".into(),
        skill: "news-digest".into(),
        scores: Scores { coverage, ..Scores::default() },
        detail: json!({}),
        started_at: started_at.into(),
    }
}

const JUNE: &str = "2026-06-18T00:00:00.000Z";

// Defaults mirroring the locked design: candidate iff score < 0.6 AND
// score < 0.75 − k·σ; holdout = all-time score ≥ 0.85.
fn opts(sigma: f64, recent_since: Option<&str>) -> SelectOpts {
    SelectOpts {
        holdout_size: 5,
        abs_max: 0.6,
        bar: 0.75,
        k: 2.0,
        sigma: Some(sigma),
        holdout_min: 0.85,
        recent_since: recent_since.map(str::to_owned),
    }
}

fn ids(records: &[JudgementRecord]) -> Vec<&str> {
    records.iter().map(|r| r.observation_id.as_str()).collect()
}

#[test]
fn selects_absolute_and_sigma_failures_ascending_and_an_all_time_high_holdout_descending_without_overlap() {
    let records = [
        rec("a", Some(0.2), JUNE),
        rec("b", Some(0.5), JUNE),
        rec("c", Some(0.9), JUNE),
        rec("d", Some(0.95), JUNE),
        rec("e", Some(0.4), JUNE),
    ];
    let s = select_candidates(&records, NoiseAxis::Coverage, &opts(0.05, None));
    // sigmaFloor = 0.75 − 0.1 = 0.65; absMax 0.6 dominates → s < 0.6
    assert_eq!(ids(&s.candidates), ["o-a", "o-e", "o-b"]);
    assert_eq!(ids(&s.holdout), ["o-d", "o-c"]);
}

#[test]
fn the_sigma_term_excludes_near_bar_lows_on_a_noisy_axis() {
    let records = [rec("a", Some(0.4), JUNE), rec("b", Some(0.5), JUNE)];
    // sigma 0.15 → sigmaFloor = 0.75 − 0.3 = 0.45; 0.5 fails absMax but not the σ floor.
    let s = select_candidates(&records, NoiseAxis::Coverage, &opts(0.15, None));
    assert_eq!(ids(&s.candidates), ["o-a"]);
}

#[test]
fn draws_the_cluster_from_the_recent_window_but_the_holdout_from_all_time() {
    let records = [
        rec("old-low", Some(0.3), "2026-01-01T00:00:00.000Z"),
        rec("new-low", Some(0.3), "2026-06-17T00:00:00.000Z"),
        rec("old-high", Some(0.9), "2026-01-01T00:00:00.000Z"),
    ];
    let s = select_candidates(&records, NoiseAxis::Coverage, &opts(0.05, Some("2026-06-01T00:00:00.000Z")));
    assert_eq!(ids(&s.candidates), ["o-new-low"]); // old low dropped
    assert_eq!(ids(&s.holdout), ["o-old-high"]); // height, not recency
}

#[test]
fn ignores_nodes_where_the_axis_is_null() {
    let records = [rec("a", None, JUNE), rec("b", Some(0.3), JUNE), rec("c", None, JUNE)];
    let s = select_candidates(&records, NoiseAxis::Coverage, &opts(0.05, None));
    assert_eq!(ids(&s.candidates), ["o-b"]);
}

#[test]
fn returns_no_candidates_when_the_floor_has_reached_the_ceiling() {
    let records = [rec("a", Some(0.8), JUNE), rec("b", Some(0.9), JUNE)];
    assert!(select_candidates(&records, NoiseAxis::Coverage, &opts(0.05, None)).candidates.is_empty());
}

fn mode(name: &str, description: &str, node_ids: &[&str]) -> FailureMode {
    FailureMode {
        name: name.into(),
        description: description.into(),
        node_ids: node_ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn picks_the_pareto_dominant_mode() {
    let taxonomy = Taxonomy {
        modes: vec![
            mode("omits-item", "omits a concrete item", &["t-a:o-a", "t-b:o-b"]),
            mode("off-contract", "includes off-contract opinion", &["t-c:o-c"]),
        ],
    };
    let valid = set(&["t-a:o-a", "t-b:o-b", "t-c:o-c"]);
    assert_eq!(dominant_mode(&taxonomy, &valid).unwrap().name, "omits-item");
}

#[test]
fn drops_hallucinated_ids_and_re_ranks_on_the_cleaned_counts() {
    // Only one of omits-item's ids is real → off-contract now wins.
    let valid = set(&["t-a:o-a", "t-c:o-c", "t-x:o-x"]);
    let taxonomy = Taxonomy {
        modes: vec![
            mode("omits-item", "x", &["t-a:o-a", "t-ghost:o-ghost"]),
            mode("off-contract", "y", &["t-c:o-c", "t-x:o-x"]),
        ],
    };
    assert_eq!(dominant_mode(&taxonomy, &valid).unwrap().name, "off-contract");
}

#[test]
fn returns_none_when_no_candidate_mapped() {
    let taxonomy = Taxonomy { modes: vec![mode("m", "d", &["ghost"])] };
    assert!(dominant_mode(&taxonomy, &HashSet::new()).is_none());
}

// A gate result for one node with the given per-axis verdicts.
fn gate_result(label: &str, verdicts: &[(NoiseAxis, GateVerdict)]) -> NodeGateResult {
    let grades = verdicts
        .iter()
        .map(|(axis, verdict)| AxisGrade {
            axis: *axis,
            before_n: 3,
            before_mean: Some(0.5),
            after_n: 3,
            after_mean: Some(0.7),
            delta: Some(0.2),
            sigma: Some(0.05),
            threshold: Some(0.1),
            verdict: *verdict,
        })
        .collect();
    NodeGateResult {
        node: GatedNode {
            observation_id: label.into(),
            kind: NodeKind::Compose,
            skill: "news-digest".into(),
            label: label.into(),
        },
        patched_outputs: Vec::new(),
        grades,
    }
}

use GateVerdict::{Improve, Noise, Regress};
use NoiseAxis::{Composition, Coverage};

#[test]
fn accepts_when_the_target_axis_improves_on_the_cluster_and_nothing_regresses() {
    let cluster = [gate_result("c1", &[(Coverage, Improve)]), gate_result("c2", &[(Coverage, Improve)])];
    let holdout = [gate_result("h1", &[(Coverage, Noise)])];
    let d = decide_ship(Coverage, &cluster, &holdout);
    assert!(d.accept);
    assert_eq!(d.cluster_improve, 2);
}

#[test]
fn rejects_when_the_target_axis_does_not_net_improve_on_the_cluster() {
    let cluster = [gate_result("c1", &[(Coverage, Noise)]), gate_result("c2", &[(Coverage, Noise)])];
    assert!(!decide_ship(Coverage, &cluster, &[]).accept);
}

#[test]
fn rejects_on_any_regression_even_a_collateral_axis_on_the_cluster() {
    let cluster = [gate_result("c1", &[(Coverage, Improve), (Composition, Regress)])];
    assert!(!decide_ship(Coverage, &cluster, &[]).accept);
}

#[test]
fn rejects_on_a_holdout_regression() {
    let cluster = [gate_result("c1", &[(Coverage, Improve)])];
    let holdout = [gate_result("h1", &[(Coverage, Regress)])];
    assert!(!decide_ship(Coverage, &cluster, &holdout).accept);
}
