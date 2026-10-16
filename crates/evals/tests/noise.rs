// A port of packages/agent/src/judging/noise.test.ts: repeated verdicts of
// one node folded into per-axis noise, rolled up per axis.
use evals::noise::{
    NodeNoise, NodeNoiseMeta, NoiseAxis, NoiseContext, collect_node_noise, extract_axis_scores, percentile,
    sample_std_dev, summarize_noise,
};
use evals::schema::{Axis, AxisResult, Label, NodeJudgement, Scorecard};

fn close(a: f64, b: f64, places: i32) -> bool {
    (a - b).abs() < 10f64.powi(-places) / 2.0
}

// A planner verdict with given process / query_formulation values (Some(None)
// = the axis was n/a; None = absent).
fn planner_verdict(process: Option<Option<f64>>, qf: Option<Option<f64>>) -> NodeJudgement {
    let mut axes = Vec::new();
    for (axis, v) in [(Axis::Process, process), (Axis::QueryFormulation, qf)] {
        let Some(v) = v else { continue };
        axes.push(AxisResult {
            axis,
            applicable: v.is_some(),
            score: v,
            label: if v.is_none() { Label::NotApplicable } else { Label::Ok },
            rationale: String::new(),
            evidence: String::new(),
        });
    }
    NodeJudgement { scorecard: Scorecard { axes, overall_note: String::new() }, faithfulness: None }
}

fn meta(id: &str, label: &str) -> NodeNoiseMeta {
    NodeNoiseMeta { observation_id: id.into(), label: label.into(), kind: "planner".into(), skill: "planner".into() }
}

#[test]
fn returns_the_numeric_score_for_applicable_axes_none_otherwise() {
    let scores = extract_axis_scores(&planner_verdict(Some(Some(0.7)), Some(None)));
    assert_eq!(scores.get(NoiseAxis::Process), Some(0.7));
    assert_eq!(scores.get(NoiseAxis::QueryFormulation), None); // n/a → None
    assert_eq!(scores.get(NoiseAxis::Coverage), None); // absent → None
    assert_eq!(scores.get(NoiseAxis::Faithfulness), None); // planner has no faithfulness
}

#[test]
fn treats_applicable_false_as_none_even_if_a_number_is_present() {
    let v = NodeJudgement {
        scorecard: Scorecard {
            axes: vec![AxisResult {
                axis: Axis::Process,
                applicable: false,
                score: Some(0.9),
                label: Label::NotApplicable,
                rationale: String::new(),
                evidence: String::new(),
            }],
            overall_note: String::new(),
        },
        faithfulness: None,
    };
    assert_eq!(extract_axis_scores(&v).get(NoiseAxis::Process), None);
}

#[test]
fn is_zero_for_fewer_than_two_samples() {
    assert_eq!(sample_std_dev(&[]), 0.0);
    assert_eq!(sample_std_dev(&[0.5]), 0.0);
}

#[test]
fn computes_the_n_minus_1_sample_stddev() {
    // Mean 0.5, deviations ±0.1 → var = (0.01+0.01)/1 = 0.02 → σ ≈ 0.1414.
    assert!(close(sample_std_dev(&[0.4, 0.6]), 0.141421, 5));
}

#[test]
fn interpolates_percentiles() {
    assert!(close(percentile(&[0.0, 0.1, 0.2, 0.3], 90.0), 0.27, 5));
    assert_eq!(percentile(&[0.05], 90.0), 0.05);
}

#[test]
fn buckets_numeric_samples_and_counts_na_flips_per_axis() {
    let node = collect_node_noise(
        meta("obs1", "attempt-1"),
        &[
            planner_verdict(Some(Some(0.6)), Some(Some(0.8))),
            planner_verdict(Some(Some(0.7)), Some(None)), // qf flips to n/a
            planner_verdict(Some(Some(0.65)), Some(Some(0.8))),
        ],
    );
    assert_eq!(node.runs, 3);
    let proc = node.axes.iter().find(|a| a.axis == NoiseAxis::Process).unwrap();
    assert_eq!(proc.scores, [0.6, 0.7, 0.65]);
    assert_eq!(proc.na_count, 0);
    let qf = node.axes.iter().find(|a| a.axis == NoiseAxis::QueryFormulation).unwrap();
    assert_eq!(qf.scores, [0.8, 0.8]);
    assert_eq!(qf.na_count, 1);
    // Axes the node never touched are omitted from the breakdown.
    assert!(node.axes.iter().all(|a| a.axis != NoiseAxis::Coverage));
}

fn ctx(model: &str, provider: &str) -> NoiseContext {
    NoiseContext { model: model.into(), provider: provider.into(), prompt_version: "n3".into(), runs: 2 }
}

#[test]
fn rolls_per_node_sigmas_into_a_pooled_per_axis_noise_floor() {
    let nodes: Vec<NodeNoise> = vec![
        // σ ≈ 0.1414
        collect_node_noise(
            meta("o1", "a"),
            &[planner_verdict(Some(Some(0.4)), None), planner_verdict(Some(Some(0.6)), None)],
        ),
        // σ = 0
        collect_node_noise(
            meta("o2", "b"),
            &[planner_verdict(Some(Some(0.5)), None), planner_verdict(Some(Some(0.5)), None)],
        ),
    ];
    let report = summarize_noise(ctx("gpt-5.4", "openai"), &nodes);
    assert_eq!(report.sample_nodes, 2);
    assert_eq!(report.total_judge_calls, 4);
    let proc = report.axes.iter().find(|a| a.axis == NoiseAxis::Process).unwrap();
    assert_eq!(proc.nodes, 2);
    // pooled = sqrt(mean(0.02, 0)) = sqrt(0.01) = 0.1
    assert!(close(proc.pooled_sigma, 0.1, 4));
    assert!(close(proc.max_sigma, 0.1414, 3));
    assert!(close(proc.mean_score, 0.5, 3));
    assert_eq!(proc.applicability_flips, 0);
}

#[test]
fn flags_applicability_flips_and_omits_never_scored_axes() {
    let nodes = vec![collect_node_noise(
        meta("o1", "a"),
        &[planner_verdict(Some(Some(0.6)), Some(Some(0.8))), planner_verdict(Some(Some(0.6)), Some(None))],
    )];
    let report = summarize_noise(ctx("m", "openai"), &nodes);
    let qf = report.axes.iter().find(|a| a.axis == NoiseAxis::QueryFormulation).unwrap();
    assert_eq!(qf.applicability_flips, 1);
    // coverage/composition/faithfulness never scored → omitted
    let mut axes: Vec<&str> = report.axes.iter().map(|a| a.axis.as_str()).collect();
    axes.sort();
    assert_eq!(axes, ["process", "query_formulation"]);
}
