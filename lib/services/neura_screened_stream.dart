/// Releases a streamed offline reply sentence by sentence, never ahead of
/// screening.
///
/// Every release screens the whole text released so far plus the new
/// sentence, so the last release screens exactly the complete reply, the same
/// check a non-streamed reply receives. Once screening blocks, nothing more
/// is released and [blocked] holds the safe replacement message.
class NeuraScreenedStream {
  NeuraScreenedStream(this._screen);

  /// Returns null to allow [text], or a safe replacement message to block.
  final Future<String?> Function(String text) _screen;

  final StringBuffer _released = StringBuffer();
  String? _blocked;

  /// Safe replacement message once screening blocked the reply.
  String? get blocked => _blocked;

  /// Screened text released so far.
  String get released => _released.toString();

  /// Offers the unscreened text streamed so far. Returns newly released,
  /// screened text, or an empty string when no whole sentence is ready, the
  /// snapshot does not extend what was released, or the reply is blocked.
  Future<String> offer(String partial) async {
    if (_blocked != null) return '';
    final start = _released.length;
    if (partial.length <= start || !partial.startsWith(released)) return '';
    final end = sentenceEnd(partial, start);
    if (end <= start) return '';
    return _release(partial.substring(start, end));
  }

  /// Releases the rest of the complete reply after screening all of it.
  /// Throws when the complete reply does not extend the released text.
  Future<String> finish(String complete) async {
    if (_blocked != null) return '';
    if (!complete.startsWith(released)) {
      throw StateError('Offline reply changed while streaming.');
    }
    final rest = complete.substring(_released.length);
    if (rest.isEmpty) return '';
    return _release(rest);
  }

  Future<String> _release(String chunk) async {
    final blocked = await _screen('$released$chunk');
    if (blocked != null) {
      _blocked = blocked;
      return '';
    }
    _released.write(chunk);
    return chunk;
  }

  /// End of the last whole sentence in [text] after [from], or -1.
  ///
  /// A boundary is sentence punctuation followed by whitespace, or a line
  /// break. Punctuation at the very end is not yet a boundary: more text may
  /// follow (for example "3." of "3.5").
  static int sentenceEnd(String text, int from) {
    for (var i = text.length - 1; i >= from; i--) {
      final char = text[i];
      if (char == '\n') return i + 1;
      if (i + 1 < text.length &&
          '.!?…'.contains(char) &&
          text[i + 1].trim().isEmpty) {
        return i + 1;
      }
    }
    return -1;
  }
}
