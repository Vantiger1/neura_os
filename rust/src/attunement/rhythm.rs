//! Personal rhythm: a cosinor fit over timestamped, content-free scores.
//!
//! `lib/intelligence/routine_model.dart` already keeps an hour-of-week
//! histogram of app opens. This goes one step further for *scored* signals
//! (mood check-ins on the 0..10 scale, bandwidth energy estimates, prosody
//! arousal): it fits `value(t) = mesor + amplitude * cos(2*pi*t/period + phase)`
//! by ordinary least squares for a 24 h and a 7 d period, and reports the
//! fitted phase, amplitude, goodness of fit and an honest confidence.
//!
//! From the fit the app can ask "when does this person usually dip?" and
//! "how unusual is today's value for this hour?" without any model inference
//! and without storing anything but the numbers it was given.
//!
//! Timestamps are seconds since the local Monday 00:00 of the observation's
//! own week (`0 ..< 604_800`). The caller owns local time and DST; a
//! timezone change should clear the observations the same way the routine
//! model already discards history on an offset change.

use std::f64::consts::TAU;

use super::unit;

/// Seconds in one day.
pub const DAY_SECS: f64 = 86_400.0;
/// Seconds in one week.
pub const WEEK_SECS: f64 = 604_800.0;

/// Observations needed before a daily fit is attempted.
pub const MIN_DAILY_OBSERVATIONS: usize = 10;
/// Observations needed before a weekly fit is attempted.
pub const MIN_WEEKLY_OBSERVATIONS: usize = 28;
/// Distinct days needed before any fit is attempted.
pub const MIN_SPAN_DAYS: usize = 5;
/// Observations at which the sample-size part of confidence saturates.
const FULL_CONFIDENCE_OBSERVATIONS: f64 = 60.0;

/// One scored, timestamped sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RhythmObservation {
    /// Seconds since local Monday 00:00 of that observation's week.
    pub week_secs: f64,
    /// Score on whatever scale the caller uses consistently (e.g. mood 0..10).
    pub value: f64,
    /// Day index used only to count how many distinct days were observed.
    pub day_key: u32,
}

/// Least-squares cosinor parameters for one period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CosinorFit {
    pub period_secs: f64,
    /// Rhythm-adjusted mean.
    pub mesor: f64,
    /// Half the peak-to-trough swing.
    pub amplitude: f64,
    /// Time within the period at which the fitted curve peaks, in seconds.
    pub peak_secs: f64,
    /// Fraction of variance explained by the rhythm.
    pub r_squared: f64,
    /// Root mean square of the residuals.
    pub residual_rms: f64,
    pub observations: usize,
}

impl CosinorFit {
    /// Fitted value at a time within the period.
    pub fn value_at(&self, secs_in_period: f64) -> f64 {
        let phase = TAU * (secs_in_period - self.peak_secs) / self.period_secs;
        self.mesor + self.amplitude * phase.cos()
    }

    /// Time within the period of the fitted trough.
    pub fn trough_secs(&self) -> f64 {
        (self.peak_secs + self.period_secs / 2.0) % self.period_secs
    }

    /// Window around the trough where the curve is within `depth` (0..1) of
    /// the trough's distance from the mesor. `depth = 0.5` is the half-depth
    /// dip. Returns `(start, end)` in period seconds; `end` may be below
    /// `start` when the window wraps past the period boundary.
    pub fn dip_window(&self, depth: f64) -> Option<(f64, f64)> {
        if self.amplitude <= 0.0 {
            return None;
        }
        // cos(theta) <= -(1 - depth)  =>  theta in [pi - acos(1-depth), pi + acos(1-depth)]
        let half = (1.0 - unit(depth)).acos();
        let half_secs = half * self.period_secs / TAU;
        let trough = self.trough_secs();
        let start = (trough - half_secs).rem_euclid(self.period_secs);
        let end = (trough + half_secs).rem_euclid(self.period_secs);
        Some((start, end))
    }
}

/// Fit one cosinor by ordinary least squares.
pub fn fit_cosinor(observations: &[RhythmObservation], period_secs: f64) -> Option<CosinorFit> {
    let n = observations.len();
    if n < 3 || period_secs <= 0.0 {
        return None;
    }
    // Design matrix columns: 1, cos(wt), sin(wt). Solve the 3x3 normal equations.
    let omega = TAU / period_secs;
    let mut s = [[0.0f64; 3]; 3];
    let mut b = [0.0f64; 3];
    for obs in observations {
        let t = obs.week_secs.rem_euclid(period_secs);
        let row = [1.0, (omega * t).cos(), (omega * t).sin()];
        for i in 0..3 {
            b[i] += row[i] * obs.value;
            for j in 0..3 {
                s[i][j] += row[i] * row[j];
            }
        }
    }
    let coeff = solve_3x3(s, b)?;
    let (mesor, beta, gamma) = (coeff[0], coeff[1], coeff[2]);
    let amplitude = (beta * beta + gamma * gamma).sqrt();
    // value = M + beta cos(wt) + gamma sin(wt) = M + A cos(wt - phi), phi = atan2(gamma, beta)
    let phi = gamma.atan2(beta);
    let peak_secs = (phi / omega).rem_euclid(period_secs);

    let mean = observations.iter().map(|o| o.value).sum::<f64>() / n as f64;
    let mut ss_total = 0.0;
    let mut ss_res = 0.0;
    for obs in observations {
        let t = obs.week_secs.rem_euclid(period_secs);
        let fitted = mesor + beta * (omega * t).cos() + gamma * (omega * t).sin();
        ss_total += (obs.value - mean).powi(2);
        ss_res += (obs.value - fitted).powi(2);
    }
    let r_squared = if ss_total > 1e-12 {
        unit(1.0 - ss_res / ss_total)
    } else {
        0.0
    };
    Some(CosinorFit {
        period_secs,
        mesor,
        amplitude,
        peak_secs,
        r_squared,
        residual_rms: (ss_res / n as f64).sqrt(),
        observations: n,
    })
}

