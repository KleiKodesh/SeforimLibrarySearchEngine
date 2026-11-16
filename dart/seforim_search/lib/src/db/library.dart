// The ready-made host API: a few direct calls a screen makes (open, search
// -> ready results with text, snippet and highlights, counts, find in a book,
// build or update the index), with the engine on a worker isolate and
// seforim.db read by line id. Everything below it stays public for hosts that
// need the low-level calls.
import 'dart:async';

import '../../seforim_search.dart';
import 'index_builder.dart';
import 'seforim_db.dart';

/// One ready result: where it is, its text and the highlighted snippet.
final class SearchHit {
  const SearchHit({
    required this.position,
    required this.lineId,
    required this.bookId,
    required this.bookTitle,
    required this.lineIndex,
    required this.reference,
    required this.text,
    required this.snippet,
    this.before = '',
    this.after = '',
    this.nextLineId,
  });

  /// The hit's place in the search's order (catalog order by default).
  final int position;
  final int lineId, bookId, lineIndex;
  final String bookTitle, reference;

  /// The line's full text (HTML as stored); for a cross-line hit the line,
  /// "\n" and the next line ([Snippet.breakAt]).
  final String text;

  /// The window to show and its marks, UTF-16 offsets into [text]. Null when
  /// the line no longer matches (the DB changed since the index was built).
  final Snippet? snippet;

  /// Plain context around a short window (the same book's neighbour lines).
  final String before, after;

  /// A cross-line hit: the next line it continues onto.
  final int? nextLineId;

  /// The highlighted words as (start, end) ranges into [text].
  List<(int, int)> get highlights => [for (final m in snippet?.marks ?? const <SnippetMark>[]) (m.start, m.start + m.length)];
}

/// One page of [SeforimLibrary.search]: the next ready results and the
/// search's progress. [hitCount] is the total once [complete]; [counts]
/// (exact total + per book) comes on the last page when asked for.
final class ResultPage {
  const ResultPage(this.results, this.hitCount, this.complete, [this.counts]);
  final List<SearchHit> results;
  final int hitCount;
  final bool complete;
  final HitCounts? counts;
}

/// One line found by [SeforimLibrary.findInBook] with every occurrence in it.
final class BookMatch {
  const BookMatch(this.lineId, this.lineIndex, this.text, this.matches);
  final int lineId, lineIndex;
  final String text;
  final List<TextMatch> matches;
}

/// seforim.db + its index, ready to search.
final class SeforimLibrary {
  SeforimLibrary._(this.db, this.worker, this.indexDir);

  /// The open database (titles, refs, texts by line id).
  final SeforimDb db;

  /// The engine's worker isolate (low-level calls).
  final SearchWorker worker;
  final String indexDir;
  Map<int, String>? _titles;

  /// Builds the index when missing or stale, else applies the per-book
  /// update; progress until [IndexBuildStage.done]. Call before [open] on a
  /// first run, and on a DB change (close the library first: a rebuild
  /// replaces the directory; then [open] again).
  static Stream<IndexBuildProgress> buildOrUpdateIndex(String dbPath, String indexDir,
          {IndexOptions options = const IndexOptions(), Future<void>? cancel, String? nativePath}) =>
      SeforimIndexBuilder.ensure(dbPath, indexDir, options: options, cancel: cancel, nativePath: nativePath);

  /// True when [indexDir] needs [buildOrUpdateIndex] before [open].
  static bool needsBuild(String indexDir) => SeforimIndexBuilder.needsBuild(indexDir);

  /// Opens the DB ([dbPath], default: the usual install locations) and the
  /// index at [indexDir] on a worker isolate.
  static Future<SeforimLibrary> open(String indexDir, {String? dbPath, String? nativePath}) async {
    final path = dbPath ?? defaultDbPath();
    if (path == null) throw StateError('seforim.db not found');
    final db = SeforimDb.open(path);
    try {
      return SeforimLibrary._(db, await SearchWorker.spawn(indexDir, nativePath: nativePath), indexDir);
    } catch (_) {
      db.close();
      rethrow;
    }
  }

