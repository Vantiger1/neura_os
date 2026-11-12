import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/intelligence/capability_registry.dart';
import 'package:neuro_companion/intelligence/intelligence_preferences.dart';
import 'package:neuro_companion/intelligence/models.dart';
import 'package:neuro_companion/intelligence/neura_intelligence_controller.dart';
import 'package:neuro_companion/intelligence/rupture_repair.dart';
import 'package:neuro_companion/reliability/safe_boot.dart';
import 'package:neuro_companion/startup/neura_prefs_cache.dart';
import 'package:shared_preferences/shared_preferences.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(() async {
    SharedPreferences.setMockInitialValues({});
    await NeuraPrefsCache.init();
  });

  setUp(() async {
    await NeuraPrefsCache.prefs.remove('neura.intelligence.state.v1');
    SafeBoot.instance.safeMode.value = false;
    NeuraIntelligenceController.instance.resetForTesting();
  });

  test('records and updates an explainable suggestion lifecycle', () async {
    final now = DateTime.utc(2026, 8, 14, 18);
    final controller = NeuraIntelligenceController.instance;
    final result = await controller.evaluateCandidates(
      candidates: [_candidate(now)],
      now: now,
    );

    expect(result.visible, hasLength(1));
    final receipt = result.visible.single.receipt;
    expect(controller.records, hasLength(1));

    await controller.markShown(receipt.id);
    expect(
      controller.records.single.receipt.userOutcome,
      NeuraSuggestionOutcome.shown,
    );

    await controller.recordOutcome(receiptId: receipt.id, acted: true);
    await controller.recordOutcome(receiptId: receipt.id, acted: false);
    expect(
      controller.records.single.receipt.userOutcome,
      NeuraSuggestionOutcome.acted,
    );
  });

  test('does not persist private suggestion title or body', () async {
    final controller = NeuraIntelligenceController.instance;
    final now = DateTime.utc(2026, 8, 14, 18);
    await controller.evaluateCandidates(
      candidates: [_candidate(now)],
      now: now,
    );

    final stored = NeuraPrefsCache.prefs.getString(
      'neura.intelligence.state.v1',
    );
    expect(stored, isNotNull);
    expect(stored, isNot(contains('Private title')));
    expect(stored, isNot(contains('Private recommendation body')));
    expect(stored, contains('surface_local_suggestion'));
  });

  test('acted outcomes raise a specialist\'s reputation over time', () async {
    final controller = NeuraIntelligenceController.instance;
    var now = DateTime.utc(2026, 8, 14, 18);
    expect(controller.specialistReputation('controller_test'), 1.0);

    for (var i = 0; i < 5; i++) {
      now = now.add(const Duration(days: 1, hours: 1));
      final result = await controller.evaluateCandidates(
        candidates: [_candidate(now, id: 'candidate-$i')],
        now: now,
      );
      await controller.recordOutcome(
        receiptId: result.visible.single.receipt.id,
        acted: true,
      );
    }

    expect(
      controller.specialistReputation('controller_test'),
      greaterThan(1.0),
    );
  });

  test(
    'dismissed outcomes lower a specialist\'s reputation over time',
    () async {
      final controller = NeuraIntelligenceController.instance;
      var now = DateTime.utc(2026, 8, 14, 18);

      for (var i = 0; i < 5; i++) {
        now = now.add(const Duration(days: 1, hours: 1));
        final result = await controller.evaluateCandidates(
          candidates: [_candidate(now, id: 'candidate-$i')],
          now: now,
        );
        await controller.recordOutcome(
          receiptId: result.visible.single.receipt.id,
          acted: false,
        );
      }

      expect(controller.specialistReputation('controller_test'), lessThan(1.0));
    },
  );

  test(
    'a lowered specialist reputation feeds back into future scoring',
    () async {
      final controller = NeuraIntelligenceController.instance;
      var now = DateTime.utc(2026, 8, 14, 18);

      for (var i = 0; i < 5; i++) {
        now = now.add(const Duration(days: 1, hours: 1));
        final result = await controller.evaluateCandidates(
          candidates: [_candidate(now, id: 'candidate-$i')],
          now: now,
        );
        await controller.recordOutcome(
          receiptId: result.visible.single.receipt.id,
          acted: false,
        );
      }

      now = now.add(const Duration(hours: 1));
      final next = await controller.evaluateCandidates(
        candidates: [_candidate(now, id: 'candidate-final')],
        now: now,
      );
      final earlierScore = controller.records.first.receipt.score;
      expect(next.visible.single.receipt.score, lessThan(earlierScore));
    },
  );

  test('quiet hours suppress a candidate before it reaches the UI', () async {
    final controller = NeuraIntelligenceController.instance;
    await controller.updatePreferences(
      const NeuraIntelligencePreferences(
        quietHoursEnabled: true,
        quietHoursStart: 22,
        quietHoursEnd: 7,
      ),
    );
    final now = DateTime(2026, 8, 14, 23);
    final result = await controller.evaluateCandidates(
      candidates: [_candidate(now)],
      now: now,
    );

    expect(result.visible, isEmpty);
    expect(
      controller.records.single.receipt.outcome,
      NeuraDecisionOutcome.suppressed,
    );
    expect(
      controller.records.single.receipt.reasons.join(' '),
      contains('quiet hours'),
    );
  });

  test(
    'off proactivity suppresses a candidate before it reaches the UI',
    () async {
      final controller = NeuraIntelligenceController.instance;
      await controller.updatePreferences(
        const NeuraIntelligencePreferences(
          enabled: false,
          proactivityLevel: NeuraProactivityLevel.off,
        ),
      );
      final now = DateTime.utc(2026, 8, 14, 18);
      final result = await controller.evaluateCandidates(
        candidates: [_candidate(now)],
        now: now,
      );

      expect(result.visible, isEmpty);
      expect(
        controller.records.single.receipt.reasons.join(' '),
        contains('disabled by the user'),
      );
    },
  );
  group('rupture and repair wiring', () {
    test('repeated dismissals hold later candidates back with a reason',
        () async {
      final controller = NeuraIntelligenceController.instance;
      var now = DateTime.utc(2026, 8, 14, 18);

      // Three dismissals a minute apart cross the "say less" threshold
      // (0.18 each, 30 minute half-life) without reaching the repair one.
      for (var i = 0; i < 3; i++) {
        final result = await controller.evaluateCandidates(
          candidates: [_candidate(now, id: 'dismissed-$i')],
          now: now,
        );
        await controller.recordOutcome(
          receiptId: result.visible.single.receipt.id,
          acted: false,
          now: now,
        );
        now = now.add(const Duration(minutes: 1));
      }

      final state = controller.repairState(now: now);
      expect(state.move, NeuraRepairMove.quietDown);
      expect(state.suggestsSuppression, isTrue);

      final next = await controller.evaluateCandidates(
        candidates: [_candidate(now, id: 'after-dismissals')],
        now: now,
      );
      final receipt = next.decisions.single.receipt;
      expect(receipt.reasons.join(' '), contains('Rupture & Repair'));
      expect(
        receipt.reasons.join(' '),
        contains('A recent dismissal reduced the candidate score.'),
      );
    });

    test('a repair can only begin when the engine asked for one', () async {
      final controller = NeuraIntelligenceController.instance;
      final now = DateTime.utc(2026, 8, 14, 18);

      expect(controller.beginRepair(now: now), isNull);

      controller.observeAttunementSignal(
        NeuraAttunementSignal.explicitCorrection,
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.reflexInterruption,
        now: now,
      );
      expect(
        controller.repairState(now: now).move,
        NeuraRepairMove.acknowledgeAndAsk,
      );

      final id = controller.beginRepair(now: now);
      expect(id, isNotNull);
      expect(controller.beginRepair(now: now), isNull);
    });

    test('completing a repair queues a reviewable correction', () async {
      final controller = NeuraIntelligenceController.instance;
      final now = DateTime.utc(2026, 8, 14, 18);
      controller.observeAttunementSignal(
        NeuraAttunementSignal.explicitCorrection,
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.negativeFeedback,
        now: now,
      );
      final id = controller.beginRepair(now: now)!;

      expect(
        controller.completeRepair(
          repairId: 'wrong',
          category: 'tone',
          userCorrection: 'softer',
          now: now,
        ),
        isNull,
      );
      final proposal = controller.completeRepair(
        repairId: id,
        category: 'tone',
        userCorrection: 'Softer in the evenings.',
        now: now.add(const Duration(minutes: 1)),
      );
      expect(proposal, isNotNull);
      expect(controller.pendingCorrections, hasLength(1));
      expect(controller.pendingCorrections.single.id, id);
      expect(
        controller.repairState(now: now.add(const Duration(minutes: 1))).move,
        NeuraRepairMove.none,
      );

      expect(controller.resolveCorrection('missing'), isFalse);
      expect(controller.resolveCorrection(id), isTrue);
      expect(controller.pendingCorrections, isEmpty);
    });

    test('correction text never reaches the persisted state', () async {
      final controller = NeuraIntelligenceController.instance;
      final now = DateTime.utc(2026, 8, 14, 18);
      await controller.evaluateCandidates(
        candidates: [_candidate(now)],
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.explicitCorrection,
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.negativeFeedback,
        now: now,
      );
      final id = controller.beginRepair(now: now)!;
      controller.completeRepair(
        repairId: id,
        category: 'timing',
        userCorrection: 'Private correction text',
        now: now,
      );
      await controller.updatePreferences(controller.preferences);

      final stored = NeuraPrefsCache.prefs.getString(
        'neura.intelligence.state.v1',
      );
      expect(stored, isNotNull);
      expect(stored, isNot(contains('Private correction text')));
    });

    test('Safe Mode never asks for a repair', () async {
      final controller = NeuraIntelligenceController.instance;
      final now = DateTime.utc(2026, 8, 14, 18);
      SafeBoot.instance.safeMode.value = true;
      controller.observeAttunementSignal(
        NeuraAttunementSignal.explicitCorrection,
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.negativeFeedback,
        now: now,
      );
      expect(controller.repairState(now: now).move, NeuraRepairMove.quietDown);
      expect(controller.beginRepair(now: now), isNull);
    });

    test('a data wipe clears repair pressure and pending corrections',
        () async {
      final controller = NeuraIntelligenceController.instance;
      final now = DateTime.utc(2026, 8, 14, 18);
      controller.observeAttunementSignal(
        NeuraAttunementSignal.explicitCorrection,
        now: now,
      );
      controller.observeAttunementSignal(
        NeuraAttunementSignal.negativeFeedback,
        now: now,
      );
      final id = controller.beginRepair(now: now)!;
      controller.completeRepair(
        repairId: id,
        category: 'other',
        userCorrection: 'anything',
        now: now,
      );
      expect(controller.pendingCorrections, hasLength(1));

      await controller.resetAfterDataWipe();
      controller.resumeAfterDataWipe();

      expect(controller.pendingCorrections, isEmpty);
      expect(controller.repairState(now: now).score, 0);
    });
  });
}

NeuraIntelligenceCandidate _candidate(
  DateTime now, {
  String id = 'controller-candidate',
}) =>
    NeuraIntelligenceCandidate(
      id: id,
      capabilityId: NeuraCapabilityRegistry.surfaceSuggestion,
      specialistId: 'controller_test',
      title: 'Private title',
      body: 'Private recommendation body',
      createdAt: now,
      expiresAt: now.add(const Duration(hours: 1)),
      benefit: 1,
      urgency: 0.8,
      disruption: 0.05,
      confidence: 1,
    );