fn solve_3x3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let mut out = [0.0; 3];
    for (col, slot) in out.iter_mut().enumerate() {
        let mut m = a;
        for row in 0..3 {
            m[row][col] = b[row];
        }
        let d = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        *slot = d / det;
    }
    Some(out)
}

/// Daily and weekly rhythm of one scored signal.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RhythmModel {
    pub daily: Option<CosinorFit>,
    pub weekly: Option<CosinorFit>,
    pub observations: usize,
    pub span_days: usize,
}

/// How a fresh value compares with what the rhythm expected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RhythmSurprise {
    pub expected: f64,
    pub actual: f64,
    /// Residual in units of the fit's residual RMS; `|z| > 2` is unusual.
    pub z: f64,
    pub confidence: f64,
}

impl RhythmModel {
    /// Fit both periods when there is enough data. Weekly needs more.
    pub fn fit(observations: &[RhythmObservation]) -> Self {
        let mut days: Vec<u32> = observations.iter().map(|o| o.day_key).collect();
        days.sort_unstable();
        days.dedup();
        let span_days = days.len();
        if span_days < MIN_SPAN_DAYS {
            return Self {
                daily: None,
                weekly: None,
                observations: observations.len(),
                span_days,
            };
        }
        let daily = (observations.len() >= MIN_DAILY_OBSERVATIONS)
            .then(|| fit_cosinor(observations, DAY_SECS))
            .flatten();
        let weekly = (observations.len() >= MIN_WEEKLY_OBSERVATIONS)
            .then(|| fit_cosinor(observations, WEEK_SECS))
            .flatten();
        Self {
            daily,
            weekly,
            observations: observations.len(),
            span_days,
        }
    }

    /// 0..1: how much to trust the daily rhythm. Combines sample size and
    /// explained variance, so a well-sampled but flat signal stays low.
    pub fn confidence(&self) -> f64 {
        let Some(daily) = self.daily else {
            return 0.0;
        };
        let size = unit(self.observations as f64 / FULL_CONFIDENCE_OBSERVATIONS);
        unit(0.5 * size + 0.5 * daily.r_squared)
    }

    /// Predicted value at a week-second, folding in the weekly component
    /// when it was fitted.
    pub fn predict(&self, week_secs: f64) -> Option<f64> {
        let daily = self.daily?;
        let mut value = daily.value_at(week_secs.rem_euclid(DAY_SECS));
        if let Some(weekly) = self.weekly {
            value += weekly.value_at(week_secs.rem_euclid(WEEK_SECS)) - weekly.mesor;
        }
        Some(value)
    }

    /// Compare a new value with the rhythm's expectation.
    pub fn surprise(&self, week_secs: f64, actual: f64) -> Option<RhythmSurprise> {
        let daily = self.daily?;
        let expected = self.predict(week_secs)?;
        let scale = daily.residual_rms.max(1e-6);
        Some(RhythmSurprise {
            expected,
            actual,
            z: (actual - expected) / scale,
            confidence: self.confidence(),
        })
    }

    /// Hour of day (0..24, fractional) of the usual daily low.
    pub fn daily_trough_hour(&self) -> Option<f64> {
        self.daily.map(|d| d.trough_secs() / 3_600.0)
    }

    /// Hour of day (0..24, fractional) of the usual daily high.
    pub fn daily_peak_hour(&self) -> Option<f64> {
        self.daily.map(|d| d.peak_secs / 3_600.0)
    }

