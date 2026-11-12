//! Content-free paralinguistic features from 16-bit PCM.
//!
//! `lib/avatar/neura_mic_driver.dart` already opens a 16 kHz mono PCM16 stream
//! for the existing voice session and discards the samples after reading the
//! amplitude. This analyzer consumes that same stream and keeps only running
//! numeric statistics per frame: energy, a fundamental-frequency estimate,
//! and an envelope-peak count. No sample buffer outlives a single frame, no
//! audio is written anywhere, and nothing here can be inverted back into
//! words.
//!
//! The derived indices are heuristics about *delivery*, not clinical measures
//! and not emotion recognition. The Dart side must present them as "you
//! sound faster / flatter than usual", with the confidence attached, and only
//! as one more piece of evidence for the existing kernels.

use serde::{Deserialize, Serialize};

use super::{normalize, unit};

/// Lowest fundamental frequency searched, in hertz.
const F0_MIN_HZ: f64 = 60.0;
/// Highest fundamental frequency searched, in hertz.
const F0_MAX_HZ: f64 = 400.0;
/// Normalised autocorrelation needed to call a frame voiced.
const VOICED_THRESHOLD: f64 = 0.55;
/// Energy gate for speech, relative to the tracked noise floor, in decibels.
const SPEECH_GATE_DB: f64 = 10.0;
/// Absolute gate below which a frame is never speech, in dBFS.
const ABSOLUTE_GATE_DB: f64 = -48.0;
/// Noise floor rises this fast per frame so a long quiet stretch recalibrates.
const FLOOR_RISE_DB: f64 = 0.05;
/// Minimum separation between syllable peaks, in frames (10 ms hop).
const MIN_PEAK_SEPARATION_FRAMES: usize = 10;
/// A peak must rise this far above the preceding valley, in decibels.
const PEAK_PROMINENCE_DB: f64 = 3.0;
/// Smoothing applied to the energy track before peak picking.
const ENVELOPE_ALPHA: f64 = 0.5;
/// Voiced frames needed before a snapshot is reported at all.
const MIN_VOICED_FRAMES: u32 = 25;
/// Voiced frames at which confidence saturates.
const FULL_CONFIDENCE_FRAMES: f64 = 300.0;
/// Bound on retained F0 estimates so a long session stays O(1) in memory.
const MAX_F0_SAMPLES: usize = 4_096;
/// Silences shorter than this (in 10 ms frames) are part of articulation,
/// not pauses, when measuring speaking rate.
const SHORT_GAP_FRAMES: u32 = 15;

/// Incremental analyzer over a single voice session.
#[derive(Clone, Debug)]
pub struct ProsodyAnalyzer {
    sample_rate: u32,
    frame_len: usize,
    hop: usize,
    pending: Vec<f64>,
    total_frames: u32,
    speech_frames: u32,
    articulation_frames: u32,
    silence_run: u32,
    voiced_frames: u32,
    noise_floor_db: f64,
    energy_sum: f64,
    energy_sq_sum: f64,
    f0_hz: Vec<f64>,
    previous_period: Option<f64>,
    jitter_sum: f64,
    jitter_count: u32,
    envelope_db: Option<f64>,
    envelope_rising: bool,
    last_valley_db: f64,
    candidate_peak_db: f64,
    candidate_peak_speech: bool,
    frames_since_peak: usize,
    syllable_peaks: u32,
}

