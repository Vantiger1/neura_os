import 'package:flutter/foundation.dart';

/// Why the policy chose the rate it did. Mirrors the kernel habit of
/// returning an explainable reason with every decision.
enum NeuraEntrainmentReason {
  disabled,
  safeMode,
  crisisPlaybook,
  noMeasurement,
  lowConfidence,
  leadingSlower,
  gentleLift,
  holdingPreference,
}

@immutable
class NeuraEntrainmentDecision {
  const NeuraEntrainmentDecision({
    required this.rate,
    required this.reason,
    required this.changed,
  });

  /// Speech rate to apply, already inside `NeuraTtsService.setSpeechRate`'s
  /// `[0.25, 0.75]` clamp.
  final double rate;
  final NeuraEntrainmentReason reason;

  /// False when the rate equals what was already playing.
  final bool changed;

  @override
  bool operator ==(Object other) =>
      other is NeuraEntrainmentDecision &&
      other.rate == rate &&
      other.reason == reason &&
      other.changed == changed;

  @override
  int get hashCode => Object.hash(rate, reason, changed);

  @override
  String toString() => 'NeuraEntrainmentDecision(rate: $rate, '
      'reason: $reason, changed: $changed)';
}

/// Vocal co-regulation: Neura paces her reply voice against how the user
/// just spoke, leading slightly slower when their delivery is fast and
/// aroused, and slightly brighter when it is flat, so her voice settles or
/// lifts the exchange instead of mirroring it.
///
/// Inputs are the content-free numbers from the Rust prosody analyzer
/// (`rust/src/attunement/prosody.rs`): a syllable-rate estimate, an arousal
/// index and a confidence. The policy is a pure function, opt-in, and bounded
/// twice: by the TTS service's own rate clamp and by a per-turn step limit so
/// the voice never lurches. It never applies during Safe Mode or while the
/// crisis playbook owns the voice, and it never changes pitch or volume.
@immutable
class NeuraVocalEntrainmentPolicy {
  const NeuraVocalEntrainmentPolicy({
    this.leadFraction = 0.10,
    this.liftFraction = 0.05,
    this.maxStepPerTurn = 0.05,
    this.minRate = 0.25,
    this.maxRate = 0.75,
    this.highArousal = 0.60,
    this.lowArousal = 0.30,
    this.minConfidence = 0.35,
    this.slowSyllableRateHz = 2.0,
    this.fastSyllableRateHz = 7.0,
    this.slowTtsRate = 0.35,
    this.fastTtsRate = 0.65,
  })  : assert(leadFraction >= 0 && leadFraction <= 0.3),
        assert(liftFraction >= 0 && liftFraction <= 0.3),
        assert(maxStepPerTurn > 0),
        assert(minRate < maxRate),
        assert(lowArousal < highArousal),
        assert(slowSyllableRateHz < fastSyllableRateHz),
        assert(slowTtsRate < fastTtsRate);

  /// How much slower than the user's matched rate Neura leads when aroused.
  final double leadFraction;

  /// How much brighter Neura goes when the user's delivery is flat.
  final double liftFraction;

  /// Largest rate change allowed between consecutive replies.
  final double maxStepPerTurn;

  final double minRate;
  final double maxRate;
  final double highArousal;
  final double lowArousal;
  final double minConfidence;

  /// Syllable-rate range mapped onto the TTS rate range.
  final double slowSyllableRateHz;
  final double fastSyllableRateHz;
  final double slowTtsRate;
  final double fastTtsRate;

  NeuraEntrainmentDecision resolve({
    required bool enabled,
    required bool safeMode,
    required bool crisisPlaybookActive,
    required double currentRate,
    required double preferredRate,
    required double? userSyllableRateHz,
    required double arousal,
    required double confidence,
  }) {
    final current = _clamp(currentRate);
    final preferred = _clamp(preferredRate);

    if (!enabled) {
      return _hold(current, NeuraEntrainmentReason.disabled);
    }
    if (safeMode) {
      return _hold(current, NeuraEntrainmentReason.safeMode);
    }
    if (crisisPlaybookActive) {
      return _hold(current, NeuraEntrainmentReason.crisisPlaybook);
    }
    if (userSyllableRateHz == null || userSyllableRateHz <= 0) {
      return _step(current, preferred, NeuraEntrainmentReason.noMeasurement);
    }
    if (confidence < minConfidence) {
      return _step(current, preferred, NeuraEntrainmentReason.lowConfidence);
    }

    final matched = _matchedRate(userSyllableRateHz);
    if (arousal >= highArousal) {
      final target = matched * (1 - leadFraction);
      return _step(current, target, NeuraEntrainmentReason.leadingSlower);
    }
    if (arousal <= lowArousal) {
      // Lift relative to the user's preference, not their flat delivery, so a
      // quiet day never makes Neura race ahead of them.
      final target = preferred * (1 + liftFraction);
      return _step(current, target, NeuraEntrainmentReason.gentleLift);
    }
    return _step(current, preferred, NeuraEntrainmentReason.holdingPreference);
  }

  /// Linear map of the user's syllable rate onto the TTS rate range.
  double _matchedRate(double syllableRateHz) {
    final span = fastSyllableRateHz - slowSyllableRateHz;
    final t = ((syllableRateHz - slowSyllableRateHz) / span)
        .clamp(0.0, 1.0)
        .toDouble();
    return slowTtsRate + t * (fastTtsRate - slowTtsRate);
  }

  NeuraEntrainmentDecision _hold(double current, NeuraEntrainmentReason why) {
    return NeuraEntrainmentDecision(rate: current, reason: why, changed: false);
  }

  /// Move toward [target] by at most [maxStepPerTurn].
  NeuraEntrainmentDecision _step(
    double current,
    double target,
    NeuraEntrainmentReason why,
  ) {
    final bounded = _clamp(target);
    final delta = (bounded - current)
        .clamp(-maxStepPerTurn, maxStepPerTurn)
        .toDouble();
    final next = _clamp(current + delta);
    return NeuraEntrainmentDecision(
      rate: next,
      reason: why,
      changed: (next - current).abs() > 1e-9,
    );
  }

  double _clamp(double value) => value.clamp(minRate, maxRate).toDouble();
}
