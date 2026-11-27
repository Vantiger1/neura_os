import 'package:flutter_test/flutter_test.dart';
import 'package:neuro_companion/services/neura_screened_stream.dart';

void main() {
  test('sentence boundaries need following whitespace or a line break', () {
    expect(NeuraScreenedStream.sentenceEnd('Hello there', 0), -1);
    expect(NeuraScreenedStream.sentenceEnd('Hello there.', 0), -1);
    expect(NeuraScreenedStream.sentenceEnd('Pi is 3.14', 0), -1);
    expect(NeuraScreenedStream.sentenceEnd('Hi. How', 0), 3);
    expect(NeuraScreenedStream.sentenceEnd('Hi. Are you ok? Yes', 0), 15);
    expect(NeuraScreenedStream.sentenceEnd('List\nnext', 0), 5);
    expect(NeuraScreenedStream.sentenceEnd('Hi. How', 3), -1);
  });

  test('releases whole sentences after screening all text shown so far',
      () async {
    final screened = <String>[];
    final stream = NeuraScreenedStream((text) async {
      screened.add(text);
      return null;
    });
    expect(await stream.offer('Hello'), '');
    expect(await stream.offer('Hello there. How'), 'Hello there.');
    expect(await stream.offer('Hello there. How are'), '');
    expect(await stream.offer('Hello there. How are you? I'), ' How are you?');
    final rest = await stream.finish('Hello there. How are you? I am here.');
    expect(rest, ' I am here.');
    expect(stream.released, 'Hello there. How are you? I am here.');
    expect(screened, [
      'Hello there.',
      'Hello there. How are you?',
      'Hello there. How are you? I am here.',
    ]);
    expect(stream.blocked, isNull);
  });

  test('a block releases nothing more and keeps the safe message', () async {
    final stream = NeuraScreenedStream(
      (text) async => text.contains('unsafe') ? 'Safe alternative.' : null,
    );
    expect(await stream.offer('Fine start. Then'), 'Fine start.');
    expect(await stream.offer('Fine start. Then unsafe. More'), '');
    expect(stream.blocked, 'Safe alternative.');
    expect(await stream.offer('Fine start. Then unsafe. More. Yes'), '');
    expect(await stream.finish('Fine start. Then unsafe. Done.'), '');
    expect(stream.released, 'Fine start.');
  });

  test('the final remainder is screened with the complete reply', () async {
    final screened = <String>[];
    final stream = NeuraScreenedStream((text) async {
      screened.add(text);
      return text.endsWith('bad end') ? 'Safe alternative.' : null;
    });
    expect(await stream.offer('Start here. Then'), 'Start here.');
    expect(await stream.finish('Start here. Then bad end'), '');
    expect(screened.last, 'Start here. Then bad end');
    expect(stream.blocked, 'Safe alternative.');
  });

  test('ignores snapshots that do not extend released text', () async {
    final stream = NeuraScreenedStream((_) async => null);
    expect(await stream.offer('One. Two'), 'One.');
    expect(await stream.offer('Different. Text. More'), '');
    expect(await stream.offer('One.'), '');
    await expectLater(stream.finish('Changed reply.'), throwsStateError);
  });

  test('a reply with no sentence break is released whole', () async {
    final stream = NeuraScreenedStream((_) async => null);
    expect(await stream.offer('One long thought'), '');
    expect(await stream.finish('One long thought'), 'One long thought');
    expect(await stream.finish('One long thought'), '');
  });
}
