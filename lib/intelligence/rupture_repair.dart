import 'dart:math' as math;

import 'package:flutter/foundation.dart';

/// Content-free evidence that Neura may have misattuned, or that the
/// relationship is on solid ground. The engine never parses text; callers
/// map existing events (a dismissed card, a reflex command, a correction the
/// user explicitly flagged) onto these signals.
enum NeuraAttunementSignal {
  /// A proactive card was dismissed with "Not now".
  dismissedSuggestion,

  /// The user said Neura misunderstood (caller-detected, e.g. a correction
  /// chip or the compassionate policy's `frustrated` support signal).
  explicitCorrection,

  /// A deterministic reflex command (`stop`, `be quiet`, `cancel`) fired
  /// while Neura was speaking or acting.
  reflexInterruption,

  /// Thumbs-down or equivalent negative feedback on a reply.
  negativeFeedback,

  /// A proactive card was acted on.
  acceptedSuggestion,

  /// Thumbs-up, thanks, or equivalent positive feedback.
  positiveFeedback,

  /// A repair move was answered by the user.
  repairAnswered,
}

/// What Neura should do about the current attunement state.
enum NeuraRepairMove {
  /// Carry on; nothing to repair.
  none,

  /// Say less: suppress non-urgent initiative until the score settles.
  quietDown,

  /// Acknowledge once and ask what she got wrong. Never more than once per
  /// [NeuraRuptureRepairEngine.repairCooldown].
  acknowledgeAndAsk,
}

/// The reviewable outcome of a repair: a proposal the user can accept into
/// memory or discard. It carries only a category and the user's own words
/// as they supplied them; Neura never infers the correction.
@immutable
class NeuraCorrectionProposal {
  const NeuraCorrectionProposal({
    required this.id,
    required this.category,
    required this.userCorrection,
    required this.proposedAt,
    required this.triggerSignals,
  });

  final String id;

  /// One of [NeuraCorrectionProposal.categories].
  final String category;

  /// The user's correction verbatim. Stays in the quarantined proposal until
  /// the user admits it through the existing memory review flow.
  final String userCorrection;
  final DateTime proposedAt;

  /// Signals that led to the repair, oldest first.
  final List<NeuraAttunementSignal> triggerSignals;

  static const List<String> categories = [
    'timing',
    'tone',
    'topic',
    'frequency',
    'misunderstanding',
    'other',
  ];
}

@immutable
class NeuraRuptureState {
  const NeuraRuptureState({
    required this.score,
    required this.move,
    required this.lastSignalAt,
    required this.lastRepairAt,
    required this.pendingRepairId,
    required this.reasons,
  });

  /// 0..1 rupture pressure; decays with time and with positive signals.
  final double score;
  final NeuraRepairMove move;
  final DateTime? lastSignalAt;
  final DateTime? lastRepairAt;

  /// Set while an `acknowledgeAndAsk` is waiting for the user's answer.
  final String? pendingRepairId;
  final List<String> reasons;

  /// True when the Autonomy Kernel should treat the user as having recently
  /// declined (its existing `recentlyDeclined` input), so initiative backs
  /// off through the path it already has rather than a new one.
  bool get suggestsSuppression => move != NeuraRepairMove.none;
}

/// Rupture-and-repair: the part of a good relationship that happens after a
/// misstep. The engine keeps a decaying pressure score from content-free
/// signals. When pressure crosses a threshold it asks Neura to say less;
/// when it crosses a higher one it asks her to acknowledge and ask what went
/// wrong, at most once per cooldown. The user's answer becomes a
/// [NeuraCorrectionProposal] that must still pass the memory review flow.
///
/// This is a pure, deterministic state machine. It stores no text except the
/// user's own correction inside a proposal, executes nothing, and is
/// tightening-only: it can make Neura quieter, never louder.
class NeuraRuptureRepairEngine {
  NeuraRuptureRepairEngine({
    this.halfLife = const Duration(minutes: 30),
    this.quietThreshold = 0.35,
    this.repairThreshold = 0.60,
    this.repairCooldown = const Duration(hours: 1),
    this.repairAnswerWindow = const Duration(minutes: 10),
    String Function(DateTime now)? idFactory,
  })  : assert(quietThreshold > 0 && quietThreshold < repairThreshold),
        assert(repairThreshold <= 1),
        _idFactory = idFactory ?? _defaultId;

  final Duration halfLife;
  final double quietThreshold;
  final double repairThreshold;
  final Duration repairCooldown;
  final Duration repairAnswerWindow;
  final String Function(DateTime now) _idFactory;

  double _score = 0;
  DateTime? _lastSignalAt;
  DateTime? _lastRepairAt;
  String? _pendingRepairId;
  DateTime? _pendingRepairAt;
  final List<NeuraAttunementSignal> _recent = [];

  static const int _recentCap = 12;

  /// Pressure added (positive) or relieved (negative) by each signal.
  static const Map<NeuraAttunementSignal, double> weights = {
    NeuraAttunementSignal.dismissedSuggestion: 0.18,
    NeuraAttunementSignal.explicitCorrection: 0.45,
    NeuraAttunementSignal.reflexInterruption: 0.35,
    NeuraAttunementSignal.negativeFeedback: 0.40,
    NeuraAttunementSignal.acceptedSuggestion: -0.20,
    NeuraAttunementSignal.positiveFeedback: -0.30,
    NeuraAttunementSignal.repairAnswered: -0.75,
  };

