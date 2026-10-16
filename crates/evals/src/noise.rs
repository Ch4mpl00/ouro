// Judge-noise calibration: how much does the SAME judge wobble when it re-scores
// the SAME node, unchanged, K times? That wobble (σ_judge) is the floor under
// the improver's gate — a patch's Δ on an axis is only believable when it clears
// the judge's own repeatability noise. Noise is MODEL-specific (a stronger judge
// is more self-consistent), so we measure it per (model, prompt version) and
// commit the baseline the gate reads.
//
// This module is pure: it turns repeated verdicts of the same nodes into a
// per-axis noise report. The IO (assembling nodes, calling the backend K times,
// writing the baseline file) lives in the judge-noise binary.

use serde::{Deserialize, Serialize};

use crate::schema::{Axis, NodeJudgement};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseAxis {
    QueryFormulation,
    Process,
    Coverage,
    Composition,
    Faithfulness,
}

pub const NOISE_AXES: [NoiseAxis; 5] = [
    NoiseAxis::QueryFormulation,
    NoiseAxis::Process,
    NoiseAxis::Coverage,
    NoiseAxis::Composition,
    NoiseAxis::Faithfulness,
];

impl NoiseAxis {
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseAxis::QueryFormulation => "query_formulation",
            NoiseAxis::Process => "process",
            NoiseAxis::Coverage => "coverage",
            NoiseAxis::Composition => "composition",
            NoiseAxis::Faithfulness => "faithfulness",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<NoiseAxis> {
        NOISE_AXES.into_iter().find(|a| a.as_str() == s).ok_or_else(|| anyhow::anyhow!("unknown axis: {s}"))
    }

    // The scorecard axis this is, or None for faithfulness (its own pass).
    pub fn card_axis(self) -> Option<Axis> {
        match self {
            NoiseAxis::QueryFormulation => Some(Axis::QueryFormulation),
            NoiseAxis::Process => Some(Axis::Process),
            NoiseAxis::Coverage => Some(Axis::Coverage),
            NoiseAxis::Composition => Some(Axis::Composition),
            NoiseAxis::Faithfulness => None,
        }
    }
}

// The five axis values one verdict carries, in NOISE_AXES order — numeric
// when the axis applied and was scored, None when the rubric didn't emit it
// or marked it n/a.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AxisScores(pub [Option<f64>; 5]);

impl AxisScores {
    pub fn get(&self, axis: NoiseAxis) -> Option<f64> {
        self.0[NOISE_AXES.iter().position(|a| *a == axis).unwrap_or(0)]
    }

    pub fn from_db(s: &agent::db::Scores) -> AxisScores {
        AxisScores([s.query_formulation, s.process, s.coverage, s.composition, s.faithfulness])
    }

    pub fn to_db(&self) -> agent::db::Scores {
        let [query_formulation, process, coverage, composition, faithfulness] = self.0;
        agent::db::Scores { query_formulation, process, coverage, composition, faithfulness }
    }
}

pub fn extract_axis_scores(verdict: &NodeJudgement) -> AxisScores {
    AxisScores(NOISE_AXES.map(|axis| match axis.card_axis() {
        Some(card) => {
            verdict.scorecard.axes.iter().find(|a| a.axis == card).filter(|a| a.applicable).and_then(|a| a.score)
        }
        None => verdict.faithfulness.as_ref().filter(|f| f.applicable).and_then(|f| f.score),
    }))
}

// Three-state view of ONE axis in ONE verdict, the distinction the noise
// collector needs: a number (scored), Na (the rubric OWNS this axis but
// returned n/a — a flip if it's numeric elsewhere), or Absent (the rubric
// doesn't emit this axis at all, e.g. coverage on a planner node — ignore it).
enum AxisObservation {
    Score(f64),
    Na,
    Absent,
}

fn classify_axis(verdict: &NodeJudgement, axis: NoiseAxis) -> AxisObservation {
    let (applicable, score) = match axis.card_axis() {
        None => match &verdict.faithfulness {
            None => return AxisObservation::Absent,
            Some(f) => (f.applicable, f.score),
        },
        Some(card) => match verdict.scorecard.axes.iter().find(|a| a.axis == card) {
            None => return AxisObservation::Absent,
            Some(a) => (a.applicable, a.score),
        },
    };
    match (applicable, score) {
        (true, Some(s)) => AxisObservation::Score(s),
        _ => AxisObservation::Na,
    }
}

