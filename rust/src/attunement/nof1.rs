//! Tiny experiments: single-person (N-of-1) trials over existing check-ins.
//!
//! The user picks one thing to try ("a ten-minute walk after lunch"), Neura
//! lays out alternating baseline / intervention blocks, and the only data
//! collected are the mood or energy scores the user already records. The
//! analysis answers one question in plain terms: did the scores move, and
//! how sure can we be?
//!
//! Method: difference of phase means, with a stratified bootstrap that
//! resamples within each block (so block structure and any autocorrelation
//! inside a block are respected) and a per-cycle consistency check. The
//! random source is a seeded xorshift so results are reproducible and the
//! crate stays dependency-free. A verdict of "too little data" is a first
//! class answer, not a failure.

/// Which arm a block belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Baseline,
    Intervention,
}

/// One contiguous block of scores collected under a single phase.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub phase: Phase,
    pub scores: Vec<f64>,
}

/// Plain-language outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The interval excludes zero on the positive side by at least the
    /// minimal important difference.
    Helped,
    /// The interval excludes zero on the negative side.
    Hurt,
    /// Enough data, but the interval straddles zero.
    NoClearEffect,
    /// Fewer completed cycles or scores than the design requires.
    TooLittleData,
}

/// Analysis settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExperimentConfig {
    /// Completed baseline+intervention pairs required.
    pub min_cycles: usize,
    /// Scores required in every block.
    pub min_scores_per_block: usize,
    /// Smallest mean change worth calling "helped".
    pub minimal_important_difference: f64,
    /// Two-sided interval width, e.g. `0.90`.
    pub interval: f64,
    pub bootstrap_samples: usize,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            min_cycles: 2,
            min_scores_per_block: 3,
            minimal_important_difference: 0.5,
            interval: 0.90,
            bootstrap_samples: 2_000,
        }
    }
}

/// Result of analysing one experiment.
#[derive(Clone, Debug, PartialEq)]
pub struct ExperimentResult {
    pub verdict: Verdict,
    /// Mean intervention score minus mean baseline score.
    pub effect: f64,
    pub interval_low: f64,
    pub interval_high: f64,
    /// Bootstrap fraction of resamples with a positive effect.
    pub probability_positive: f64,
    /// Fraction of completed cycles whose own difference agrees in sign
    /// with the overall effect. Low values mean the effect was not steady.
    pub consistency: f64,
    pub cycles_completed: usize,
    pub baseline_scores: usize,
    pub intervention_scores: usize,
    /// Why a verdict was limited, in content-free terms.
    pub notes: Vec<&'static str>,
}