  /// Fold a signal in and return the resulting state.
  NeuraRuptureState observe(
    NeuraAttunementSignal signal, {
    required DateTime now,
    bool safeMode = false,
  }) {
    _decayTo(now);
    _score = (_score + (weights[signal] ?? 0)).clamp(0.0, 1.0).toDouble();
    _lastSignalAt = now;
    _recent.add(signal);
    while (_recent.length > _recentCap) {
      _recent.removeAt(0);
    }
    if (signal == NeuraAttunementSignal.repairAnswered) {
      _pendingRepairId = null;
      _pendingRepairAt = null;
    }
    return state(now: now, safeMode: safeMode);
  }

  /// Current state without recording a signal.
  NeuraRuptureState state({required DateTime now, bool safeMode = false}) {
    _decayTo(now);
    _expirePendingRepair(now);
    final reasons = <String>[];
    var move = NeuraRepairMove.none;

    if (safeMode) {
      reasons.add('Safe Mode: repair prompts are paused; Neura stays quiet.');
      move = _score >= quietThreshold
          ? NeuraRepairMove.quietDown
          : NeuraRepairMove.none;
    } else if (_pendingRepairId != null) {
      move = NeuraRepairMove.quietDown;
      reasons.add('A repair question is still waiting for an answer.');
    } else if (_score >= repairThreshold && _repairAllowed(now)) {
      move = NeuraRepairMove.acknowledgeAndAsk;
      reasons.add(
        'Several misattunement signals in a row; acknowledge once and ask.',
      );
    } else if (_score >= quietThreshold) {
      move = NeuraRepairMove.quietDown;
      reasons.add(
        _score >= repairThreshold
            ? 'Repair cooldown active; saying less instead of asking again.'
            : 'Attunement pressure is elevated; saying less for now.',
      );
    } else {
      reasons.add('Attunement pressure is low.');
    }

    return NeuraRuptureState(
      score: _score,
      move: move,
      lastSignalAt: _lastSignalAt,
      lastRepairAt: _lastRepairAt,
      pendingRepairId: _pendingRepairId,
      reasons: List.unmodifiable(reasons),
    );
  }

  /// Mark that Neura actually delivered the acknowledge-and-ask move.
  /// Returns the repair id the answer must reference, or null if no repair
  /// was due (so a caller cannot force a repair the engine did not ask for).
  String? beginRepair({required DateTime now, bool safeMode = false}) {
    final current = state(now: now, safeMode: safeMode);
    if (current.move != NeuraRepairMove.acknowledgeAndAsk) return null;
    final id = _idFactory(now);
    _pendingRepairId = id;
    _pendingRepairAt = now;
    _lastRepairAt = now;
    return id;
  }

  /// Turn the user's answer into a reviewable proposal. Returns null when
  /// the id does not match the pending repair, the window has passed, the
  /// category is unknown, or the correction is blank.
  NeuraCorrectionProposal? completeRepair({
    required String repairId,
    required String category,
    required String userCorrection,
    required DateTime now,
  }) {
    _expirePendingRepair(now);
    if (_pendingRepairId == null || _pendingRepairId != repairId) return null;
    if (!NeuraCorrectionProposal.categories.contains(category)) return null;
    final trimmed = userCorrection.trim();
    if (trimmed.isEmpty) return null;

    final triggers = List<NeuraAttunementSignal>.unmodifiable(
      _recent.where((s) => (weights[s] ?? 0) > 0),
    );
    observe(NeuraAttunementSignal.repairAnswered, now: now);
    return NeuraCorrectionProposal(
      id: repairId,
      category: category,
      userCorrection: trimmed,
      proposedAt: now,
      triggerSignals: triggers,
    );
  }

  /// Forget all pressure and history, e.g. on wipe or "clear conversation".
  void reset() {
    _score = 0;
    _lastSignalAt = null;
    _lastRepairAt = null;
    _pendingRepairId = null;
    _pendingRepairAt = null;
    _recent.clear();
  }

  bool _repairAllowed(DateTime now) {
    final last = _lastRepairAt;
    return last == null || !now.isBefore(last.add(repairCooldown));
  }

  void _expirePendingRepair(DateTime now) {
    final at = _pendingRepairAt;
    if (at != null && now.isAfter(at.add(repairAnswerWindow))) {
      _pendingRepairId = null;
      _pendingRepairAt = null;
    }
  }

  void _decayTo(DateTime now) {
    final last = _lastSignalAt;
    if (last == null || _score == 0) return;
    final elapsed = now.difference(last);
    if (elapsed.isNegative) return;
    final halfLives = elapsed.inMilliseconds / halfLife.inMilliseconds;
    _score = _score * math.pow(2, -halfLives).toDouble();
    if (_score < 0.005) _score = 0;
    _lastSignalAt = now;
  }

  static String _defaultId(DateTime now) =>
      'repair:${now.toUtc().microsecondsSinceEpoch}';
}
