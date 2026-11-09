import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'models.dart';
import 'native.dart';

/// One search: every matching line (catalog order unless [SearchOptions.order]) and the snippet state
/// (prepared groups, distance/order filters, term table). Not thread-safe.
final class SearchSession implements Finalizable {
  /// [streaming]: a session from `se_search_stream` (see [pull]).
  SearchSession.internal(this._index, this._handle, this.contextWords, [this._cancel, bool streaming = false]) {
    _n.searchCloseFinalizer.attach(this, _handle, detach: this);
    hitCount = _n.searchHitCount(_handle);
    if (streaming) {
      isComplete = false;
      pull(0, timeoutMs: 0);
    }
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

  /// Number of matching lines taken in so far (all of them unless the search
  /// streams, see [pull]). On an index without positions some may still fail
  /// the snippet filters; on an index with positions every hit passes
  /// (issue #21).
  late int hitCount;

  /// True once every hit is in: [hitCount] is final and [counts] exact.
  /// Always true unless the search streams ([SeforimIndex.searchStreaming]).
  bool isComplete = true;

  /// A streamed search (issue #21): takes in the hits decided so far, waiting
  /// until at least [minHits] are in, the search is complete, or [timeoutMs]
  /// passes (negative: no limit; 0: only what has arrived). Returns
  /// [isComplete]. A cancelled search throws [SearchCancelledException].
  bool pull(int minHits, {int timeoutMs = -1}) {
    if (isComplete) return true;
    final r = _n.searchStreamPull(_h, minHits < 0 ? 0 : minHits, timeoutMs);
    if (r < 0) throw _n.error();
    hitCount = _n.searchHitCount(_h);
    isComplete = r == 1;
    return isComplete;
  }

  Pointer<Void> get _h {
    if (_handle == nullptr) throw StateError('SearchSession is closed');
    return _handle;
  }

  /// Hits [offset, offset+count), in the search's order ([SearchOptions.order]; catalog by default).
  /// A negative [offset] or [count] throws [ArgumentError].
  List<PlacedHit> hits(int offset, int count) {
    if (offset < 0) throw RangeError.range(offset, 0, null, 'offset');
    if (count < 0) throw RangeError.range(count, 0, null, 'count');
    if (count == 0 || offset >= hitCount) return const [];
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

  /// Every hit, in the search's order, paged. [pageSize] must be positive:
  /// anything else throws [ArgumentError] at the call, before iteration.
  Iterable<PlacedHit> allHits({int pageSize = 4096}) {
    if (pageSize <= 0) throw RangeError.range(pageSize, 1, null, 'pageSize');
    return _allHits(pageSize);
  }

  Iterable<PlacedHit> _allHits(int pageSize) sync* {
    for (var o = 0;; o += pageSize) {
      // a streamed search: wait until the page is in
      if (o + pageSize > hitCount) pull(o + pageSize);
      if (o >= hitCount) return;
      yield* hits(o, pageSize);
    }
  }

  SnippetMatchMode _matchMode = SnippetMatchMode.bestWindow;

  /// What [locateBatch], [locateLines] and [locatePairs] return for a passing
  /// hit (issues #14 / #23): the best window (default) or every match in the
  /// whole text. Which hits pass does not change.
  SnippetMatchMode get matchMode => _matchMode;
  set matchMode(SnippetMatchMode mode) {
    if (_n.searchSetMatchMode(_h, mode.index) != 0) throw _n.error();
    _matchMode = mode;
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

  /// Locates a batch of hits given as their LINES: each hit is its matched
  /// line and, for a short window, its neighbour lines (document order),
  /// located over the non-empty texts joined by single spaces (offsets into
  /// that joined text). [HitLines.hitIndex] is the hit's index in this
  /// search's hits (-1 when unknown). On an index with positions the engine
  /// reads the matched words from the postings instead of searching every
  /// word of the text (faster, same snippets); otherwise it is [locateBatch]
  /// on the joined texts. Cross-line pairs go through [locatePairs].
  List<Snippet?> locateLines(List<HitLines> hits) {
    final n = hits.length;
    final out = List<Snippet?>.filled(n, null);
    if (n == 0) return out;
    final texts = <String>[for (final h in hits) for (final l in h.lines) l.$2];
    final data = using((a) {
      final idx = a<Int64>(n);
      final counts = a<Size>(n);
      final ids = a<Uint64>(texts.isEmpty ? 1 : texts.length);
      var k = 0;
      for (var h = 0; h < n; h++) {
        idx[h] = hits[h].hitIndex;
        counts[h] = hits[h].lines.length;
        for (final l in hits[h].lines) {
          ids[k++] = l.$1;
        }
      }
      return _withTexts(texts, (p, l) => _n.takeBuffer(_n.locateLines(_h, idx, counts, n, ids, p, l)));
    });
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

  // -- User regex, Otzaria's Line kind --

  /// The state of a user regex of Otzaria's Line kind (matched on each line's
  /// text); null for any other search. Not [LineRegexStatus.fullScan]: feed
  /// the candidates' texts (this session's hits, in order) to
  /// [lineMatchBatch]. fullScan: stream every line in scope
  /// ([LineRegexStatus.scopeLines], catalog order) to [lineScanBatch].
  LineRegexStatus? lineRegex() {
    final j = jsonDecode(_n.takeString(_n.lineStatus(_h)));
    return j == null ? null : LineRegexStatus.fromJson(j as Map<String, dynamic>);
  }

  /// Line regex, candidate path: matches the texts of the next
  /// `texts.length` candidates (hit order, continuing from the previous
  /// call), across all cores. Per text: its matches as UTF-16 (start, end)
  /// ranges (empty: no match, or past the 100,000-line cap).
  List<List<(int, int)>> lineMatchBatch(List<String> texts) {
    if (texts.isEmpty) return const [];
    final data = _withTexts(texts, (ptrs, lens) => _n.takeBuffer(_n.lineMatchBatch(_h, ptrs, lens, texts.length)));
    return _ranges(data, texts.length);
  }

  /// Line regex, full-scan path: matches lines the host streams itself
  /// (every line in scope, catalog order), each with its placement.
  List<List<(int, int)>> lineScanBatch(List<PlacedHit> lines, List<String> texts) {
    if (lines.length != texts.length) throw ArgumentError('one text per line');
    if (texts.isEmpty) return const [];
    final raw = calloc<Uint64>(lines.length * 3);
    try {
      for (var i = 0; i < lines.length; i++) {
        raw[i * 3] = lines[i].position;
        raw[i * 3 + 1] = lines[i].lineId;
        raw[i * 3 + 2] = lines[i].bookId;
      }
      final data =
          _withTexts(texts, (ptrs, lens) => _n.takeBuffer(_n.lineScanBatch(_h, raw, ptrs, lens, texts.length)));
      return _ranges(data, texts.length);
    } finally {
      calloc.free(raw);
    }
  }

  static List<List<(int, int)>> _ranges(List<int> data, int n) {
    final out = <List<(int, int)>>[];
    var at = 0;
    for (var i = 0; i < n; i++) {
      final k = data[at++];
      out.add([for (var j = 0; j < k; j++) (data[at + 2 * j], data[at + 2 * j + 1])]);
      at += 2 * k;
    }
    return out;
  }

  /// Line regex: the matching lines kept so far [offset, offset+count), in
  /// feed order.
  List<PlacedHit> lineHits(int offset, int count) {
    if (count <= 0) return const [];
    final raw = calloc<Uint64>(count * 3);
    try {
      final copied = _n.copyLineHits(_h, offset, count, raw);
      final v = raw.asTypedList(copied * 3);
      return [for (var i = 0; i < copied; i++) PlacedHit(v[i * 3], v[i * 3 + 1], v[i * 3 + 2])];
    } finally {
      calloc.free(raw);
    }
  }

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

/// The state of a line-regex search ([SearchSession.lineRegex]).
class LineRegexStatus {
  const LineRegexStatus(this.fullScan, this.scopeLines, this.fed, this.matched, this.truncated, this.done);

  /// No selective fixed word: the host streams every line in scope.
  final bool fullScan;

  /// Lines to feed: the candidates, or every live line in scope.
  final int scopeLines;

  /// Lines in scope fed so far.
  final int fed;

  /// Matching lines kept so far.
  final int matched;

  /// The 100,000-line cap was reached.
  final bool truncated;

  /// Every line in scope was fed, or the cap was reached.
  final bool done;

  static LineRegexStatus fromJson(Map<String, dynamic> j) => LineRegexStatus(j['full_scan'] as bool,
      j['scope_lines'] as int, j['fed'] as int, j['matched'] as int, j['truncated'] as bool, j['done'] as bool);
}

/// How a search was served ([SearchSession.info]).
class SearchInfo {
  const SearchInfo(this.notices, {this.truncated = false, this.orderStamp = ''});

  /// Notice: relevance order was asked for a phrase / proximity / multi-word
  /// fuzzy query on an index without positions; the hits are in catalog order.
  static const relevanceNeedsPositions = 'relevance_needs_positions';

  /// Notice: the index's FM sidecar file is corrupt, truncated or built over
  /// another dictionary; leading-`*` and fuzzy words ran as walks (same hits,
  /// slower). Rebuild it with `SeforimIndex.buildFmSidecar`.
  static const fmSidecarUnusable = 'fm_sidecar_unusable';

  /// Notices about the search (not errors), e.g. [relevanceNeedsPositions].
  final List<String> notices;

  /// A walk (any word, alternative or negative word) hit its expansion cap
  /// (2000 per segment): terms beyond it were not searched.
  final bool truncated;

  /// The identity of the catalog order the hits' positions belong to (the
  /// index snapshot searched). Pass it as [SearchOptions.resumeStamp] with
  /// [SearchOptions.afterPosition] to resume safely (issue #12).
  final String orderStamp;

  static SearchInfo fromJson(Map<String, dynamic> j) => SearchInfo((j['notices'] as List).cast<String>(),
      truncated: j['truncated'] as bool, orderStamp: (j['order_stamp'] as String?) ?? '');
}

/// What a session's locate calls return for a passing hit (issues #14 / #23;
/// FtsLib `SnippetMatchMode`). The index is the engine's code.
enum SnippetMatchMode {
  /// The best window and its occurrences' marks (the default).
  bestWindow,

  /// The whole text (start 0, end its length) and the marks of EVERY full
  /// occurrence in it (the best window's occurrence rule).
  allMatches,
}

/// One occurrence found by [findAllMatches]: UTF-16 [start] and [length] in
/// the text, and the [term] ordinal (first-seen distinct query word).
final class TextMatch {
  const TextMatch(this.start, this.length, this.term);
  final int start;
  final int length;
  final int term;

  @override
  bool operator ==(Object other) =>
      other is TextMatch && other.start == start && other.length == length && other.term == term;

  @override
  int get hashCode => Object.hash(start, length, term);

  @override
  String toString() => '[$start+$length] #$term';
}

/// Find in an open text, no index or session (issues #14 / #23): every
/// occurrence of [words] (each normalized by the line scanner, its words in
/// order) in [text], with the engine's own word rules (HTML, entities, marks,
/// quotes). [phrase]: only runs of the words in order at consecutive
/// positions; [ignoreQuotes]: geresh / gershayim ignored; [matchMarks]: nikud
/// / te'amim must match as typed (by default both sides drop them).
List<TextMatch> findAllMatches(String text, List<String> words,
    {bool phrase = false, bool ignoreQuotes = false, bool matchMarks = false}) {
  if (text.isEmpty || words.isEmpty) return const [];
  final n = SeforimNative.instance;
  final flags = (phrase ? 1 : 0) | (ignoreQuotes ? 2 : 0) | (matchMarks ? 4 : 0);
  final data = using((a) {
    final (tp, tl) = utf8Native(text, a);
    final ptrs = a<Pointer<Uint8>>(words.length);
    final lens = a<Size>(words.length);
    for (var i = 0; i < words.length; i++) {
      final (p, l) = utf8Native(words[i], a);
      ptrs[i] = p;
      lens[i] = l;
    }
    return n.takeBuffer(n.findAllMatches(tp, tl, ptrs, lens, words.length, flags));
  });
  return [for (var i = 0; i + 2 < data.length; i += 3) TextMatch(data[i], data[i + 1], data[i + 2])];
}