  /// Book titles by id (virtual notes books included), cached.
  Map<int, String> get bookTitles => _titles ??= db.bookTitles();

  /// Runs [options] and streams ready pages of [pageSize] results in the
  /// search's order: the first page as soon as it is decided, then on
  /// demand (a paused listener pauses the search's paging). [offset] skips
  /// the first hits (load more); [limit] > 0 stops the results after that
  /// many. [withCounts]: the last page carries the exact [HitCounts] (the
  /// same search, run to its end; no texts read past [limit]). Cancelling
  /// the subscription (or [cancel]) stops the search.
  Stream<ResultPage> search(SearchOptions options,
      {SearchCancelToken? cancel, int pageSize = 50, int offset = 0, int limit = 0, bool withCounts = false}) {
    final token = cancel ?? SearchCancelToken();
    AsyncSearchSession? session;
    var stopped = false;
    Completer<void>? resumed;
    late final StreamController<ResultPage> out;
    Future<void> run() async {
      try {
        final s = session = await worker.searchStreaming(options, cancel: token);
        var at = offset, sent = 0;
        var done = false;
        while (!stopped && (limit <= 0 || sent < limit)) {
          if (out.isPaused) await (resumed = Completer<void>()).future;
          if (stopped) break;
          final n = limit > 0 && limit - sent < pageSize ? limit - sent : pageSize;
          if (at + n > s.hitCount && !s.isComplete) await s.pull(at + n);
          final placed = at < s.hitCount ? await s.hits(at, n) : const <PlacedHit>[];
          at += placed.length;
          sent += placed.length;
          final ready = await _ready(s, placed, options.contextWords);
          if (stopped) break;
          done = s.isComplete && at >= s.hitCount;
          out.add(ResultPage(ready, s.hitCount, done, done && withCounts ? await _exactCounts(s) : null));
          if (done || placed.isEmpty) break;
        }
        if (withCounts && !done && !stopped) {
          final c = await _exactCounts(s);
          if (!stopped) out.add(ResultPage(const [], s.hitCount, true, c));
        }
      } catch (e, st) {
        if (!stopped) out.addError(e, st);
      } finally {
        await session?.close();
        session = null;
        if (!stopped) await out.close();
      }
    }

    out = StreamController<ResultPage>(
      onListen: run,
      onResume: () {
        final r = resumed;
        resumed = null;
        if (r != null && !r.isCompleted) r.complete();
      },
      onCancel: () {
        stopped = true;
        final r = resumed;
        if (r != null && !r.isCompleted) r.complete();
        if (cancel == null) token.cancel();
      },
    );
    return out.stream;
  }

  Future<List<SearchHit>> _ready(AsyncSearchSession s, List<PlacedHit> placed, int contextWords) async {
    if (placed.isEmpty) return const [];
    final ids = <int>{for (final h in placed) h.lineId, for (final h in placed) ?h.nextLineId};
    final texts = db.lineTexts(ids);
    final pairs = placed.any((h) => h.nextLineId != null);
    final lineTexts = [for (final h in placed) texts[h.lineId] ?? ''];
    final snippets = pairs
        ? await s.locatePairs(lineTexts, [for (final h in placed) h.nextLineId == null ? null : texts[h.nextLineId]])
        : await s.locateBatch(lineTexts);
    final short = [
      for (var i = 0; i < placed.length; i++)
        if (placed[i].nextLineId == null && (snippets[i]?.isShort(contextWords) ?? false)) placed[i].lineId
    ];
    final context = short.isEmpty ? const <int, SnippetText>{} : db.snippetTexts(short, short);
    final where = db.lineLocations(placed.map((h) => h.lineId));
    final refs = db.heRefs(placed.map((h) => h.lineId));
    final titles = bookTitles;
    return [
      for (var i = 0; i < placed.length; i++)
        SearchHit(
          position: placed[i].position,
          lineId: placed[i].lineId,
          bookId: placed[i].bookId,
          bookTitle: titles[placed[i].bookId] ?? '',
          lineIndex: where[placed[i].lineId]?.lineIndex ?? 0,
          reference: refs[placed[i].lineId] ?? '',
          text: placed[i].nextLineId == null
              ? lineTexts[i]
              : '${lineTexts[i]}\n${texts[placed[i].nextLineId] ?? ''}',
          snippet: snippets[i],
          before: context[placed[i].lineId]?.before ?? '',
          after: context[placed[i].lineId]?.after ?? '',
          nextLineId: placed[i].nextLineId,
        ),
    ];
  }

