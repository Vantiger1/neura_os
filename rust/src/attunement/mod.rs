//! Attunement: content-free signal engines for a companion that adapts to
//! *how* the user is doing without reading *what* they say.
//!
//! Every module here is pure computation over numbers the app already has or
//! can derive locally. None of them read files, touch the network, start the
//! microphone, store audio, or carry any text. They produce bounded, labelled
//! numbers with explicit confidence so the Dart side can treat them the way it
//! treats every other signal: as evidence for the Autonomy Kernel, never as
//! authority, and never as a diagnosis.
//!
//! | Module | Input | Output |
//! | --- | --- | --- |
//! | [`prosody`] | PCM16 frames an active voice session already receives | arousal / flatness indices, speaking rate, pitch variability |
//! | [`rhythm`] | timestamped scores (mood check-ins, energy estimates) | daily / weekly cosinor fit, predicted dips, surprise |
//! | [`nof1`] | per-block check-in scores for a user-chosen tiny experiment | effect size, bootstrap interval, plain verdict |
//! | [`forecast`] | Neura's own predictions and how they resolved | Brier scores, calibration, bounded reputation multiplier |
//! | [`state`] | all of the above | one JSON-persistable bundle and the per-evaluation `hints()` the kernel reads |

pub mod forecast;
pub mod nof1;
pub mod prosody;
pub mod rhythm;
pub mod state;

/// Clamp a value into `0.0..=1.0`, mapping NaN to `0.0` so a bad input can
/// never propagate an unbounded or undefined signal into a policy.
pub(crate) fn unit(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// Linear map of `value` from `[lo, hi]` onto `0..=1`, clamped.
pub(crate) fn normalize(value: f64, lo: f64, hi: f64) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    unit((value - lo) / (hi - lo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_handles_nan_and_bounds() {
        assert_eq!(unit(f64::NAN), 0.0);
        assert_eq!(unit(-3.0), 0.0);
        assert_eq!(unit(7.0), 1.0);
        assert_eq!(unit(0.25), 0.25);
    }

    #[test]
    fn normalize_maps_range_and_rejects_empty_range() {
        assert_eq!(normalize(5.0, 0.0, 10.0), 0.5);
        assert_eq!(normalize(-1.0, 0.0, 10.0), 0.0);
        assert_eq!(normalize(1.0, 1.0, 1.0), 0.0);
    }
}
