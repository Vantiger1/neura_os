import 'dart:convert';

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/reliability/safe_boot.dart';
import 'package:neuro_companion/services/neura_ai_service.dart';
import 'package:neuro_companion/services/neura_model_router.dart';
import 'package:neuro_companion/services/neura_offline_budget.dart';
import 'package:neuro_companion/services/neura_ollama_chat.dart';

import '../helpers/data_wipe_fixture.dart';

void main() {
  final binding = TestWidgetsFlutterBinding.ensureInitialized();
  const channel = MethodChannel('test/offline_streaming');
  late DataWipeFixture fixture;
  late NeuraOllamaChat adapter;
  late NeuraAiService ai;
  late List<MethodCall> calls;
  late List<String> partials;
  const complete = 'Hello there. How are you? I am here.';

  setUp(() async {
    fixture = await DataWipeFixture.create();
    SafeBoot.instance.safeMode.value = false;
    SafeBoot.instance.restartRequired.value = false;
    binding.handleAppLifecycleStateChanged(AppLifecycleState.resumed);
    calls = [];
    partials = ['Hello there. How', 'Hello there. How are you? I'];
    adapter = NeuraOllamaChat(channel: channel, supported: true);
    ai = NeuraAiService.forTesting(
        modelRouter: NeuraSecureModelRouter(),
        configuredReader: () => false,
        ollama: adapter);
    var polls = 0;
    binding.defaultBinaryMessenger.setMockMethodCallHandler(channel,
        (call) async {
      calls.add(call);
      if (call.method == 'beginOllama') return 1;
      if (call.method == 'pollOllama') {
        return ++polls > partials.length ? complete : null;
      }
      if (call.method == 'partialOllama') {
        return polls <= partials.length ? partials[polls - 1] : null;
      }
      return null;
    });
  });
  tearDown(() {
    NeuraOfflineStreaming.enabled.value = false;
    adapter.dispose();
    binding.defaultBinaryMessenger.setMockMethodCallHandler(channel, null);
  });

  Future<List<String>> ask() {
    const turn = NeuraChatTurn(isUser: true, text: 'Hello');
    return ai.streamReply(const [turn], localOllama: true).toList();
  }

  Map<String, dynamic> request() {
    final begin = calls.firstWhere((c) => c.method == 'beginOllama');
    return jsonDecode(begin.arguments as String) as Map<String, dynamic>;
  }

  test(
      'streaming releases screened sentences that join to the reply',
      () => fixture.run(() async {
            NeuraOfflineStreaming.enabled.value = true;
            final chunks = await ask();
            expect(chunks, ['Hello there.', ' How are you?', ' I am here.']);
            expect(chunks.join(), complete);
            expect(request()['stream'], true);
          }));

  test(
      'a blocked sentence stops native work and shows the safe message',
      () => fixture.run(() async {
            NeuraOfflineStreaming.enabled.value = true;
            const unsafe = "Hello there. I don't want to be alive anymore. ";
            partials = ['Hello there. I', '${unsafe}More'];
            final chunks = await ask();
            expect(chunks.first, 'Hello there.');
            expect(chunks, hasLength(2));
            expect(chunks.last, startsWith('\n\n'));
            expect(chunks.last, contains('I’m here.'));
            expect(chunks.join(), isNot(contains('alive')));
            final cancels = calls.where((c) => c.method == 'cancelOllama');
            expect(cancels.map((c) => c.arguments), contains(1));
            // The stopped request was never read to completion.
            expect(calls.where((c) => c.method == 'partialOllama'),
                hasLength(2));
          }));

  test(
      'abandoning the stream cancels native work',
      () => fixture.run(() async {
            NeuraOfflineStreaming.enabled.value = true;
            const turn = NeuraChatTurn(isUser: true, text: 'Hello');
            final first =
                await ai.streamReply(const [turn], localOllama: true).first;
            expect(first, 'Hello there.');
            await Future<void>.delayed(const Duration(milliseconds: 300));
            final cancels = calls.where((c) => c.method == 'cancelOllama');
            expect(cancels.map((c) => c.arguments), contains(1));
          }));

  test(
      'streaming is off by default and keeps the original request',
      () => fixture.run(() async {
            final chunks = await ask();
            expect(chunks, [complete]);
            expect(request().containsKey('stream'), isFalse);
            expect(calls.where((c) => c.method == 'partialOllama'), isEmpty);
          }));
}