  /// Exact totals of [options]: all lines and per book id. On an index
  /// without positions the lines are validated on their texts (read here).
  Future<HitCounts> counts(SearchOptions options, {SearchCancelToken? cancel}) async {
    final s = await worker.searchStreaming(options, cancel: cancel);
    try {
      return await _exactCounts(s);
    } finally {
      await s.close();
    }
  }

  Future<HitCounts> _exactCounts(AsyncSearchSession s) async {
    while (!s.isComplete) {
      await s.pull(s.hitCount + 65536);
    }
    if (await s.countNeedsText) {
      for (var offset = 0; offset < s.hitCount; offset += 2048) {
        final page = await s.hits(offset, 2048);
        final texts = db.lineTexts({for (final h in page) h.lineId, for (final h in page) ?h.nextLineId});
        final lines = [for (final h in page) texts[h.lineId] ?? ''];
        if (page.any((h) => h.nextLineId != null)) {
          await s.countBatchPairs(lines, [for (final h in page) h.nextLineId == null ? null : texts[h.nextLineId]]);
        } else {
          await s.countBatch(lines);
        }
      }
    }
    return s.counts();
  }

  /// Every occurrence of [words] in the book [bookId] (find in an open book),
  /// with the engine's word rules: the index picks the lines, the text gives
  /// the occurrences. [phrase]: the words in order, adjacent.
  Future<List<BookMatch>> findInBook(int bookId, List<String> words,
      {bool phrase = false, bool ignoreQuotes = false, bool matchMarks = false, SearchCancelToken? cancel}) async {
    if (words.isEmpty) return const [];
    // the words are literal: query syntax characters go (a quote between
    // letters becomes the gershayim the sanitizer folds back)
    final plain = [
      for (final w in words)
        w.replaceAll(RegExp(r'[*?~%{}/|()]'), ' ').replaceAll('"', '״').trim()
    ].where((w) => w.isNotEmpty).toList();
    if (plain.isEmpty) return const [];
    final query = phrase ? '"${plain.join(' ')}"' : plain.join(' | ');
    final ids = <int>[];
    await for (final b in worker.searchStream(SearchOptions(query, books: [bookId]), cancel: cancel)) {
      ids.addAll(b.hits.map((h) => h.lineId));
    }
    final texts = db.lineTexts(ids);
    final where = db.lineLocations(ids);
    final out = <BookMatch>[];
    for (final id in ids) {
      final t = texts[id];
      if (t == null) continue;
      final m = findAllMatches(t, words, phrase: phrase, ignoreQuotes: ignoreQuotes, matchMarks: matchMarks);
      if (m.isNotEmpty) out.add(BookMatch(id, where[id]?.lineIndex ?? 0, t, m));
    }
    return out;
  }

  /// Every occurrence of [words] in an open [text] (no index).
  static List<TextMatch> findInText(String text, List<String> words,
          {bool phrase = false, bool ignoreQuotes = false, bool matchMarks = false}) =>
      findAllMatches(text, words, phrase: phrase, ignoreQuotes: ignoreQuotes, matchMarks: matchMarks);

  /// Picks up an index changed by [buildOrUpdateIndex].
  Future<void> reload() => worker.reload();

  Future<void> close() async {
    await worker.close();
    db.close();
  }
}
