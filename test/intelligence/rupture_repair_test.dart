import 'dart:math' as math;

import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/intelligence/rupture_repair.dart';

void main() {
  final t0 = DateTime.utc(2026, 11, 10, 9, 0);

  NeuraRuptureRepairEngine engine() => NeuraRuptureRepairEngine(
        idFactory: (now) => 'repair:${now.millisecondsSinceEpoch}',
      );

  group('NeuraRuptureRepairEngine.observe', () {
    test('starts quiet and a single dismissal does not trigger a move', () {
      final e = engine();
      final s = e.observe(NeuraAttunementSignal.dismissedSuggestion, now: t0);

      expect(s.score, closeTo(0.18, 1e-9));
      expect(s.move, NeuraRepairMove.none);
      expect(s.suggestsSuppression, isFalse);
      expect(s.reasons.single, 'Attunement pressure is low.');
    });

    test('repeated dismissals ask Neura to say less', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.dismissedSuggestion, now: t0);
      final s = e.observe(
        NeuraAttunementSignal.dismissedSuggestion,
        now: t0.add(const Duration(minutes: 1)),
      );

      expect(s.move, NeuraRepairMove.quietDown);
      expect(s.suggestsSuppression, isTrue);
    });

    test('a correction plus an interruption asks for a repair', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      final s = e.observe(
        NeuraAttunementSignal.reflexInterruption,
        now: t0.add(const Duration(minutes: 1)),
      );

      expect(s.score, greaterThanOrEqualTo(0.6));
      expect(s.move, NeuraRepairMove.acknowledgeAndAsk);
    });

    test('positive signals relieve pressure and the score never goes below 0',
        () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      final relieved = e.observe(
        NeuraAttunementSignal.positiveFeedback,
        now: t0.add(const Duration(seconds: 30)),
      );
      // 0.45 decayed over 30 s of a 30 min half-life, minus 0.30.
      expect(
        relieved.score,
        closeTo(0.45 * math.pow(2, -30 / 1800) - 0.30, 1e-9),
      );
      expect(relieved.score, greaterThan(0));

      final floor = e.observe(
        NeuraAttunementSignal.positiveFeedback,
        now: t0.add(const Duration(seconds: 40)),
      );
      expect(floor.score, 0);
    });

    test('pressure decays with the configured half-life', () {
      final e = NeuraRuptureRepairEngine(halfLife: const Duration(minutes: 30));
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);

      final later = e.state(now: t0.add(const Duration(minutes: 30)));
      expect(later.score, closeTo(0.20, 1e-6));

      final muchLater = e.state(now: t0.add(const Duration(hours: 6)));
      expect(muchLater.score, 0);
      expect(muchLater.move, NeuraRepairMove.none);
    });

    test('a clock that moves backwards does not inflate the score', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      final earlier = e.state(now: t0.subtract(const Duration(hours: 1)));
      expect(earlier.score, closeTo(0.40, 1e-9));
    });
  });

  group('repair lifecycle', () {
    test('beginRepair only hands out an id when a repair is due', () {
      final e = engine();
      expect(e.beginRepair(now: t0), isNull);

      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      final id = e.beginRepair(now: t0);
      expect(id, 'repair:${t0.millisecondsSinceEpoch}');

      final waiting = e.state(now: t0.add(const Duration(minutes: 1)));
      expect(waiting.pendingRepairId, id);
      expect(waiting.move, NeuraRepairMove.quietDown);
      expect(
        waiting.reasons.single,
        'A repair question is still waiting for an answer.',
      );
    });

    test('completeRepair returns a proposal and relieves pressure', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.dismissedSuggestion, now: t0);
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      final id = e.beginRepair(now: t0)!;

      final proposal = e.completeRepair(
        repairId: id,
        category: 'timing',
        userCorrection: '  Not during my morning focus block.  ',
        now: t0.add(const Duration(minutes: 2)),
      );

      expect(proposal, isNotNull);
      expect(proposal!.id, id);
      expect(proposal.category, 'timing');
      expect(proposal.userCorrection, 'Not during my morning focus block.');
      expect(proposal.triggerSignals, [
        NeuraAttunementSignal.dismissedSuggestion,
        NeuraAttunementSignal.explicitCorrection,
        NeuraAttunementSignal.negativeFeedback,
      ]);

      final after = e.state(now: t0.add(const Duration(minutes: 2)));
      expect(after.pendingRepairId, isNull);
      expect(after.score, lessThan(e.quietThreshold));
      expect(after.move, NeuraRepairMove.none);
    });

    test('completeRepair rejects wrong id, unknown category and blank text',
        () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      final id = e.beginRepair(now: t0)!;
      final later = t0.add(const Duration(minutes: 1));

      expect(
        e.completeRepair(
          repairId: 'other',
          category: 'tone',
          userCorrection: 'softer',
          now: later,
        ),
        isNull,
      );
      expect(
        e.completeRepair(
          repairId: id,
          category: 'diagnosis',
          userCorrection: 'softer',
          now: later,
        ),
        isNull,
      );
      expect(
        e.completeRepair(
          repairId: id,
          category: 'tone',
          userCorrection: '   ',
          now: later,
        ),
        isNull,
      );
      expect(e.state(now: later).pendingRepairId, id);
    });

    test('an unanswered repair expires and the cooldown blocks a second ask',
        () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      final id = e.beginRepair(now: t0)!;

      final expired = t0.add(const Duration(minutes: 11));
      expect(e.state(now: expired).pendingRepairId, isNull);
      expect(
        e.completeRepair(
          repairId: id,
          category: 'tone',
          userCorrection: 'softer',
          now: expired,
        ),
        isNull,
      );

      // Pressure is still high but the cooldown has not elapsed.
      e.observe(NeuraAttunementSignal.explicitCorrection, now: expired);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: expired);
      final blocked = e.state(now: expired);
      expect(blocked.move, NeuraRepairMove.quietDown);
      expect(
        blocked.reasons.single,
        'Repair cooldown active; saying less instead of asking again.',
      );
      expect(e.beginRepair(now: expired), isNull);

      final afterCooldown = t0.add(const Duration(hours: 1, minutes: 1));
      e.observe(NeuraAttunementSignal.explicitCorrection, now: afterCooldown);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: afterCooldown);
      expect(
        e.state(now: afterCooldown).move,
        NeuraRepairMove.acknowledgeAndAsk,
      );
    });

    test('Safe Mode never asks a repair question but still quiets down', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      final s = e.observe(
        NeuraAttunementSignal.negativeFeedback,
        now: t0,
        safeMode: true,
      );

      expect(s.move, NeuraRepairMove.quietDown);
      expect(s.reasons.single, contains('Safe Mode'));
      expect(e.beginRepair(now: t0, safeMode: true), isNull);
    });

    test('reset clears pressure, history and pending repair', () {
      final e = engine();
      e.observe(NeuraAttunementSignal.explicitCorrection, now: t0);
      e.observe(NeuraAttunementSignal.negativeFeedback, now: t0);
      e.beginRepair(now: t0);
      e.reset();

      final s = e.state(now: t0);
      expect(s.score, 0);
      expect(s.pendingRepairId, isNull);
      expect(s.lastRepairAt, isNull);
      expect(s.move, NeuraRepairMove.none);
    });
  });
}
