// Snippet texts on demand (KitveiHakodesh's model: Core SeedSnippetTextHitCount,
// the service's ftsSnippetTexts and the page's fullTextSearchSnippets.ts).
//
// A hit's snippet window is located once, during the search; the texts it is
// drawn over are read only for the rows a page shows: the first
// [seedSnippetTextHitCount] hits' texts ride along with the search (the batch
// already read them to locate), every later row asks for its text by line id,
// at most [snippetFetchBatch] ids per request, and a SHORT row (fewer words
// than the context) gets its +-2 neighbour lines in the same request (one SQL
// statement, SeforimDb.snippetTexts). Host code: it knows seforim.db.
import 'dart:async';

import 'seforim_db.dart';

/// Leading hits whose texts ride along with the search (Core's
/// `SeedSnippetTextHitCount`): about a screenful or two.
const seedSnippetTextHitCount = 100;

/// Line ids per on-demand request (the page's FETCH_BATCH).
const snippetFetchBatch = 200;

/// Texts kept by line id (the page's TEXT_CAP).
const snippetTextCacheCapacity = 1500;

/// Rows fetched ahead of / behind the rows shown (the results list's lookahead).
const snippetLookAhead = 60, snippetLookBehind = 20;

/// Reads the texts of [lineIds], with neighbour context for [shortLineIds]
/// (SeforimDb.snippetTexts). A line with no text row is absent.
typedef SnippetTextFetch = FutureOr<Map<int, SnippetText>> Function(List<int> lineIds, List<int> shortLineIds);

/// A row that needs its snippet text: the line and whether its window is short.
typedef SnippetRow = ({int lineId, bool short});

/// The page's snippet-text cache: LRU by line id ([capacity]), requests split
/// at [batch] ids, a line already in flight awaited instead of asked again.
class SnippetTextCache {
  SnippetTextCache(this._fetch, {this.capacity = snippetTextCacheCapacity, this.batch = snippetFetchBatch});

  final SnippetTextFetch _fetch;
  final int capacity, batch;

  // insertion order = LRU order (a hit moves the entry to the end)
  final _texts = <int, (SnippetText?, bool)>{}; // text (null: no row), read with context
  final _inFlight = <int, Future<void>>{};

  /// Requests sent so far (each at most [batch] ids).
  int requests = 0;

  /// Seeds texts that came with the search (Core's FtsStreamChunk.SnippetTexts).
  /// [withContext]: they were read as short rows (with their neighbours).
  void seed(Iterable<SnippetText> texts, {bool withContext = false}) {
    for (final t in texts) {
      _put(t.lineId, t, withContext || t.before.isNotEmpty || t.after.isNotEmpty);
    }
  }

  /// The cached text a row can render from, or null when it must be fetched
  /// (a line cached without context is a miss for a short row). A line known
  /// to have no text row reads as an empty text.
  SnippetText? textOf(SnippetRow row) {
    final e = _texts.remove(row.lineId);
    if (e == null) return null;
    _texts[row.lineId] = e;
    if (row.short && !e.$2) return null;
    return e.$1 ?? SnippetText(row.lineId, '', '', '');
  }

  bool has(SnippetRow row) {
    final e = _texts[row.lineId];
    return e != null && (!row.short || e.$2);
  }

  /// Completes once every row's text is cached: the missing ones are fetched in
  /// requests of at most [batch] ids (short rows ask for context in the same
  /// request), the ones another call already has in flight are awaited.
  Future<void> ensure(Iterable<SnippetRow> rows) async {
    final list = rows.toList();
    await _ensureOnce(list);
    // a short row that waited on a PLAIN read of its line still lacks its context
    final again = [for (final r in list) if (r.short && !has(r) && _texts.containsKey(r.lineId)) r];
    if (again.isNotEmpty) await _ensureOnce(again);
  }

  Future<void> _ensureOnce(List<SnippetRow> rows) async {
    final missing = <SnippetRow>[];
    final seen = <int>{};
    final waits = <Future<void>>{};
    for (final r in rows) {
      if (has(r)) continue;
      if (!seen.add(r.lineId)) {
        // the same line as plain and short: ask it short (the text covers both)
        final k = missing.indexWhere((m) => m.lineId == r.lineId);
        if (r.short && k >= 0) missing[k] = r;
        continue;
      }
      final pending = _inFlight[r.lineId];
      if (pending != null) {
        waits.add(pending);
        continue;
      }
      missing.add(r);
    }
    for (var i = 0; i < missing.length; i += batch) {
      final part = missing.sublist(i, i + batch < missing.length ? i + batch : missing.length);
      final plain = [for (final r in part) if (!r.short) r.lineId];
      final shorts = [for (final r in part) if (r.short) r.lineId];
      requests++;
      final f = Future(() => _fetch(plain, shorts)).then((got) {
        // an empty answer to a non-empty ask is not cached (asked again next time)
        if (got.isEmpty) return;
        for (final id in plain) {
          _put(id, got[id], false);
        }
        for (final id in shorts) {
          _put(id, got[id], true);
        }
      });
      final done = f.whenComplete(() {
        for (final r in part) {
          _inFlight.remove(r.lineId);
        }
      });
      for (final r in part) {
        _inFlight[r.lineId] = done;
      }
      waits.add(done);
    }
    await Future.wait(waits);
  }

  /// [ensure] for the rows shown, [first] .. [first] + [count], plus the
  /// lookahead ([snippetLookAhead] ahead, [snippetLookBehind] behind) of
  /// [rows] (all the rows known so far, in display order). Completes when the
  /// shown rows can render; the lookahead goes in the same requests.
  Future<void> ensureAround(List<SnippetRow> rows, int first, int count) {
    final from = first - snippetLookBehind < 0 ? 0 : first - snippetLookBehind;
    final end = first + count + snippetLookAhead;
    final to = end > rows.length ? rows.length : end;
    // the shown rows first, so a request split never leaves them for later
    final shownEnd = first + count > rows.length ? rows.length : first + count;
    return ensure([...rows.sublist(first < rows.length ? first : rows.length, shownEnd), ...rows.sublist(from, to)]);
  }

  void _put(int id, SnippetText? text, bool withContext) {
    final old = _texts.remove(id);
    // a plain read never downgrades a cached text with context
    if (old != null && old.$2 && !withContext) {
      _texts[id] = old;
    } else {
      _texts[id] = (text, withContext);
    }
    while (_texts.length > capacity) {
      _texts.remove(_texts.keys.first);
    }
  }
}
