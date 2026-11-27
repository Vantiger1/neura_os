import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/attunement/vocal_entrainment_policy.dart';

void main() {
  const policy = NeuraVocalEntrainmentPolicy();

  NeuraEntrainmentDecision resolve({
    bool enabled = true,
    bool safeMode = false,
    bool crisis = false,
    double currentRate = 0.5,
    double preferredRate = 0.5,
    double? syllableRate = 4.5,
    double arousal = 0.45,
    double confidence = 0.9,
  }) {
    return policy.resolve(
      enabled: enabled,
      safeMode: safeMode,
      crisisPlaybookActive: crisis,
      currentRate: currentRate,
      preferredRate: preferredRate,
      userSyllableRateHz: syllableRate,
      arousal: arousal,
      confidence: confidence,
    );
  }

  group('gates', () {
    test('disabled, Safe Mode and crisis hold the current rate untouched', () {
      for (final decision in [
        resolve(enabled: false, currentRate: 0.6),
        resolve(safeMode: true, currentRate: 0.6),
        resolve(crisis: true, currentRate: 0.6),
      ]) {
        expect(decision.rate, 0.6);
        expect(decision.changed, isFalse);
      }
      expect(resolve(enabled: false).reason, NeuraEntrainmentReason.disabled);
      expect(resolve(safeMode: true).reason, NeuraEntrainmentReason.safeMode);
      expect(
        resolve(crisis: true).reason,
        NeuraEntrainmentReason.crisisPlaybook,
      );
    });

    test('no measurement or low confidence drifts back to the preference', () {
      final none = resolve(
        syllableRate: null,
        currentRate: 0.6,
        preferredRate: 0.5,
      );
      expect(none.reason, NeuraEntrainmentReason.noMeasurement);
      expect(none.rate, closeTo(0.55, 1e-9));
      expect(none.changed, isTrue);

      final unsure = resolve(
        confidence: 0.1,
        currentRate: 0.5,
        preferredRate: 0.5,
      );
      expect(unsure.reason, NeuraEntrainmentReason.lowConfidence);
      expect(unsure.changed, isFalse);
    });
  });

  group('co-regulation', () {
    test('fast aroused speech makes Neura lead slower than the matched rate',
        () {
      // 7 syllables/s maps to the fast TTS rate 0.65; leading 10 % slower
      // targets 0.585, reached in one 0.05 step from 0.55.
      final decision = resolve(
        syllableRate: 7.0,
        arousal: 0.8,
        currentRate: 0.55,
      );
      expect(decision.reason, NeuraEntrainmentReason.leadingSlower);
      expect(decision.rate, closeTo(0.585, 1e-9));
      expect(decision.changed, isTrue);
    });

    test('a step never exceeds maxStepPerTurn', () {
      final decision = resolve(
        syllableRate: 7.0,
        arousal: 0.9,
        currentRate: 0.30,
      );
      expect(decision.rate, closeTo(0.35, 1e-9));
    });

    test('flat delivery gets a gentle lift relative to the preference', () {
      final decision = resolve(
        syllableRate: 2.0,
        arousal: 0.1,
        currentRate: 0.5,
        preferredRate: 0.5,
      );
      expect(decision.reason, NeuraEntrainmentReason.gentleLift);
      expect(decision.rate, closeTo(0.525, 1e-9));
    });

    test('mid-range arousal holds the user preference', () {
      final decision = resolve(
        arousal: 0.45,
        currentRate: 0.5,
        preferredRate: 0.5,
      );
      expect(decision.reason, NeuraEntrainmentReason.holdingPreference);
      expect(decision.changed, isFalse);
    });

    test('rates always stay inside the TTS service clamp', () {
      final high = resolve(
        syllableRate: 2.0,
        arousal: 0.0,
        currentRate: 0.75,
        preferredRate: 0.75,
      );
      expect(high.rate, 0.75);
      expect(high.changed, isFalse);

      final low = resolve(
        syllableRate: 9.0,
        arousal: 1.0,
        currentRate: 0.25,
        preferredRate: 0.25,
      );
      expect(low.rate, greaterThanOrEqualTo(0.25));
      expect(low.rate, lessThanOrEqualTo(0.75));
    });

    test('out-of-range inputs are clamped rather than rejected', () {
      final decision = resolve(
        currentRate: 5.0,
        preferredRate: -1.0,
        arousal: 0.45,
      );
      expect(decision.rate, closeTo(0.70, 1e-9));
      expect(decision.reason, NeuraEntrainmentReason.holdingPreference);
    });
  });

  test('decisions compare by value', () {
    expect(
      resolve(),
      const NeuraEntrainmentDecision(
        rate: 0.5,
        reason: NeuraEntrainmentReason.holdingPreference,
        changed: false,
      ),
    );
  });
}