/// Analyse blocks in the order they were run. `seed` fixes the bootstrap.
pub fn analyze(blocks: &[Block], config: &ExperimentConfig, seed: u64) -> ExperimentResult {
    let mut notes = Vec::new();
    let baseline: Vec<f64> = blocks
        .iter()
        .filter(|b| b.phase == Phase::Baseline)
        .flat_map(|b| b.scores.iter().copied())
        .collect();
    let intervention: Vec<f64> = blocks
        .iter()
        .filter(|b| b.phase == Phase::Intervention)
        .flat_map(|b| b.scores.iter().copied())
        .collect();

    let cycles = completed_cycles(blocks, config.min_scores_per_block);
    let short_block = blocks
        .iter()
        .any(|b| b.scores.len() < config.min_scores_per_block);
    if short_block {
        notes.push("at least one block has fewer scores than required");
    }

    let too_little =
        cycles.len() < config.min_cycles || baseline.is_empty() || intervention.is_empty();
    let effect = if too_little {
        0.0
    } else {
        mean(&intervention) - mean(&baseline)
    };

    if too_little {
        notes.push("not enough completed baseline/intervention cycles yet");
        return ExperimentResult {
            verdict: Verdict::TooLittleData,
            effect,
            interval_low: 0.0,
            interval_high: 0.0,
            probability_positive: 0.5,
            consistency: 0.0,
            cycles_completed: cycles.len(),
            baseline_scores: baseline.len(),
            intervention_scores: intervention.len(),
            notes,
        };
    }

    let usable: Vec<&Block> = blocks
        .iter()
        .filter(|b| b.scores.len() >= config.min_scores_per_block)
        .collect();
    let mut rng = XorShift::new(seed);
    let samples = config.bootstrap_samples.max(200);
    let mut diffs = Vec::with_capacity(samples);
    for _ in 0..samples {
        let mut b_sum = 0.0;
        let mut b_n = 0usize;
        let mut i_sum = 0.0;
        let mut i_n = 0usize;
        for block in &usable {
            for _ in 0..block.scores.len() {
                let pick = block.scores[rng.below(block.scores.len())];
                match block.phase {
                    Phase::Baseline => {
                        b_sum += pick;
                        b_n += 1;
                    }
                    Phase::Intervention => {
                        i_sum += pick;
                        i_n += 1;
                    }
                }
            }
        }
        diffs.push(i_sum / i_n as f64 - b_sum / b_n as f64);
    }
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let tail = (1.0 - config.interval.clamp(0.5, 0.999)) / 2.0;
    let interval_low = quantile(&diffs, tail);
    let interval_high = quantile(&diffs, 1.0 - tail);
    let probability_positive =
        diffs.iter().filter(|d| **d > 0.0).count() as f64 / diffs.len() as f64;

    let agreeing = cycles
        .iter()
        .filter(|(b, i)| (i - b).signum() == effect.signum() && (i - b).abs() > 1e-9)
        .count();
    let consistency = agreeing as f64 / cycles.len() as f64;

    let verdict = if interval_low > config.minimal_important_difference {
        Verdict::Helped
    } else if interval_high < -config.minimal_important_difference {
        Verdict::Hurt
    } else {
        Verdict::NoClearEffect
    };
    if verdict != Verdict::NoClearEffect && consistency < 0.75 {
        notes.push("the effect was not steady across cycles");
    }

    ExperimentResult {
        verdict,
        effect,
        interval_low,
        interval_high,
        probability_positive,
        consistency,
        cycles_completed: cycles.len(),
        baseline_scores: baseline.len(),
        intervention_scores: intervention.len(),
        notes,
    }
}

