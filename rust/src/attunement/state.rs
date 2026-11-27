//! One durable, content-free bundle for everything attunement learns.
//!
//! The Dart side already persists the intelligence controller's state as one
//! JSON string under one prefs key that the data wipe clears. This module
//! gives attunement the same shape: `AttunementState` owns the prosody
//! baseline, bounded rhythm observations per signal, the forecast ledger and
//! open tiny experiments, round-trips through JSON, and answers the one
//! question the Autonomy Kernel needs each evaluation: `hints(..)`.
//!
//! What is stored, exhaustively: floating-point averages, `(time, score)`
//! pairs, probabilities with ids and timestamps, and per-block score lists
//! keyed by a caller-chosen experiment id. No audio, no notes, no titles.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::forecast::{Forecast, ForecastLedger, MIN_SCORED_FOR_REPUTATION};
use super::nof1::{analyze, Block, ExperimentConfig, ExperimentResult, Phase};
use super::prosody::{ProsodyBaseline, ProsodyDelta, ProsodySnapshot};
use super::rhythm::{RhythmModel, RhythmObservation, RhythmSurprise};

/// Schema version written into the JSON so a future change can migrate.
pub const STATE_VERSION: u32 = 1;
/// Observations kept per rhythm signal; older ones are dropped first.
pub const MAX_RHYTHM_OBSERVATIONS: usize = 2_000;
/// Days of rhythm history kept, counted in `day_key` units.
pub const RHYTHM_RETENTION_DAYS: u32 = 90;
/// Open experiments kept; the oldest is dropped when exceeded.
pub const MAX_EXPERIMENTS: usize = 8;
/// Scores kept per experiment block.
pub const MAX_SCORES_PER_BLOCK: usize = 64;

/// One tiny experiment in progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Experiment {
    /// Caller-chosen id; never user text.
    pub id: String,
    /// The planned sequence of phases, e.g. `[B, I, B, I]`.
    pub plan: Vec<Phase>,
    /// Blocks run so far, in order. The last one is the active block.
    pub blocks: Vec<Block>,
    pub started_at: u64,
}

impl Experiment {
    /// Phase the next recorded score belongs to, or `None` when the plan is
    /// complete.
    pub fn active_phase(&self) -> Option<Phase> {
        self.plan.get(self.blocks.len().saturating_sub(1)).copied()
    }

    pub fn is_complete(&self) -> bool {
        self.blocks.len() >= self.plan.len()
            && self
                .blocks
                .last()
                .map(|b| b.scores.len() >= ExperimentConfig::default().min_scores_per_block)
                .unwrap_or(false)
    }
}

