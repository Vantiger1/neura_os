//! Forecast ledger: Neura keeps score on her own predictions.
//!
//! Every time a specialist predicts something checkable ("you usually dip
//! around 15:00", "this experiment will help", "you'll open the app before
//! noon"), it records a probability here. When the outcome is known, the
//! forecast is scored with the Brier rule. The ledger then answers two
//! questions the Autonomy Kernel already has a slot for:
//!
//! * how calibrated is this specialist (bins of predicted vs. observed), and
//! * what reputation multiplier does that earn, bounded to the kernel's
//!   existing `[0.75, 1.15]` range so calibration can nudge but never
//!   override attention, privacy or consent checks.
//!
//! Nothing stored here carries content: a forecast is an id, a source label,
//! a probability and two timestamps.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::unit;

/// Reputation bounds mirror `NeuraAutonomyKernel.{min,max}SpecialistReputation`.
pub const MIN_REPUTATION: f64 = 0.75;
pub const MAX_REPUTATION: f64 = 1.15;
/// Brier score of always predicting 0.5; the "no skill" reference.
pub const NO_SKILL_BRIER: f64 = 0.25;
/// Scored forecasts needed before the multiplier leaves neutral.
pub const MIN_SCORED_FOR_REPUTATION: usize = 10;
/// Maximum scored forecasts retained per ledger.
pub const MAX_SCORED: usize = 2_000;
/// Maximum open forecasts retained; the oldest are dropped unscored.
pub const MAX_OPEN: usize = 256;

/// A prediction awaiting resolution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub id: String,
    pub source: String,
    pub probability: f64,
    pub made_at: u64,
    /// After this time an unresolved forecast is dropped without a score.
    pub resolves_by: u64,
}

/// A resolved, scored forecast.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredForecast {
    pub source: String,
    pub probability: f64,
    pub outcome: bool,
    pub brier: f64,
    pub resolved_at: u64,
}

/// One calibration bin for a source.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationBin {
    pub lower: f64,
    pub upper: f64,
    pub mean_probability: f64,
    pub observed_frequency: f64,
    pub count: usize,
}

/// Skill summary for one source.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceSkill {
    pub scored: usize,
    pub mean_brier: f64,
    /// `1 - brier / 0.25`; positive beats a coin flip, negative is worse.
    pub skill_score: f64,
    pub reputation_multiplier: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastLedger {
    open: Vec<Forecast>,
    scored: VecDeque<ScoredForecast>,
}