/// One session's content-free delivery statistics.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProsodySnapshot {
    /// Audio analysed so far, in seconds.
    pub duration_secs: f64,
    /// Fraction of frames above the speech gate.
    pub speech_ratio: f64,
    /// `1 - speech_ratio`: silence, breaths and hesitation.
    pub pause_ratio: f64,
    /// Envelope peaks per second of articulation (speech plus gaps under
    /// 150 ms); a syllable-rate proxy.
    pub syllable_rate_hz: f64,
    /// Median fundamental frequency over voiced frames.
    pub f0_median_hz: f64,
    /// Interquartile range of fundamental frequency; a pitch-variability proxy.
    pub f0_iqr_hz: f64,
    /// Mean relative period change between consecutive voiced frames.
    pub jitter: f64,
    /// Mean frame energy over speech frames, in dBFS.
    pub energy_mean_db: f64,
    /// Standard deviation of frame energy over speech frames, in decibels.
    pub energy_std_db: f64,
    /// 0..1 composite: faster, louder, more pitch-varied delivery scores higher.
    pub arousal: f64,
    /// 0..1 composite: monotone, quiet, slow, pause-heavy delivery scores higher.
    pub flatness: f64,
    /// 0..1 confidence from how much voiced speech has been observed.
    pub confidence: f64,
    /// Number of voiced frames behind this snapshot.
    pub voiced_frames: u32,
}

