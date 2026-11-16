import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

/// User-selected context caps; no profile changes model or permissions.
///
/// Compact and Standard keep the original native limits. Roomy and Extended
/// ask the native adapter for a larger, separately RAM-admitted profile
/// (`standard` / `extended` in Rust) with longer replies and more recent
/// turns. The native adapter refuses a profile the machine cannot admit; it
/// never silently falls back to a smaller profile or to the cloud.
enum NeuraOfflineBudget {
  compact('Compact', 750),
  standard('Standard', 1500),
  roomy(
    'Roomy',
    4000,
    maxTurns: 12,
    nativeProfile: 'standard',
    minAvailableMiB: 4096,
  ),
  extended(
    'Extended',
    8000,
    maxTurns: 16,
    nativeProfile: 'extended',
    minTotalMiB: 15360,
    minAvailableMiB: 8192,
  );

  const NeuraOfflineBudget(
    this.label,
    this.contextBytes, {
    this.maxTurns = 8,
    this.nativeProfile,
    this.minTotalMiB = 7168,
    this.minAvailableMiB = 3584,
  });
  final String label;
  final int contextBytes;

  /// Most whole turns retained, including an optional conversation brief.
  final int maxTurns;

  /// Native profile name, or null to send the original request shape.
  final String? nativeProfile;

  /// RAM floor mirrored from the native admission check, for display only.
  /// The Rust adapter remains the enforcing check.
  final int minTotalMiB;
  final int minAvailableMiB;

  bool admits(NeuraOfflineMemory memory) =>
      memory.admitted &&
      memory.totalMiB >= minTotalMiB &&
      memory.availableMiB >= minAvailableMiB;

  /// Preserve the newest user turn and whole older turns within the byte cap.
  /// Never truncate user text or include an orphaned assistant response.
  List<Map<String, dynamic>> retain(List<Map<String, dynamic>> messages,
      {String threadContext = ''}) {
    if (messages.isEmpty || messages.last['role'] != 'user') {
      throw StateError('An offline request needs a new user message.');
    }
    final newest = messages.last['content'];
    if (newest is! String) throw StateError('Offline chat accepts text only.');
    // The brief shares the existing budget. Never displace the newest request
    // or expand native context/RAM limits to keep it.
    final threadBytes = utf8.encode(threadContext).length;
    final useThread = threadContext.isNotEmpty &&
        threadBytes <= 450 &&
        threadBytes + utf8.encode(newest).length <= contextBytes;
    var bytes = useThread ? threadBytes : 0;
    final retained = <Map<String, dynamic>>[];
    final turns = useThread ? maxTurns - 1 : maxTurns;
    for (final message in messages.reversed.take(turns)) {
      final text = message['content'];
      if (text is! String ||
          (message['role'] != 'user' && message['role'] != 'assistant')) {
        throw StateError('Offline chat accepts text turns only.');
      }
      final size = utf8.encode(text).length;
      if (bytes + size > contextBytes) {
        if (retained.isEmpty) {
          throw StateError('Shorten this message or choose Standard context.');
        }
        break;
      }
      bytes += size;
      retained.insert(0, Map<String, dynamic>.of(message));
    }
    while (retained.first['role'] != 'user') {
      retained.removeAt(0);
    }
    if (useThread) {
      retained.insert(0, {'role': 'user', 'content': threadContext});
    }
    return retained;
  }
}

/// Content-free RAM snapshot, requested explicitly. No model or network work.
class NeuraOfflineMemory {
  const NeuraOfflineMemory(this.totalMiB, this.availableMiB);
  final int totalMiB;
  final int availableMiB;
  bool get admitted => totalMiB >= 7168 && availableMiB >= 3584;

  static Future<NeuraOfflineMemory?> read({
    MethodChannel channel = const MethodChannel(
      'neura/engine_synthetic_session',
    ),
  }) async {
    try {
      final values = await channel.invokeMapMethod<String, dynamic>(
        'readOfflineMemory',
      );
      if (values == null || values.length != 2) return null;
      final total = values['totalMiB'];
      final available = values['availableMiB'];
      if (total is! int ||
          available is! int ||
          total <= 0 ||
          available < 0 ||
          available > total) {
        return null;
      }
      return NeuraOfflineMemory(total, available);
    } catch (_) {
      return null;
    }
  }
}

/// Session-only "keep Neura warm" choice. Off at every launch; Safe Mode and
/// recovery switch it off. When on, the local model may stay loaded for at
/// most three idle minutes after a reply (Ollama unloads it by itself),
/// trading RAM for a faster next reply. It never starts inference, loads a
/// model or downloads anything.
class NeuraOfflineWarmth {
  NeuraOfflineWarmth._();
  static final ValueNotifier<bool> enabled = ValueNotifier<bool>(false);
}

/// Session-only "show replies as they're written" choice. Off at every
/// launch; Safe Mode and recovery switch it off. When on, offline replies
/// stream sentence by sentence, and each sentence is released only after
/// everything shown so far passes the same guardrails as a complete reply.
/// Reference-shelf answers never stream: they are verified whole.
class NeuraOfflineStreaming {
  NeuraOfflineStreaming._();
  static final ValueNotifier<bool> enabled = ValueNotifier<bool>(false);
}