// One axis observed across the K repeats of a single node: the numeric samples
// (n/a repeats excluded) plus how many repeats were n/a — a flip between numeric
// and n/a is itself instability the gate must know about.
#[derive(Clone, Debug, PartialEq)]
pub struct AxisSamples {
    pub axis: NoiseAxis,
    pub scores: Vec<f64>,
    pub na_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeNoiseMeta {
    pub observation_id: String,
    pub label: String,
    pub kind: String,
    pub skill: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeNoise {
    pub meta: NodeNoiseMeta,
    pub runs: usize,
    pub axes: Vec<AxisSamples>,
}

// Fold K verdicts of the SAME node into per-axis samples.
pub fn collect_node_noise(meta: NodeNoiseMeta, verdicts: &[NodeJudgement]) -> NodeNoise {
    let mut per_axis: Vec<AxisSamples> =
        NOISE_AXES.iter().map(|a| AxisSamples { axis: *a, scores: Vec::new(), na_count: 0 }).collect();
    for verdict in verdicts {
        for bucket in per_axis.iter_mut() {
            match classify_axis(verdict, bucket.axis) {
                // The rubric doesn't own this axis — not noise.
                AxisObservation::Absent => {}
                AxisObservation::Na => bucket.na_count += 1,
                AxisObservation::Score(s) => bucket.scores.push(s),
            }
        }
    }
    NodeNoise {
        meta,
        runs: verdicts.len(),
        // Drop axes that never applied to this node (no numeric sample, no n/a
        // noise to report) so the breakdown only shows axes the node owns.
        axes: per_axis.into_iter().filter(|s| !s.scores.is_empty() || s.na_count > 0).collect(),
    }
}

// ─── aggregate stats ─────────────────────────────────────────────────

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

// Sample stddev (n-1). 0 for fewer than two samples (no spread observable).
pub fn sample_std_dev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let ss: f64 = xs.iter().map(|x| (x - m).powi(2)).sum();
    (ss / (xs.len() - 1) as f64).sqrt()
}

// Linear-interpolated percentile over an unsorted slice.
pub fn percentile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = (p / 100.0) * (sorted.len() - 1) as f64;
    let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] * (hi as f64 - idx) + sorted[hi] * (idx - lo as f64)
}

// JS Math.round: halves go up, toward +∞.
pub fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f + 0.5).floor() / f
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisNoise {
    pub axis: NoiseAxis,
    // Nodes with ≥2 numeric samples (where a stddev is computable).
    pub nodes: usize,
    // sqrt(mean of per-node sample variance) — the pooled repeatability σ, the
    // headline number the gate uses as the noise floor for this axis.
    pub pooled_sigma: f64,
    pub mean_sigma: f64,
    pub p90_sigma: f64,
    pub max_sigma: f64,
    // Mean score across all numeric samples — context for whether the axis even
    // exercises its range (a near-constant axis has tiny σ for the wrong reason).
    pub mean_score: f64,
    // Nodes where the axis was numeric in some repeats and n/a in others — a
    // distinct, harder-to-gate instability than a wobbling number.
    pub applicability_flips: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoiseReport {
    pub model: String,
    pub provider: String,
    pub prompt_version: String,
    pub runs: usize,
    pub sample_nodes: usize,
    pub total_judge_calls: usize,
    pub axes: Vec<AxisNoise>,
}

pub struct NoiseContext {
    pub model: String,
    pub provider: String,
    pub prompt_version: String,
    pub runs: usize,
}

// Roll per-node samples up into one noise number per axis.
pub fn summarize_noise(ctx: NoiseContext, nodes: &[NodeNoise]) -> NoiseReport {
    let mut axes = Vec::new();
    for axis in NOISE_AXES {
        let samples_per_node: Vec<&AxisSamples> =
            nodes.iter().filter_map(|n| n.axes.iter().find(|a| a.axis == axis)).collect();
        let with_spread: Vec<&&AxisSamples> = samples_per_node.iter().filter(|a| a.scores.len() >= 2).collect();
        if with_spread.is_empty() && samples_per_node.iter().all(|a| a.scores.is_empty()) {
            // Axis never numerically scored across the corpus — omit it.
            continue;
        }
        let sigmas: Vec<f64> = with_spread.iter().map(|a| sample_std_dev(&a.scores)).collect();
        let variances: Vec<f64> = sigmas.iter().map(|s| s * s).collect();
        let all_scores: Vec<f64> = samples_per_node.iter().flat_map(|a| a.scores.iter().copied()).collect();
        let flips = samples_per_node.iter().filter(|a| !a.scores.is_empty() && a.na_count > 0).count();
        axes.push(AxisNoise {
            axis,
            nodes: with_spread.len(),
            pooled_sigma: if variances.is_empty() { 0.0 } else { round(mean(&variances).sqrt(), 4) },
            mean_sigma: if sigmas.is_empty() { 0.0 } else { round(mean(&sigmas), 4) },
            p90_sigma: round(percentile(&sigmas, 90.0), 4),
            max_sigma: if sigmas.is_empty() { 0.0 } else { round(sigmas.iter().copied().fold(f64::MIN, f64::max), 4) },
            mean_score: if all_scores.is_empty() { 0.0 } else { round(mean(&all_scores), 3) },
            applicability_flips: flips,
        });
    }
    NoiseReport {
        model: ctx.model,
        provider: ctx.provider,
        prompt_version: ctx.prompt_version,
        runs: ctx.runs,
        sample_nodes: nodes.len(),
        total_judge_calls: nodes.iter().map(|n| n.runs).sum(),
        axes,
    }
}