impl ProsodyAnalyzer {
    /// Frames are 32 ms with a 10 ms hop at the given sample rate.
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(8_000) as usize;
        let frame_len = rate * 32 / 1_000;
        let hop = rate / 100;
        Self {
            sample_rate: rate as u32,
            frame_len,
            hop,
            pending: Vec::with_capacity(frame_len * 2),
            total_frames: 0,
            speech_frames: 0,
            articulation_frames: 0,
            silence_run: 0,
            voiced_frames: 0,
            noise_floor_db: -70.0,
            energy_sum: 0.0,
            energy_sq_sum: 0.0,
            f0_hz: Vec::new(),
            previous_period: None,
            jitter_sum: 0.0,
            jitter_count: 0,
            envelope_db: None,
            envelope_rising: false,
            last_valley_db: -120.0,
            candidate_peak_db: -120.0,
            candidate_peak_speech: false,
            frames_since_peak: MIN_PEAK_SEPARATION_FRAMES,
            syllable_peaks: 0,
        }
    }

    /// Sample rate this analyzer expects, in hertz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Discard all state. Call on Safe Mode, session end or user request.
    pub fn reset(&mut self) {
        *self = Self::new(self.sample_rate);
    }

    /// Feed interleaved-free mono PCM16 samples. Partial frames are held
    /// until enough samples arrive; nothing else is retained.
    pub fn push_pcm16(&mut self, samples: &[i16]) {
        self.pending
            .extend(samples.iter().map(|s| f64::from(*s) / 32_768.0));
        while self.pending.len() >= self.frame_len {
            let frame: Vec<f64> = self.pending[..self.frame_len].to_vec();
            self.analyze_frame(&frame);
            self.pending.drain(..self.hop);
        }
    }

    fn analyze_frame(&mut self, frame: &[f64]) {
        self.total_frames += 1;
        let rms = (frame.iter().map(|s| s * s).sum::<f64>() / frame.len() as f64).sqrt();
        let energy_db = if rms > 1e-9 {
            20.0 * rms.log10()
        } else {
            -120.0
        };

        if energy_db < self.noise_floor_db {
            self.noise_floor_db = energy_db;
        } else {
            self.noise_floor_db += FLOOR_RISE_DB;
        }
        let is_speech =
            energy_db > ABSOLUTE_GATE_DB && energy_db > self.noise_floor_db + SPEECH_GATE_DB;

        if is_speech {
            self.speech_frames += 1;
            if self.silence_run > 0 && self.silence_run < SHORT_GAP_FRAMES {
                self.articulation_frames += self.silence_run;
            }
            self.silence_run = 0;
            self.articulation_frames += 1;
            self.energy_sum += energy_db;
            self.energy_sq_sum += energy_db * energy_db;
            if let Some(period) = self.estimate_period(frame) {
                self.voiced_frames += 1;
                if self.f0_hz.len() < MAX_F0_SAMPLES {
                    self.f0_hz.push(f64::from(self.sample_rate) / period);
                }
                if let Some(previous) = self.previous_period {
                    self.jitter_sum += (period - previous).abs() / previous;
                    self.jitter_count += 1;
                }
                self.previous_period = Some(period);
            } else {
                self.previous_period = None;
            }
        } else {
            self.silence_run = self.silence_run.saturating_add(1);
            self.previous_period = None;
        }

        self.track_syllable_peak(energy_db, is_speech);
    }

    /// Envelope peak detection on the smoothed per-frame energy track: a
    /// rise-then-fall whose crest stands at least [`PEAK_PROMINENCE_DB`]
    /// above the preceding valley counts as one syllable nucleus.
    fn track_syllable_peak(&mut self, energy_db: f64, is_speech: bool) {
        self.frames_since_peak = self.frames_since_peak.saturating_add(1);
        let floor_db = energy_db.max(-120.0);
        let smoothed = match self.envelope_db {
            Some(previous) => previous + ENVELOPE_ALPHA * (floor_db - previous),
            None => floor_db,
        };
        let Some(previous) = self.envelope_db else {
            self.envelope_db = Some(smoothed);
            self.last_valley_db = smoothed;
            self.candidate_peak_db = smoothed;
            return;
        };
        let rising_now = smoothed > previous;
        if rising_now {
            if !self.envelope_rising {
                // Turned upward: the previous frame was a valley.
                self.last_valley_db = previous;
                self.candidate_peak_db = previous;
                self.candidate_peak_speech = false;
            }
            self.candidate_peak_db = smoothed;
            self.candidate_peak_speech = is_speech;
        } else if self.envelope_rising {
            // Turned downward: the previous frame was a crest.
            let prominent = self.candidate_peak_db - self.last_valley_db >= PEAK_PROMINENCE_DB;
            if prominent
                && self.candidate_peak_speech
                && self.frames_since_peak >= MIN_PEAK_SEPARATION_FRAMES
            {
                self.syllable_peaks += 1;
                self.frames_since_peak = 0;
            }
        }
        self.envelope_rising = rising_now;
        self.envelope_db = Some(smoothed);
    }

    /// Normalised autocorrelation pitch estimate. Returns the period in
    /// samples when the frame is confidently periodic in the voice range.
    fn estimate_period(&self, frame: &[f64]) -> Option<f64> {
        let rate = f64::from(self.sample_rate);
        let min_lag = (rate / F0_MAX_HZ).floor() as usize;
        let max_lag = ((rate / F0_MIN_HZ).ceil() as usize).min(frame.len() / 2);
        if min_lag == 0 || max_lag <= min_lag {
            return None;
        }
        let mean = frame.iter().sum::<f64>() / frame.len() as f64;
        let centred: Vec<f64> = frame.iter().map(|s| s - mean).collect();
        let energy: f64 = centred.iter().map(|s| s * s).sum();
        if energy <= 1e-12 {
            return None;
        }

        let mut best_lag = 0usize;
        let mut best_value = f64::MIN;
        let mut previous = f64::MIN;
        let mut rising = false;
        for lag in min_lag..=max_lag {
            let n = centred.len() - lag;
            let mut acc = 0.0;
            let mut e1 = 0.0;
            let mut e2 = 0.0;
            for i in 0..n {
                acc += centred[i] * centred[i + lag];
                e1 += centred[i] * centred[i];
                e2 += centred[i + lag] * centred[i + lag];
            }
            let value = if e1 > 0.0 && e2 > 0.0 {
                acc / (e1 * e2).sqrt()
            } else {
                0.0
            };
            // Take the first strong local maximum so harmonics at 2x the
            // period do not win over the fundamental.
            if value < previous && rising && previous >= VOICED_THRESHOLD {
                best_lag = lag - 1;
                best_value = previous;
                break;
            }
            rising = value > previous;
            previous = value;
            if value > best_value {
                best_value = value;
                best_lag = lag;
            }
        }
        (best_value >= VOICED_THRESHOLD && best_lag > 0).then_some(best_lag as f64)
    }

    /// Current statistics, or `None` until enough voiced speech was heard.
    pub fn snapshot(&self) -> Option<ProsodySnapshot> {
        if self.voiced_frames < MIN_VOICED_FRAMES || self.speech_frames == 0 {
            return None;
        }
        let hop_secs = self.hop as f64 / f64::from(self.sample_rate);
        let duration_secs = f64::from(self.total_frames) * hop_secs;
        let articulation_secs = f64::from(self.articulation_frames) * hop_secs;
        let speech_ratio = unit(f64::from(self.speech_frames) / f64::from(self.total_frames));

        let n = f64::from(self.speech_frames);
        let energy_mean_db = self.energy_sum / n;
        let energy_var = (self.energy_sq_sum / n - energy_mean_db * energy_mean_db).max(0.0);
        let energy_std_db = energy_var.sqrt();

        let mut sorted = self.f0_hz.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let f0_median_hz = percentile(&sorted, 0.5);
        let f0_iqr_hz = percentile(&sorted, 0.75) - percentile(&sorted, 0.25);

        let jitter = if self.jitter_count > 0 {
            self.jitter_sum / f64::from(self.jitter_count)
        } else {
            0.0
        };
        let syllable_rate_hz = if articulation_secs > 0.0 {
            f64::from(self.syllable_peaks) / articulation_secs
        } else {
            0.0
        };

        let rate_n = normalize(syllable_rate_hz, 2.0, 7.0);
        let energy_n = normalize(energy_mean_db, -40.0, -15.0);
        let pitch_var_n = normalize(f0_iqr_hz, 5.0, 80.0);
        let arousal = unit(0.40 * rate_n + 0.35 * energy_n + 0.25 * pitch_var_n);
        let flatness =
            unit(0.45 * (1.0 - pitch_var_n) + 0.30 * (1.0 - rate_n) + 0.25 * (1.0 - speech_ratio));
        let confidence = unit(f64::from(self.voiced_frames) / FULL_CONFIDENCE_FRAMES);

        Some(ProsodySnapshot {
            duration_secs,
            speech_ratio,
            pause_ratio: 1.0 - speech_ratio,
            syllable_rate_hz,
            f0_median_hz,
            f0_iqr_hz,
            jitter,
            energy_mean_db,
            energy_std_db,
            arousal,
            flatness,
            confidence,
            voiced_frames: self.voiced_frames,
        })
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

/// Per-user baseline of delivery statistics, so "faster than usual" is
/// relative to this person rather than to a population norm. Holds only
/// exponential moving averages of the numeric fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProsodyBaseline {
    pub sessions: u32,
    pub syllable_rate_hz: f64,
    pub f0_median_hz: f64,
    pub f0_iqr_hz: f64,
    pub energy_mean_db: f64,
    pub pause_ratio: f64,
    pub arousal: f64,
    pub flatness: f64,
}