/// Pairs each baseline block with the intervention block that follows it,
/// returning `(baseline_mean, intervention_mean)` for every completed pair.
fn completed_cycles(blocks: &[Block], min_scores: usize) -> Vec<(f64, f64)> {
    let mut cycles = Vec::new();
    let mut pending_baseline: Option<f64> = None;
    for block in blocks {
        if block.scores.len() < min_scores {
            continue;
        }
        match block.phase {
            Phase::Baseline => pending_baseline = Some(mean(&block.scores)),
            Phase::Intervention => {
                if let Some(b) = pending_baseline.take() {
                    cycles.push((b, mean(&block.scores)));
                }
            }
        }
    }
    cycles
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

/// Small deterministic generator; adequate for bootstrap resampling.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed.max(1) ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(phase: Phase, scores: &[f64]) -> Block {
        Block {
            phase,
            scores: scores.to_vec(),
        }
    }

    fn abab(b1: &[f64], i1: &[f64], b2: &[f64], i2: &[f64]) -> Vec<Block> {
        vec![
            block(Phase::Baseline, b1),
            block(Phase::Intervention, i1),
            block(Phase::Baseline, b2),
            block(Phase::Intervention, i2),
        ]
    }

    #[test]
    fn clear_improvement_is_called_helped() {
        let blocks = abab(
            &[4.0, 5.0, 4.5, 4.0, 5.0],
            &[7.0, 6.5, 7.5, 7.0, 6.0],
            &[4.5, 5.0, 4.0, 5.0, 4.5],
            &[7.0, 7.5, 6.5, 7.0, 7.0],
        );
        let result = analyze(&blocks, &ExperimentConfig::default(), 7);
        assert_eq!(result.verdict, Verdict::Helped);
        assert!(result.effect > 2.0, "effect {}", result.effect);
        assert!(result.interval_low > 1.0);
        assert!(result.probability_positive > 0.99);
        assert_eq!(result.cycles_completed, 2);
        assert_eq!(result.consistency, 1.0);
        assert!(result.notes.is_empty(), "{:?}", result.notes);
    }

    #[test]
    fn no_difference_is_not_overclaimed() {
        let blocks = abab(
            &[5.0, 6.0, 5.0, 6.0, 5.5],
            &[5.5, 5.0, 6.0, 5.0, 6.0],
            &[6.0, 5.0, 5.5, 6.0, 5.0],
            &[5.0, 6.0, 5.5, 5.0, 6.0],
        );
        let result = analyze(&blocks, &ExperimentConfig::default(), 7);
        assert_eq!(result.verdict, Verdict::NoClearEffect);
        assert!(result.interval_low < 0.0 && result.interval_high > 0.0);
        assert!(result.probability_positive > 0.2 && result.probability_positive < 0.8);
    }

    #[test]
    fn worsening_is_called_hurt() {
        let blocks = abab(
            &[7.0, 7.5, 7.0, 8.0, 7.0],
            &[4.0, 4.5, 4.0, 3.5, 4.0],
            &[7.5, 7.0, 7.0, 7.5, 8.0],
            &[4.0, 4.0, 3.5, 4.5, 4.0],
        );
        let result = analyze(&blocks, &ExperimentConfig::default(), 11);
        assert_eq!(result.verdict, Verdict::Hurt);
        assert!(result.effect < -2.5);
    }

    #[test]
    fn one_cycle_is_too_little_data() {
        let blocks = vec![
            block(Phase::Baseline, &[4.0, 4.0, 4.0]),
            block(Phase::Intervention, &[8.0, 8.0, 8.0]),
        ];
        let result = analyze(&blocks, &ExperimentConfig::default(), 1);
        assert_eq!(result.verdict, Verdict::TooLittleData);
        assert_eq!(result.cycles_completed, 1);
        assert!(result
            .notes
            .contains(&"not enough completed baseline/intervention cycles yet"));
    }

    #[test]
    fn short_blocks_do_not_count_as_cycles() {
        let blocks = vec![
            block(Phase::Baseline, &[4.0, 4.0, 4.0]),
            block(Phase::Intervention, &[8.0, 8.0]),
            block(Phase::Baseline, &[4.0, 4.0, 4.0]),
            block(Phase::Intervention, &[8.0, 8.0, 8.0]),
        ];
        let result = analyze(&blocks, &ExperimentConfig::default(), 1);
        assert_eq!(result.verdict, Verdict::TooLittleData);
        assert_eq!(result.cycles_completed, 1);
        assert!(result
            .notes
            .contains(&"at least one block has fewer scores than required"));
    }

    #[test]
    fn inconsistent_cycles_are_flagged() {
        let blocks = abab(
            &[4.0, 4.0, 4.0, 4.0],
            &[9.0, 9.0, 9.0, 9.0],
            &[6.0, 6.0, 6.0, 6.0],
            &[5.5, 5.5, 5.5, 5.5],
        );
        let result = analyze(&blocks, &ExperimentConfig::default(), 3);
        assert_eq!(result.consistency, 0.5);
        if result.verdict != Verdict::NoClearEffect {
            assert!(result
                .notes
                .contains(&"the effect was not steady across cycles"));
        }
    }

    #[test]
    fn same_seed_is_reproducible_and_seeds_differ_only_slightly() {
        let blocks = abab(
            &[4.0, 5.0, 4.5, 4.0, 5.0],
            &[6.0, 5.5, 6.5, 6.0, 5.0],
            &[4.5, 5.0, 4.0, 5.0, 4.5],
            &[6.0, 6.5, 5.5, 6.0, 6.0],
        );
        let a = analyze(&blocks, &ExperimentConfig::default(), 42);
        let b = analyze(&blocks, &ExperimentConfig::default(), 42);
        let c = analyze(&blocks, &ExperimentConfig::default(), 43);
        assert_eq!(a, b);
        assert_eq!(a.verdict, c.verdict);
        assert!((a.interval_low - c.interval_low).abs() < 0.3);
    }
}