    /// Whether `week_secs` falls inside the daily half-depth dip window.
    pub fn in_daily_dip(&self, week_secs: f64) -> bool {
        let Some(daily) = self.daily else {
            return false;
        };
        let Some((start, end)) = daily.dip_window(0.5) else {
            return false;
        };
        let t = week_secs.rem_euclid(DAY_SECS);
        if start <= end {
            t >= start && t <= end
        } else {
            t >= start || t <= end
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise so the test does not depend on a random crate.
    fn noise(i: usize) -> f64 {
        let x = (i as f64 * 12.9898).sin() * 43_758.545;
        (x - x.floor()) - 0.5
    }

    fn series(
        days: usize,
        per_day: usize,
        mesor: f64,
        amp: f64,
        peak_hour: f64,
    ) -> Vec<RhythmObservation> {
        let mut out = Vec::new();
        for day in 0..days {
            for k in 0..per_day {
                let hour = 7.0 + k as f64 * (14.0 / per_day as f64);
                let secs_in_day = hour * 3_600.0;
                let week_secs = (day % 7) as f64 * DAY_SECS + secs_in_day;
                let value = mesor
                    + amp * (TAU * (hour - peak_hour) / 24.0).cos()
                    + 0.3 * noise(day * per_day + k);
                out.push(RhythmObservation {
                    week_secs,
                    value,
                    day_key: day as u32,
                });
            }
        }
        out
    }

    #[test]
    fn recovers_known_daily_rhythm() {
        let obs = series(14, 4, 6.0, 1.5, 11.0);
        let model = RhythmModel::fit(&obs);
        let daily = model.daily.expect("daily fit");
        assert!((daily.mesor - 6.0).abs() < 0.3, "mesor {}", daily.mesor);
        assert!(
            (daily.amplitude - 1.5).abs() < 0.3,
            "amp {}",
            daily.amplitude
        );
        let peak_hour = model.daily_peak_hour().unwrap();
        assert!((peak_hour - 11.0).abs() < 1.0, "peak {peak_hour}");
        let trough = model.daily_trough_hour().unwrap();
        assert!((trough - 23.0).abs() < 1.0, "trough {trough}");
        assert!(daily.r_squared > 0.8, "r2 {}", daily.r_squared);
        assert!(model.confidence() > 0.7);
        assert!(model.weekly.is_some());
    }

    #[test]
    fn too_few_days_or_points_gives_no_fit() {
        let obs = series(3, 6, 5.0, 1.0, 12.0);
        let model = RhythmModel::fit(&obs);
        assert!(model.daily.is_none());
        assert_eq!(model.span_days, 3);
        assert_eq!(model.confidence(), 0.0);

        let sparse = series(6, 1, 5.0, 1.0, 12.0);
        assert!(RhythmModel::fit(&sparse).daily.is_none());
    }

    #[test]
    fn flat_signal_has_low_confidence_and_no_meaningful_dip() {
        let obs = series(14, 4, 5.0, 0.0, 12.0);
        let model = RhythmModel::fit(&obs);
        let daily = model.daily.expect("fit still computed");
        assert!(daily.amplitude < 0.2, "amp {}", daily.amplitude);
        assert!(daily.r_squared < 0.3, "r2 {}", daily.r_squared);
        assert!(model.confidence() < 0.6);
    }

    #[test]
    fn surprise_is_small_on_rhythm_and_large_off_it() {
        let obs = series(14, 4, 6.0, 1.5, 11.0);
        let model = RhythmModel::fit(&obs);
        let at_peak = 2.0 * DAY_SECS + 11.0 * 3_600.0;
        let expected = model.predict(at_peak).unwrap();
        let rms = model.daily.unwrap().residual_rms;
        let on = model.surprise(at_peak, expected + 0.5 * rms).unwrap();
        let off = model.surprise(at_peak, expected - 3.0).unwrap();
        assert!((on.z - 0.5).abs() < 1e-6, "on {}", on.z);
        assert!(off.z < -2.0, "off {}", off.z);
        assert!((on.expected - expected).abs() < 1e-9);
        assert!(on.confidence > 0.7);
    }

    #[test]
    fn dip_window_surrounds_trough_and_wraps() {
        let fit = CosinorFit {
            period_secs: DAY_SECS,
            mesor: 5.0,
            amplitude: 1.0,
            peak_secs: 13.0 * 3_600.0,
            r_squared: 0.9,
            residual_rms: 0.2,
            observations: 40,
        };
        let (start, end) = fit.dip_window(0.5).unwrap();
        // Trough at 01:00; half-depth window is +/- 4 h, wrapping midnight.
        assert!((fit.trough_secs() - 3_600.0).abs() < 1.0);
        assert!(start > end, "wraps: {start} > {end}");
        let model = RhythmModel {
            daily: Some(fit),
            weekly: None,
            observations: 40,
            span_days: 10,
        };
        assert!(model.in_daily_dip(0.5 * 3_600.0));
        assert!(model.in_daily_dip(22.5 * 3_600.0));
        assert!(!model.in_daily_dip(13.0 * 3_600.0));
        assert!(fit.value_at(fit.trough_secs()) < fit.value_at(fit.peak_secs));
    }

    #[test]
    fn zero_amplitude_has_no_dip_window() {
        let fit = CosinorFit {
            period_secs: DAY_SECS,
            mesor: 5.0,
            amplitude: 0.0,
            peak_secs: 0.0,
            r_squared: 0.0,
            residual_rms: 1.0,
            observations: 40,
        };
        assert!(fit.dip_window(0.5).is_none());
    }
}