/// What the kernel and the initiative sources read each evaluation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttunementHints {
    /// True when `now` falls in the half-depth dip of the named rhythm
    /// signal and the fit is confident enough to act on.
    pub in_rhythm_dip: bool,
    /// 0..1 confidence of the daily rhythm used for `in_rhythm_dip`.
    pub rhythm_confidence: f64,
    /// Fractional hour of the usual daily low, when known.
    pub daily_trough_hour: Option<f64>,
    /// How unusual the latest score was for this hour, when one was given.
    pub rhythm_surprise: Option<RhythmSurprise>,
    /// Per-source multipliers in `[0.75, 1.15]`, only for sources with
    /// enough scored forecasts to leave neutral.
    pub reputation_multipliers: BTreeMap<String, f64>,
    /// Delivery relative to this user's baseline, when a baseline exists.
    pub prosody_delta: Option<ProsodyDelta>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttunementState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub prosody_baseline: ProsodyBaseline,
    /// Signal name (e.g. `mood`, `energy`) to bounded observations.
    #[serde(default)]
    pub rhythm: BTreeMap<String, Vec<RhythmObservation>>,
    #[serde(default)]
    pub forecasts: ForecastLedger,
    #[serde(default)]
    pub experiments: Vec<Experiment>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl AttunementState {
    pub fn new() -> Self {
        Self {
            version: STATE_VERSION,
            ..Default::default()
        }
    }

    /// Serialise for the prefs store. Compact JSON, no pretty printing.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse a stored state. Any parse failure yields a fresh state so a
    /// corrupt blob can never keep the app from starting; the caller decides
    /// whether to overwrite it.
    pub fn from_json(raw: &str) -> Self {
        serde_json::from_str::<AttunementState>(raw)
            .ok()
            .filter(|state| state.version <= STATE_VERSION)
            .map(|mut state| {
                state.enforce_bounds();
                state
            })
            .unwrap_or_default()
    }

    /// Forget everything. Called on data wipe and "forget what you learned".
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    // ---- prosody -------------------------------------------------------

    /// Fold a finished voice session into the baseline and return how it
    /// compared with what was usual *before* this session.
    pub fn observe_prosody(&mut self, snapshot: &ProsodySnapshot) -> ProsodyDelta {
        let delta = self.prosody_baseline.delta(snapshot);
        self.prosody_baseline.update(snapshot);
        delta
    }

    // ---- rhythm --------------------------------------------------------

    /// Record one scored observation for a signal.
    pub fn observe_rhythm(&mut self, signal: &str, observation: RhythmObservation) {
        let list = self.rhythm.entry(signal.to_string()).or_default();
        list.push(observation);
        Self::bound_rhythm(list);
    }

    /// Fit the rhythm model for a signal from the stored observations.
    pub fn rhythm_model(&self, signal: &str) -> RhythmModel {
        self.rhythm
            .get(signal)
            .map(|obs| RhythmModel::fit(obs))
            .unwrap_or_default()
    }

    /// Drop rhythm observations older than the retention window, measured
    /// from `today_key`.
    pub fn retain_recent_rhythm(&mut self, today_key: u32) {
        let oldest = today_key.saturating_sub(RHYTHM_RETENTION_DAYS);
        for list in self.rhythm.values_mut() {
            list.retain(|o| o.day_key >= oldest);
        }
        self.rhythm.retain(|_, list| !list.is_empty());
    }

    /// Forget one signal's rhythm history (e.g. when its source is disabled).
    pub fn forget_rhythm(&mut self, signal: &str) {
        self.rhythm.remove(signal);
    }

    // ---- forecasts -----------------------------------------------------

    pub fn record_forecast(&mut self, forecast: Forecast) {
        self.forecasts.record(forecast);
    }

    pub fn resolve_forecast(&mut self, id: &str, outcome: bool, now: u64) -> Option<f64> {
        self.forecasts.resolve(id, outcome, now)
    }

    // ---- experiments ---------------------------------------------------

    /// Start an experiment. A duplicate id replaces the earlier one.
    pub fn start_experiment(&mut self, id: &str, plan: Vec<Phase>, now: u64) -> bool {
        if plan.len() < 2 || plan[0] != Phase::Baseline {
            return false;
        }
        self.experiments.retain(|e| e.id != id);
        let first = Block {
            phase: plan[0],
            scores: Vec::new(),
        };
        self.experiments.push(Experiment {
            id: id.to_string(),
            plan,
            blocks: vec![first],
            started_at: now,
        });
        while self.experiments.len() > MAX_EXPERIMENTS {
            self.experiments.remove(0);
        }
        true
    }

    /// Add a score to the active block of an experiment.
    pub fn record_experiment_score(&mut self, id: &str, score: f64) -> bool {
        let Some(exp) = self.experiments.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        let Some(block) = exp.blocks.last_mut() else {
            return false;
        };
        if block.scores.len() >= MAX_SCORES_PER_BLOCK {
            return false;
        }
        block.scores.push(score);
        true
    }

    /// Move an experiment to its next planned block. Returns the new
    /// phase, or `None` when the plan is finished.
    pub fn advance_experiment(&mut self, id: &str) -> Option<Phase> {
        let exp = self.experiments.iter_mut().find(|e| e.id == id)?;
        let next = *exp.plan.get(exp.blocks.len())?;
        exp.blocks.push(Block {
            phase: next,
            scores: Vec::new(),
        });
        Some(next)
    }

    pub fn analyze_experiment(&self, id: &str, seed: u64) -> Option<ExperimentResult> {
        let exp = self.experiments.iter().find(|e| e.id == id)?;
        Some(analyze(&exp.blocks, &ExperimentConfig::default(), seed))
    }

    /// Stop and delete an experiment and its scores.
    pub fn stop_experiment(&mut self, id: &str) -> bool {
        let before = self.experiments.len();
        self.experiments.retain(|e| e.id != id);
        before != self.experiments.len()
    }

    // ---- hints ---------------------------------------------------------

    /// Everything the kernel needs this evaluation.
    ///
    /// * `rhythm_signal` names which stored signal drives the dip hint.
    /// * `week_secs` is now, as seconds since local Monday 00:00.
    /// * `latest_score` is an optional fresh value to score for surprise.
    /// * `latest_prosody` is an optional snapshot from the last voice turn.
    /// * `min_rhythm_confidence` gates the dip hint; the design uses 0.6.
    pub fn hints(
        &self,
        rhythm_signal: &str,
        week_secs: f64,
        latest_score: Option<f64>,
        latest_prosody: Option<&ProsodySnapshot>,
        min_rhythm_confidence: f64,
    ) -> AttunementHints {
        let model = self.rhythm_model(rhythm_signal);
        let rhythm_confidence = model.confidence();
        let in_rhythm_dip =
            rhythm_confidence >= min_rhythm_confidence && model.in_daily_dip(week_secs);
        let rhythm_surprise = latest_score.and_then(|v| model.surprise(week_secs, v));

        let mut reputation_multipliers = BTreeMap::new();
        for source in self.forecasts.sources() {
            let skill = self.forecasts.skill(&source);
            if skill.scored >= MIN_SCORED_FOR_REPUTATION {
                reputation_multipliers.insert(source, skill.reputation_multiplier);
            }
        }

        let prosody_delta = latest_prosody
            .filter(|_| self.prosody_baseline.sessions >= 3)
            .map(|s| self.prosody_baseline.delta(s));

        AttunementHints {
            in_rhythm_dip,
            rhythm_confidence,
            daily_trough_hour: model.daily_trough_hour(),
            rhythm_surprise,
            reputation_multipliers,
            prosody_delta,
        }
    }

    fn bound_rhythm(list: &mut Vec<RhythmObservation>) {
        if list.len() > MAX_RHYTHM_OBSERVATIONS {
            let excess = list.len() - MAX_RHYTHM_OBSERVATIONS;
            list.drain(..excess);
        }
    }

    fn enforce_bounds(&mut self) {
        for list in self.rhythm.values_mut() {
            Self::bound_rhythm(list);
        }
        while self.experiments.len() > MAX_EXPERIMENTS {
            self.experiments.remove(0);
        }
        for exp in &mut self.experiments {
            for block in &mut exp.blocks {
                block.scores.truncate(MAX_SCORES_PER_BLOCK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attunement::rhythm::DAY_SECS;

    fn obs(day: u32, hour: f64, value: f64) -> RhythmObservation {
        RhythmObservation {
            week_secs: f64::from(day % 7) * DAY_SECS + hour * 3_600.0,
            value,
            day_key: day,
        }
    }

    fn rhythmic_state() -> AttunementState {
        let mut state = AttunementState::new();
        for day in 0..14 {
            for hour in [8.0, 12.0, 16.0, 21.0] {
                // Peak near 12:00, trough near 00:00; dip window covers 21:00.
                let value = 6.0 + 1.5 * (std::f64::consts::TAU * (hour - 12.0) / 24.0).cos();
                state.observe_rhythm("mood", obs(day, hour, value));
            }
        }
        state
    }

    #[test]
    fn json_round_trip_preserves_everything() {
        let mut state = rhythmic_state();
        state.record_forecast(Forecast {
            id: "f1".into(),
            source: "rhythm".into(),
            probability: 0.8,
            made_at: 1,
            resolves_by: 100,
        });
        state.resolve_forecast("f1", true, 5);
        assert!(state.start_experiment("walk", vec![Phase::Baseline, Phase::Intervention], 7));
        assert!(state.record_experiment_score("walk", 5.0));

        let json = state.to_json().unwrap();
        let back = AttunementState::from_json(&json);
        assert_eq!(back, state);
        assert_eq!(back.version, STATE_VERSION);
        assert!(!json.contains("title"));
    }

    #[test]
    fn corrupt_or_future_json_yields_fresh_state() {
        assert_eq!(
            AttunementState::from_json("not json"),
            AttunementState::default()
        );
        let future = format!("{{\"version\":{}}}", STATE_VERSION + 1);
        assert_eq!(
            AttunementState::from_json(&future),
            AttunementState::default()
        );
        let partial = AttunementState::from_json("{}");
        assert_eq!(partial.version, STATE_VERSION);
    }

    #[test]
    fn hints_report_dip_surprise_and_trough() {
        let state = rhythmic_state();
        let at_night = 3.0 * DAY_SECS + 23.0 * 3_600.0;
        let at_noon = 3.0 * DAY_SECS + 12.0 * 3_600.0;
        let night = state.hints("mood", at_night, Some(2.0), None, 0.6);
        let noon = state.hints("mood", at_noon, None, None, 0.6);
        assert!(night.in_rhythm_dip, "{night:?}");
        assert!(!noon.in_rhythm_dip, "{noon:?}");
        assert!(night.rhythm_confidence > 0.6);
        let trough = night.daily_trough_hour.unwrap();
        assert!(!(1.5..=22.5).contains(&trough), "trough {trough}");
        assert!(night.rhythm_surprise.unwrap().z < -2.0);
        assert!(noon.rhythm_surprise.is_none());
        assert!(night.reputation_multipliers.is_empty());
        assert!(night.prosody_delta.is_none());
    }

    #[test]
    fn hints_are_empty_for_an_unknown_signal() {
        let state = AttunementState::new();
        let hints = state.hints("missing", 0.0, Some(5.0), None, 0.6);
        assert_eq!(hints, AttunementHints::default());
    }

    #[test]
    fn rhythm_history_is_bounded_and_retained_by_day() {
        let mut state = AttunementState::new();
        for i in 0..(MAX_RHYTHM_OBSERVATIONS as u32 + 50) {
            state.observe_rhythm("energy", obs(i / 4, 9.0, 0.5));
        }
        assert_eq!(state.rhythm["energy"].len(), MAX_RHYTHM_OBSERVATIONS);

        let today = 600;
        state.retain_recent_rhythm(today);
        assert!(state.rhythm["energy"]
            .iter()
            .all(|o| o.day_key >= today - RHYTHM_RETENTION_DAYS));

        state.retain_recent_rhythm(10_000);
        assert!(state.rhythm.is_empty());
        state.observe_rhythm("energy", obs(1, 9.0, 0.5));
        state.forget_rhythm("energy");
        assert!(state.rhythm.is_empty());
    }

    #[test]
    fn reputation_multipliers_appear_only_with_enough_history() {
        let mut state = AttunementState::new();
        for i in 0..(MIN_SCORED_FOR_REPUTATION + 2) {
            let id = format!("f{i}");
            state.record_forecast(Forecast {
                id: id.clone(),
                source: "initiative.rhythm_dip".into(),
                probability: 0.9,
                made_at: 0,
                resolves_by: 10,
            });
            state.resolve_forecast(&id, true, 1);
            let hints = state.hints("mood", 0.0, None, None, 0.6);
            if i + 1 < MIN_SCORED_FOR_REPUTATION {
                assert!(hints.reputation_multipliers.is_empty());
            }
        }
        let hints = state.hints("mood", 0.0, None, None, 0.6);
        let m = hints.reputation_multipliers["initiative.rhythm_dip"];
        assert!(m > 1.0 && m <= 1.15, "{m}");
    }

    #[test]
    fn experiments_follow_their_plan() {
        let mut state = AttunementState::new();
        assert!(!state.start_experiment("bad", vec![Phase::Intervention], 0));
        assert!(!state.start_experiment("short", vec![Phase::Baseline], 0));
        let plan = vec![
            Phase::Baseline,
            Phase::Intervention,
            Phase::Baseline,
            Phase::Intervention,
        ];
        assert!(state.start_experiment("walk", plan, 0));
        let exp = &state.experiments[0];
        assert_eq!(exp.active_phase(), Some(Phase::Baseline));
        assert!(!exp.is_complete());

        for (block, scores) in [
            [4.0, 5.0, 4.5],
            [7.0, 6.5, 7.5],
            [4.5, 5.0, 4.0],
            [7.0, 7.5, 6.5],
        ]
        .iter()
        .enumerate()
        {
            for s in scores {
                assert!(state.record_experiment_score("walk", *s));
            }
            let next = state.advance_experiment("walk");
            if block < 3 {
                assert!(next.is_some());
            } else {
                assert!(next.is_none());
            }
        }
        assert!(state.experiments[0].is_complete());
        let result = state.analyze_experiment("walk", 1).unwrap();
        assert_eq!(result.verdict, crate::attunement::nof1::Verdict::Helped);
        assert!(state.stop_experiment("walk"));
        assert!(!state.stop_experiment("walk"));
        assert!(state.analyze_experiment("walk", 1).is_none());
        assert!(!state.record_experiment_score("walk", 1.0));
    }

    #[test]
    fn experiment_count_and_block_size_are_bounded() {
        let mut state = AttunementState::new();
        for i in 0..(MAX_EXPERIMENTS + 3) {
            state.start_experiment(
                &format!("e{i}"),
                vec![Phase::Baseline, Phase::Intervention],
                i as u64,
            );
        }
        assert_eq!(state.experiments.len(), MAX_EXPERIMENTS);
        assert_eq!(state.experiments[0].id, "e3");
        let last = state.experiments.last().unwrap().id.clone();
        for _ in 0..MAX_SCORES_PER_BLOCK {
            assert!(state.record_experiment_score(&last, 1.0));
        }
        assert!(!state.record_experiment_score(&last, 1.0));
    }

    #[test]
    fn prosody_observation_returns_delta_against_prior_baseline() {
        let mut state = AttunementState::new();
        let usual = ProsodySnapshot {
            duration_secs: 10.0,
            speech_ratio: 0.7,
            pause_ratio: 0.3,
            syllable_rate_hz: 4.0,
            f0_median_hz: 120.0,
            f0_iqr_hz: 20.0,
            jitter: 0.01,
            energy_mean_db: -28.0,
            energy_std_db: 4.0,
            arousal: 0.5,
            flatness: 0.4,
            confidence: 0.9,
            voiced_frames: 400,
        };
        let first = state.observe_prosody(&usual);
        assert_eq!(first, ProsodyDelta::default());
        for _ in 0..3 {
            state.observe_prosody(&usual);
        }
        let fast = ProsodySnapshot {
            syllable_rate_hz: 6.0,
            ..usual
        };
        let delta = state
            .hints("mood", 0.0, None, Some(&fast), 0.6)
            .prosody_delta;
        assert!(delta.unwrap().syllable_rate > 0.4);
        assert_eq!(state.prosody_baseline.sessions, 4);

        state.clear();
        assert_eq!(state, AttunementState::new());
    }
}
