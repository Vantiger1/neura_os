import 'dart:convert';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/services/neura_offline_budget.dart';

Map<String, dynamic> turn(String role, String text) =>
    {'role': role, 'content': text};

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('compact preserves newest user text without orphan assistant context',
      () {
    final messages = [
      turn('user', 'a' * 500),
      turn('assistant', 'b' * 300),
      turn('user', 'c' * 450)
    ];
    final retained = NeuraOfflineBudget.compact.retain(messages);
    expect(retained, [messages.last]);
    expect(messages.length, 3);
  });

  test('standard keeps complete recent text and copies retained maps', () {
    final messages = [
      turn('user', 'hello'),
      turn('assistant', 'hi'),
      turn('user', 'next')
    ];
    final retained = NeuraOfflineBudget.standard.retain(messages);
    expect(retained, messages);
    retained.last['content'] = 'changed';
    expect(messages.last['content'], 'next');
  });

  test('UTF-8 cap rejects newest turn rather than truncating it', () {
    expect(() => NeuraOfflineBudget.compact.retain([turn('user', '🙂' * 188)]),
        throwsStateError);
    expect(
        NeuraOfflineBudget.standard
            .retain([turn('user', '🙂' * 188)]).single['content'],
        '🙂' * 188);
  });

  test('history count stays bounded with first user and last user', () {
    final messages = [
      for (var i = 0; i < 25; i++) turn(i.isEven ? 'user' : 'assistant', '$i')
    ];
    final retained = NeuraOfflineBudget.standard.retain(messages);
    expect(retained.length, 7);
    expect(retained.first['role'], 'user');
    expect(retained.last['content'], '24');
  });

  test('original budgets keep their native shape and limits', () {
    for (final budget in [
      NeuraOfflineBudget.compact,
      NeuraOfflineBudget.standard,
    ]) {
      expect(budget.nativeProfile, isNull);
      expect(budget.maxTurns, 8);
      expect(budget.minTotalMiB, 7168);
      expect(budget.minAvailableMiB, 3584);
    }
    expect(NeuraOfflineBudget.compact.contextBytes, 750);
    expect(NeuraOfflineBudget.standard.contextBytes, 1500);
  });

  test('larger budgets retain more whole turns within their caps', () {
    final messages = [
      for (var i = 0; i < 31; i++)
        turn(i.isEven ? 'user' : 'assistant', '$i ${'x' * 400}')
    ];
    int count(NeuraOfflineBudget budget) {
      final retained = budget.retain(messages);
      expect(retained.first['role'], 'user');
      expect(retained.last, messages.last);
      expect(retained.length, lessThanOrEqualTo(budget.maxTurns));
      expect(
          retained.fold<int>(
              0, (n, m) => n + utf8.encode(m['content'] as String).length),
          lessThanOrEqualTo(budget.contextBytes));
      return retained.length;
    }

    final standard = count(NeuraOfflineBudget.standard);
    final roomy = count(NeuraOfflineBudget.roomy);
    final extended = count(NeuraOfflineBudget.extended);
    expect(standard, lessThan(roomy));
    expect(roomy, lessThan(extended));
    expect(NeuraOfflineBudget.roomy.nativeProfile, 'standard');
    expect(NeuraOfflineBudget.extended.nativeProfile, 'extended');
  });

  test('larger budgets mirror stricter native RAM floors', () {
    const small = NeuraOfflineMemory(8192, 3584);
    const roomyOk = NeuraOfflineMemory(8192, 4096);
    const large = NeuraOfflineMemory(16384, 8192);
    expect(NeuraOfflineBudget.standard.admits(small), isTrue);
    expect(NeuraOfflineBudget.roomy.admits(small), isFalse);
    expect(NeuraOfflineBudget.roomy.admits(roomyOk), isTrue);
    expect(NeuraOfflineBudget.extended.admits(roomyOk), isFalse);
    expect(NeuraOfflineBudget.extended.admits(large), isTrue);
  });

  test('keep-warm is off by default', () {
    expect(NeuraOfflineWarmth.enabled.value, isFalse);
  });

  test('rejects tool, image and missing current user messages', () {
    expect(() => NeuraOfflineBudget.standard.retain([]), throwsStateError);
    expect(
        () => NeuraOfflineBudget.standard.retain([turn('assistant', 'reply')]),
        throwsStateError);
    expect(
        () => NeuraOfflineBudget.standard
            .retain([turn('tool', 'secret'), turn('user', 'hello')]),
        throwsStateError);
    expect(
        () => NeuraOfflineBudget.standard.retain([
              {
                'role': 'user',
                'content': <String>['image']
              }
            ]),
        throwsStateError);
  });

  test('memory snapshot uses strict schema and conservative floor', () async {
    const channel = MethodChannel('test/offline_memory');
    final messenger =
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
    for (final value in [
      null,
      {'totalMiB': 8192},
      {'totalMiB': '8192', 'availableMiB': 4096},
      {'totalMiB': 8192, 'availableMiB': 9000},
      {'totalMiB': 8192, 'availableMiB': -1}
    ]) {
      messenger.setMockMethodCallHandler(channel, (call) async {
        expect(call.method, 'readOfflineMemory');
        expect(call.arguments, isNull);
        return value;
      });
      expect(await NeuraOfflineMemory.read(channel: channel), isNull);
    }
    messenger.setMockMethodCallHandler(
        channel, (_) async => {'totalMiB': 8192, 'availableMiB': 3584});
    expect((await NeuraOfflineMemory.read(channel: channel))!.admitted, true);
    expect(const NeuraOfflineMemory(8192, 3583).admitted, false);
    expect(const NeuraOfflineMemory(7167, 4096).admitted, false);
    messenger.setMockMethodCallHandler(channel, null);
    expect(await NeuraOfflineMemory.read(channel: channel), isNull);
  });
}