/// How a session compares with the baseline, as signed fractions of the
/// baseline value (`+0.2` means 20 % above usual). Zero when no baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProsodyDelta {
    pub syllable_rate: f64,
    pub f0_median: f64,
    pub f0_variability: f64,
    pub pause_ratio: f64,
    pub arousal: f64,
    pub flatness: f64,
    /// Baseline sessions behind this comparison; the caller should treat
    /// fewer than three as "no usual yet".
    pub baseline_sessions: u32,
}

impl ProsodyBaseline {
    /// Sessions after which the moving average stops weighting new sessions
    /// heavily.
    const WARMUP_SESSIONS: u32 = 8;

    /// Fold a finished session in. Early sessions move the baseline more.
    pub fn update(&mut self, snapshot: &ProsodySnapshot) {
        if snapshot.confidence < 0.2 {
            return;
        }
        self.sessions = self.sessions.saturating_add(1);
        let alpha = 1.0 / f64::from(self.sessions.min(Self::WARMUP_SESSIONS));
        let blend = |old: f64, new: f64| old + alpha * (new - old);
        self.syllable_rate_hz = blend(self.syllable_rate_hz, snapshot.syllable_rate_hz);
        self.f0_median_hz = blend(self.f0_median_hz, snapshot.f0_median_hz);
        self.f0_iqr_hz = blend(self.f0_iqr_hz, snapshot.f0_iqr_hz);
        self.energy_mean_db = blend(self.energy_mean_db, snapshot.energy_mean_db);
        self.pause_ratio = blend(self.pause_ratio, snapshot.pause_ratio);
        self.arousal = blend(self.arousal, snapshot.arousal);
        self.flatness = blend(self.flatness, snapshot.flatness);
    }

