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
    final next = calloc<Uint64>(count);
    try {
      final copied = _n.searchCopyHits(_h, offset, count, raw);
      _n.searchCopyNextLines(_h, offset, count, next);
      final v = raw.asTypedList(copied * 3);
      // u64::MAX (no next line) reads as -1 in a Dart int
      final nx = next.asTypedList(copied);
      return [
        for (var i = 0; i < copied; i++)
          PlacedHit(v[i * 3], v[i * 3 + 1], v[i * 3 + 2], nx[i] == -1 ? null : nx[i]),
      ];
    } finally {
      calloc.free(raw);
      calloc.free(next);
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

  /// [locateBatch] for hits given with their next line's text (cross-line
  /// matching; null for a hit without [PlacedHit.nextLineId]). The line
  /// alone is tried first; a cross-line snippet has [Snippet.breakAt] and
  /// offsets into `text + "\n" + next`.
  List<Snippet?> locatePairs(List<String> texts, List<String?> nextTexts) {
    final n = texts.length;
    final out = List<Snippet?>.filled(n, null);
    if (n == 0) return out;
    final data = _withPairs(texts, nextTexts,
        (p, l, np, nl) => _n.takeBuffer(_n.locatePairs(_h, p, l, np, nl, n)));
    var at = 0;
    for (var i = 0; i < n; i++) {
      final kind = data[at++];
      if (kind == 0) continue;
      final brk = kind == 2 ? data[at++] : null;
      final (s, next) = Snippet.read(data, at, brk);
      out[i] = s;
      at = next;
    }
    return out;
  }

  /// [countBatch] for hits given with their next line's text.
  int countBatchPairs(List<String> texts, List<String?> nextTexts) {
    final passed =
        _withPairs(texts, nextTexts, (p, l, np, nl) => _n.countBatchPairs(_h, p, l, np, nl, texts.length));
    if (passed < 0) throw _n.error();
    return passed;
  }

  static T _withPairs<T>(List<String> texts, List<String?> nextTexts,
      T Function(Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>, Pointer<Size>) call) {
    if (nextTexts.length != texts.length) throw ArgumentError('one next text (or null) per text');
    return using((a) {
      final n = nextTexts.length;
      final ptrs = a<Pointer<Uint8>>(n == 0 ? 1 : n);
      final lens = a<Size>(n == 0 ? 1 : n);
      for (var i = 0; i < n; i++) {
        final t = nextTexts[i];
        if (t == null) {
          ptrs[i] = nullptr;
          lens[i] = 0;
        } else {
          final (p, l) = utf8Native(t, a);
          ptrs[i] = p;
          lens[i] = l;
        }
      }
      return _withTexts(texts, (p, l) => call(p, l, ptrs, lens));
    });
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

  /// How the search was served: its [SearchInfo.notices] (not errors; e.g.
  /// [SearchInfo.relevanceNeedsPositions]) and [SearchInfo.truncated].
  SearchInfo info() =>
      SearchInfo.fromJson(jsonDecode(_n.takeString(_n.searchInfo(_h))) as Map<String, dynamic>);

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

/// How a search was served ([SearchSession.info]).
class SearchInfo {
  const SearchInfo(this.notices, {this.truncated = false});

  /// Notice: relevance order was asked for a phrase / proximity / multi-word
  /// fuzzy query on an index without positions; the hits are in catalog order.
  static const relevanceNeedsPositions = 'relevance_needs_positions';

  /// Notices about the search (not errors), e.g. [relevanceNeedsPositions].
  final List<String> notices;

  /// A walk (any word, alternative or negative word) hit its expansion cap
  /// (2000 per segment): terms beyond it were not searched.
  final bool truncated;

  static SearchInfo fromJson(Map<String, dynamic> j) =>
      SearchInfo((j['notices'] as List).cast<String>(), truncated: j['truncated'] as bool);
}
