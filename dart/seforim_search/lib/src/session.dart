import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'models.dart';
import 'native.dart';

/// One search: every matching line (catalog order unless [SearchOptions.order]) and the snippet state
/// (prepared groups, distance/order filters, term table). Not thread-safe.
final class SearchSession implements Finalizable {
  SearchSession.internal(this._index, this._handle, this.contextWords, [this._cancel]) {
    _n.searchCloseFinalizer.attach(this, _handle, detach: this);
    hitCount = _n.searchHitCount(_handle);
  }

  static SeforimNative get _n => SeforimNative.instance;

  // ignore: unused_field
  final SeforimIndex _index;
  // Keeps the cancel token alive as long as the session.
  // ignore: unused_field
  final SearchCancelToken? _cancel;
  Pointer<Void> _handle;

  /// Words of context per side this search locates with.
  final int contextWords;

  /// Number of matching lines (before the snippet filters).
  late final int hitCount;

  Pointer<Void> get _h {
    if (_handle == nullptr) throw StateError('SearchSession is closed');
    return _handle;
  }

  /// Hits [offset, offset+count), in the search's order ([SearchOptions.order]; catalog by default).
  List<PlacedHit> hits(int offset, int count) {
    if (count <= 0 || offset >= hitCount) return const [];
    if (count > hitCount - offset) count = hitCount - offset;
    final raw = calloc<Uint64>(count * 3);
    try {
      final copied = _n.searchCopyHits(_h, offset, count, raw);
      final v = raw.asTypedList(copied * 3);
      return [
        for (var i = 0; i < copied; i++) PlacedHit(v[i * 3], v[i * 3 + 1], v[i * 3 + 2]),
      ];
    } finally {
      calloc.free(raw);
    }
  }

  /// Every hit, in the search's order, paged.
  Iterable<PlacedHit> allHits({int pageSize = 4096}) sync* {
    for (var o = 0; o < hitCount; o += pageSize) {
      yield* hits(o, pageSize);
    }
  }

  /// Locates the snippets of a batch of matched lines' texts. Null entries are
  /// lines that do not pass the filters. Offsets index each text.
  List<Snippet?> locateBatch(List<String> texts) {
    final n = texts.length;
    final out = List<Snippet?>.filled(n, null);
    if (n == 0) return out;
    final data = _withTexts(texts, (ptrs, lens) => _n.takeBuffer(_n.locateBatch(_h, ptrs, lens, n)));
    var at = 0;
    for (var i = 0; i < n; i++) {
      if (data[at++] == 0) continue;
      final (s, next) = Snippet.read(data, at);
      out[i] = s;
      at = next;
    }
    return out;
  }

  /// Passes [texts] as UTF-8 pointer + length arrays.
  static T _withTexts<T>(List<String> texts, T Function(Pointer<Pointer<Uint8>>, Pointer<Size>) call) =>
      using((a) {
        final n = texts.length;
        final ptrs = a<Pointer<Uint8>>(n == 0 ? 1 : n);
        final lens = a<Size>(n == 0 ? 1 : n);
        for (var i = 0; i < n; i++) {
          final (p, l) = utf8Native(texts[i], a);
          ptrs[i] = p;
          lens[i] = l;
        }
        return call(ptrs, lens);
      });

  /// True when the exact count needs the hits' texts (FtsLib's distance /
  /// order, proximity on an index without positions, the text-level
  /// negative): feed them with [countBatch]. False: every hit passes and
  /// [counts] is exact at once, no text read.
  bool get countNeedsText {
    final r = _n.countNeedsText(_h);
    if (r < 0) throw _n.error();
    return r != 0;
  }

  /// Counts the next `texts.length` hits (hit order, continuing from the
  /// previous call) from their texts, across all cores: the lines that pass
  /// every filter are added to the exact totals. Returns how many passed.
  int countBatch(List<String> texts) {
    final passed = _withTexts(texts, (ptrs, lens) => _n.countBatch(_h, ptrs, lens, texts.length));
    if (passed < 0) throw _n.error();
    return passed;
  }

  /// The exact totals so far: all lines and per book id (Otzaria's count /
  /// countByBook). [HitCounts.exact] once every hit is counted.
  HitCounts counts() =>
      HitCounts.fromJson(jsonDecode(_n.takeString(_n.searchCounts(_h))) as Map<String, dynamic>);

  /// One line's snippet.
  Snippet? locate(String text) => locateBatch([text])[0];

  /// Terms added to the term table since the previous call, in id order.
  /// A mark's termId indexes the concatenation of every call's result.
  List<String> takeNewTerms() =>
      (jsonDecode(_n.takeString(_n.takeNewTerms(_h))) as List).cast<String>();

  void close() {
    if (_handle == nullptr) return;
    _n.searchCloseFinalizer.detach(this);
    _n.searchClose(_handle);
    _handle = nullptr;
  }
}