    /// Relative change of a session against this baseline.
    pub fn delta(&self, snapshot: &ProsodySnapshot) -> ProsodyDelta {
        if self.sessions == 0 {
            return ProsodyDelta::default();
        }
        let rel = |base: f64, now: f64| {
            if base.abs() < 1e-9 {
                0.0
            } else {
                ((now - base) / base.abs()).clamp(-3.0, 3.0)
            }
        };
        ProsodyDelta {
            syllable_rate: rel(self.syllable_rate_hz, snapshot.syllable_rate_hz),
            f0_median: rel(self.f0_median_hz, snapshot.f0_median_hz),
            f0_variability: rel(self.f0_iqr_hz, snapshot.f0_iqr_hz),
            pause_ratio: rel(self.pause_ratio, snapshot.pause_ratio),
            arousal: rel(self.arousal, snapshot.arousal),
            flatness: rel(self.flatness, snapshot.flatness),
            baseline_sessions: self.sessions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATE: u32 = 16_000;

    /// Synthetic "speech": a harmonic tone at `f0` whose amplitude is
    /// modulated at `syllable_hz` with silent gaps, so the envelope has
    /// syllable-like peaks and the waveform has a clear fundamental.
    fn synth(
        f0: f64,
        syllable_hz: f64,
        amplitude: f64,
        seconds: f64,
        gap_every: usize,
    ) -> Vec<i16> {
        let n = (seconds * f64::from(RATE)) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / f64::from(RATE);
            let syllable_index = (t * syllable_hz).floor() as usize;
            let in_gap = gap_every > 0 && syllable_index % gap_every == gap_every - 1;
            let env = (0.5 - 0.5 * (2.0 * PI * syllable_hz * t).cos()).powf(1.5);
            let harmonics = (2.0 * PI * f0 * t).sin()
                + 0.5 * (2.0 * PI * 2.0 * f0 * t).sin()
                + 0.25 * (2.0 * PI * 3.0 * f0 * t).sin();
            let sample = if in_gap {
                0.0
            } else {
                amplitude * env * harmonics / 1.75
            };
            out.push((sample * 32_767.0).round() as i16);
        }
        out
    }

    #[test]
    fn silence_yields_no_snapshot() {
        let mut analyzer = ProsodyAnalyzer::new(RATE);
        analyzer.push_pcm16(&vec![0i16; RATE as usize * 3]);
        assert!(analyzer.snapshot().is_none());
    }

    #[test]
    fn recovers_fundamental_and_syllable_rate() {
        let mut analyzer = ProsodyAnalyzer::new(RATE);
        analyzer.push_pcm16(&synth(120.0, 4.0, 0.4, 6.0, 0));
        let snap = analyzer.snapshot().expect("voiced speech");
        assert!(
            (snap.f0_median_hz - 120.0).abs() < 6.0,
            "f0 median {}",
            snap.f0_median_hz
        );
        assert!(
            (snap.syllable_rate_hz - 4.0).abs() < 1.0,
            "syllable rate {}",
            snap.syllable_rate_hz
        );
        assert!(
            snap.speech_ratio > 0.5,
            "speech ratio {}",
            snap.speech_ratio
        );
        assert!(snap.confidence > 0.5);
        assert!(snap.jitter < 0.05, "jitter {}", snap.jitter);
    }

    #[test]
    fn gaps_raise_pause_ratio() {
        let mut continuous = ProsodyAnalyzer::new(RATE);
        continuous.push_pcm16(&synth(140.0, 4.0, 0.4, 6.0, 0));
        let mut gappy = ProsodyAnalyzer::new(RATE);
        gappy.push_pcm16(&synth(140.0, 4.0, 0.4, 6.0, 2));
        let a = continuous.snapshot().unwrap();
        let b = gappy.snapshot().unwrap();
        assert!(
            b.pause_ratio > a.pause_ratio + 0.2,
            "{} vs {}",
            b.pause_ratio,
            a.pause_ratio
        );
    }

    #[test]
    fn fast_loud_delivery_has_higher_arousal_than_slow_quiet() {
        let mut fast = ProsodyAnalyzer::new(RATE);
        fast.push_pcm16(&synth(200.0, 6.5, 0.6, 6.0, 0));
        let mut slow = ProsodyAnalyzer::new(RATE);
        slow.push_pcm16(&synth(110.0, 2.2, 0.08, 6.0, 3));
        let f = fast.snapshot().unwrap();
        let s = slow.snapshot().unwrap();
        assert!(
            f.arousal > s.arousal + 0.25,
            "fast {} slow {}",
            f.arousal,
            s.arousal
        );
        assert!(
            s.flatness > f.flatness,
            "flat slow {} fast {}",
            s.flatness,
            f.flatness
        );
    }

    #[test]
    fn partial_frames_are_carried_between_pushes() {
        let signal = synth(130.0, 4.0, 0.4, 4.0, 0);
        let mut whole = ProsodyAnalyzer::new(RATE);
        whole.push_pcm16(&signal);
        let mut chunked = ProsodyAnalyzer::new(RATE);
        for chunk in signal.chunks(333) {
            chunked.push_pcm16(chunk);
        }
        let a = whole.snapshot().unwrap();
        let b = chunked.snapshot().unwrap();
        assert_eq!(a.voiced_frames, b.voiced_frames);
        assert!((a.f0_median_hz - b.f0_median_hz).abs() < 1e-9);
    }

    #[test]
    fn reset_clears_everything() {
        let mut analyzer = ProsodyAnalyzer::new(RATE);
        analyzer.push_pcm16(&synth(130.0, 4.0, 0.4, 3.0, 0));
        assert!(analyzer.snapshot().is_some());
        analyzer.reset();
        assert!(analyzer.snapshot().is_none());
        assert_eq!(analyzer.total_frames, 0);
    }

    #[test]
    fn baseline_learns_and_reports_relative_delta() {
        let mut baseline = ProsodyBaseline::default();
        let mut usual = ProsodyAnalyzer::new(RATE);
        usual.push_pcm16(&synth(130.0, 3.5, 0.3, 5.0, 0));
        let usual_snap = usual.snapshot().unwrap();
        for _ in 0..4 {
            baseline.update(&usual_snap);
        }
        assert_eq!(baseline.sessions, 4);
        assert!((baseline.syllable_rate_hz - usual_snap.syllable_rate_hz).abs() < 1e-9);

        let mut today = ProsodyAnalyzer::new(RATE);
        today.push_pcm16(&synth(130.0, 6.0, 0.3, 5.0, 0));
        let delta = baseline.delta(&today.snapshot().unwrap());
        assert!(delta.syllable_rate > 0.3, "delta {}", delta.syllable_rate);
        assert_eq!(delta.baseline_sessions, 4);

        let empty = ProsodyBaseline::default();
        assert_eq!(empty.delta(&usual_snap), ProsodyDelta::default());
    }

    #[test]
    fn low_confidence_sessions_do_not_move_the_baseline() {
        let mut baseline = ProsodyBaseline::default();
        let snap = ProsodySnapshot {
            duration_secs: 1.0,
            speech_ratio: 0.5,
            pause_ratio: 0.5,
            syllable_rate_hz: 4.0,
            f0_median_hz: 120.0,
            f0_iqr_hz: 10.0,
            jitter: 0.01,
            energy_mean_db: -30.0,
            energy_std_db: 3.0,
            arousal: 0.5,
            flatness: 0.5,
            confidence: 0.1,
            voiced_frames: 30,
        };
        baseline.update(&snap);
        assert_eq!(baseline.sessions, 0);
    }
}