impl ForecastLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    pub fn scored_count(&self) -> usize {
        self.scored.len()
    }

    /// Record a prediction. A duplicate id replaces the earlier forecast.
    pub fn record(&mut self, forecast: Forecast) {
        let mut forecast = forecast;
        forecast.probability = unit(forecast.probability);
        self.open.retain(|f| f.id != forecast.id);
        self.open.push(forecast);
        while self.open.len() > MAX_OPEN {
            self.open.remove(0);
        }
    }

    /// Resolve a forecast and return its Brier score, or `None` if unknown.
    pub fn resolve(&mut self, id: &str, outcome: bool, now: u64) -> Option<f64> {
        let index = self.open.iter().position(|f| f.id == id)?;
        let forecast = self.open.remove(index);
        let target = if outcome { 1.0 } else { 0.0 };
        let brier = (forecast.probability - target).powi(2);
        self.scored.push_back(ScoredForecast {
            source: forecast.source,
            probability: forecast.probability,
            outcome,
            brier,
            resolved_at: now,
        });
        while self.scored.len() > MAX_SCORED {
            self.scored.pop_front();
        }
        Some(brier)
    }

    /// Drop forecasts whose window has passed. Returns how many were dropped.
    /// Unresolved forecasts earn no penalty; silence is not evidence.
    pub fn expire(&mut self, now: u64) -> usize {
        let before = self.open.len();
        self.open.retain(|f| f.resolves_by >= now);
        before - self.open.len()
    }

    /// Forget everything about one source (user removed it or asked to forget).
    pub fn forget_source(&mut self, source: &str) {
        self.open.retain(|f| f.source != source);
        self.scored.retain(|f| f.source != source);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.open.clear();
        self.scored.clear();
    }

    /// Distinct sources with at least one scored forecast, sorted.
    pub fn sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self.scored.iter().map(|f| f.source.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn skill(&self, source: &str) -> SourceSkill {
        let briers: Vec<f64> = self
            .scored
            .iter()
            .filter(|f| f.source == source)
            .map(|f| f.brier)
            .collect();
        let scored = briers.len();
        if scored == 0 {
            return SourceSkill {
                scored: 0,
                mean_brier: NO_SKILL_BRIER,
                skill_score: 0.0,
                reputation_multiplier: 1.0,
            };
        }
        let mean_brier = briers.iter().sum::<f64>() / scored as f64;
        let skill_score = 1.0 - mean_brier / NO_SKILL_BRIER;
        let reputation_multiplier = if scored < MIN_SCORED_FOR_REPUTATION {
            1.0
        } else {
            // Full skill (Brier 0) earns the maximum; coin-flip skill is
            // neutral; worse than a coin flip decays toward the minimum.
            let adjusted = if skill_score >= 0.0 {
                1.0 + skill_score * (MAX_REPUTATION - 1.0)
            } else {
                1.0 + skill_score.max(-1.0) * (1.0 - MIN_REPUTATION)
            };
            adjusted.clamp(MIN_REPUTATION, MAX_REPUTATION)
        };
        SourceSkill {
            scored,
            mean_brier,
            skill_score,
            reputation_multiplier,
        }
    }

    /// Calibration bins of equal width for one source; empty bins are omitted.
    pub fn calibration(&self, source: &str, bins: usize) -> Vec<CalibrationBin> {
        let bins = bins.clamp(2, 20);
        let width = 1.0 / bins as f64;
        let mut sums = vec![(0.0f64, 0usize, 0usize); bins];
        for f in self.scored.iter().filter(|f| f.source == source) {
            let index = ((f.probability / width).floor() as usize).min(bins - 1);
            let slot = &mut sums[index];
            slot.0 += f.probability;
            slot.1 += usize::from(f.outcome);
            slot.2 += 1;
        }
        sums.iter()
            .enumerate()
            .filter(|(_, (_, _, n))| *n > 0)
            .map(|(i, (p_sum, hits, n))| CalibrationBin {
                lower: i as f64 * width,
                upper: (i + 1) as f64 * width,
                mean_probability: p_sum / *n as f64,
                observed_frequency: *hits as f64 / *n as f64,
                count: *n,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forecast(id: &str, source: &str, p: f64, made: u64) -> Forecast {
        Forecast {
            id: id.into(),
            source: source.into(),
            probability: p,
            made_at: made,
            resolves_by: made + 3_600,
        }
    }

    #[test]
    fn scores_with_brier_and_tracks_counts() {
        let mut ledger = ForecastLedger::new();
        ledger.record(forecast("a", "rhythm", 0.8, 0));
        ledger.record(forecast("b", "rhythm", 0.2, 0));
        assert_eq!(ledger.open_count(), 2);
        let a = ledger.resolve("a", true, 10).unwrap();
        let b = ledger.resolve("b", true, 10).unwrap();
        assert!((a - 0.04).abs() < 1e-9);
        assert!((b - 0.64).abs() < 1e-9);
        assert!(ledger.resolve("missing", true, 10).is_none());
        assert_eq!(ledger.open_count(), 0);
        assert_eq!(ledger.scored_count(), 2);
    }

    #[test]
    fn unknown_or_sparse_sources_are_neutral() {
        let mut ledger = ForecastLedger::new();
        assert_eq!(ledger.skill("none").reputation_multiplier, 1.0);
        for i in 0..(MIN_SCORED_FOR_REPUTATION - 1) {
            ledger.record(forecast(&format!("f{i}"), "s", 0.95, 0));
            ledger.resolve(&format!("f{i}"), true, 1);
        }
        let skill = ledger.skill("s");
        assert_eq!(skill.reputation_multiplier, 1.0);
        assert!(skill.skill_score > 0.9);
    }

    #[test]
    fn well_calibrated_source_earns_bounded_bonus() {
        let mut ledger = ForecastLedger::new();
        for i in 0..40 {
            ledger.record(forecast(&format!("f{i}"), "good", 0.9, 0));
            ledger.resolve(&format!("f{i}"), i % 10 != 0, 1);
        }
        let skill = ledger.skill("good");
        assert!(skill.reputation_multiplier > 1.0);
        assert!(skill.reputation_multiplier <= MAX_REPUTATION);
    }

    #[test]
    fn overconfident_wrong_source_decays_to_minimum() {
        let mut ledger = ForecastLedger::new();
        for i in 0..20 {
            ledger.record(forecast(&format!("f{i}"), "bad", 0.95, 0));
            ledger.resolve(&format!("f{i}"), false, 1);
        }
        let skill = ledger.skill("bad");
        assert!(skill.skill_score < -1.0);
        assert_eq!(skill.reputation_multiplier, MIN_REPUTATION);
    }

    #[test]
    fn calibration_bins_report_observed_frequency() {
        let mut ledger = ForecastLedger::new();
        for i in 0..10 {
            ledger.record(forecast(&format!("h{i}"), "s", 0.7, 0));
            ledger.resolve(&format!("h{i}"), i < 7, 1);
        }
        for i in 0..10 {
            ledger.record(forecast(&format!("l{i}"), "s", 0.2, 0));
            ledger.resolve(&format!("l{i}"), i < 2, 1);
        }
        let bins = ledger.calibration("s", 5);
        assert_eq!(bins.len(), 2);
        let low = bins.iter().find(|b| b.lower < 0.3).unwrap();
        let high = bins.iter().find(|b| b.lower >= 0.6).unwrap();
        assert!((low.observed_frequency - 0.2).abs() < 1e-9);
        assert!((high.observed_frequency - 0.7).abs() < 1e-9);
        assert_eq!(low.count + high.count, 20);
    }

    #[test]
    fn expiry_drops_without_scoring_and_forget_removes_source() {
        let mut ledger = ForecastLedger::new();
        ledger.record(forecast("old", "s", 0.9, 0));
        ledger.record(forecast("new", "s", 0.9, 10_000));
        assert_eq!(ledger.expire(5_000), 1);
        assert_eq!(ledger.open_count(), 1);
        assert_eq!(ledger.scored_count(), 0);

        ledger.resolve("new", true, 10_001);
        ledger.record(forecast("other", "t", 0.4, 10_000));
        ledger.forget_source("s");
        assert_eq!(ledger.scored_count(), 0);
        assert_eq!(ledger.open_count(), 1);
        ledger.clear();
        assert_eq!(ledger.open_count(), 0);
    }

    #[test]
    fn duplicate_ids_replace_and_probability_is_clamped() {
        let mut ledger = ForecastLedger::new();
        ledger.record(forecast("x", "s", 1.7, 0));
        ledger.record(forecast("x", "s", 0.3, 0));
        assert_eq!(ledger.open_count(), 1);
        let brier = ledger.resolve("x", true, 1).unwrap();
        assert!((brier - 0.49).abs() < 1e-9);
    }

    #[test]
    fn open_ledger_is_bounded() {
        let mut ledger = ForecastLedger::new();
        for i in 0..(MAX_OPEN + 10) {
            ledger.record(forecast(&format!("f{i}"), "s", 0.5, 0));
        }
        assert_eq!(ledger.open_count(), MAX_OPEN);
    }
}
