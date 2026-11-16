// The ready-made host API: a few direct calls a screen makes (open, search
// -> ready results with text, snippet and highlights, counts, find in a book,
// build or update the index), with the engine on a worker isolate and
// seforim.db read by line id. Everything below it stays public for hosts that
// need the low-level calls.
import 'dart:async';

import '../../seforim_search.dart';
import 'grouping.dart';
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
    this.groupSize = 1,
    this.siblings = const [],
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

  /// Grouped results ([SeforimLibrary.search] `grouping`): the hits in this
  /// hit's group (it is the group's first, its representative) and up to
  /// [mergedSiblingsCap] of the others.
  final int groupSize;
  final List<GroupSibling> siblings;

  /// The highlighted words as (start, end) ranges into [text].
  List<(int, int)> get highlights => [for (final m in snippet?.marks ?? const <SnippetMark>[]) (m.start, m.start + m.length)];
}

/// Another hit of a grouped result's group.
final class GroupSibling {
  const GroupSibling(this.lineId, this.bookId, this.bookTitle, this.lineIndex, this.reference);
  final int lineId, bookId, lineIndex;
  final String bookTitle, reference;
}

/// One page of [SeforimLibrary.search]: the next ready results and the
/// search's progress. [hitCount] is the total once [complete]; [counts]
/// (exact total + per book) comes on the last page when asked for.
final class ResultPage {
  const ResultPage(this.results, this.hitCount, this.complete,
      {this.counts, this.terms = const [], this.groupCount});
  final List<SearchHit> results;
  final int hitCount;
  final bool complete;
  final HitCounts? counts;

  /// Index terms the search matched that no earlier page reported (the
  /// words to highlight in an opened book: [SeforimLibrary.findInText]).
  final List<String> terms;

  /// Grouped search: the number of groups (with [complete]).
  final int? groupCount;
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
  static bool needsBuild(String indexDir, {IndexOptions? options}) =>
      SeforimIndexBuilder.needsBuild(indexDir, options: options);

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
      {SearchCancelToken? cancel,
      int pageSize = 50,
      int offset = 0,
      int limit = 0,
      bool withCounts = false,
      SnippetMatchMode matchMode = SnippetMatchMode.bestWindow,
      Grouping? grouping}) {
    final token = cancel ?? SearchCancelToken();
    AsyncSearchSession? session;
    var stopped = false;
    Completer<void>? resumed;
    late final StreamController<ResultPage> out;
    Future<void> run() async {
      try {
        final s = session = await worker.searchStreaming(options, cancel: token);
        if (matchMode != SnippetMatchMode.bestWindow) await s.setMatchMode(matchMode);
        if (grouping != null) {
          await _grouped(s, out, grouping, options.contextWords, offset, limit, pageSize, withCounts, () => stopped);
          return;
        }
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
          out.add(ResultPage(ready, s.hitCount, done,
              counts: done && withCounts ? await _exactCounts(s) : null, terms: await s.takeNewTerms()));
          if (done || placed.isEmpty) break;
        }
        if (withCounts && !done && !stopped) {
          final c = await _exactCounts(s);
          if (!stopped) out.add(ResultPage(const [], s.hitCount, true, counts: c, terms: await s.takeNewTerms()));
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

  /// Otzaria's grouping, by the host over every hit (grouping.dart): groups
  /// in the order of their first hit; [offset] / [limit] count groups; each
  /// representative is a ready hit with its group size and siblings.
  Future<void> _grouped(AsyncSearchSession s, StreamController<ResultPage> out, Grouping grouping, int contextWords,
      int offset, int limit, int pageSize, bool withCounts, bool Function() stopped) async {
    while (!s.isComplete) {
      await s.pull(s.hitCount + 65536);
    }
    final all = <PlacedHit>[];
    for (var at = 0; at < s.hitCount; at += 65536) {
      all.addAll(await s.hits(at, 65536));
    }
    final ids = [for (final h in all) h.lineId];
    final sections = grouping == Grouping.sameSection ? db.sectionsOf(ids) : const <int, int>{};
    final texts = grouping == Grouping.identicalText ? db.lineTexts(ids) : const <int, String>{};
    final groups = groupInOrder<PlacedHit>(
        all,
        (h) => groupKey(grouping,
            tocEntryId: sections[h.lineId] ?? 0, lineId: h.lineId, text: texts[h.lineId]));
    final counts = withCounts ? await _exactCounts(s) : null;
    final end = limit > 0 && offset + limit < groups.length ? offset + limit : groups.length;
    final titles = bookTitles;
    var at = offset;
    if (at >= end) {
      out.add(ResultPage(const [], s.hitCount, true, counts: counts, groupCount: groups.length));
      return;
    }
    while (at < end && !stopped()) {
      final page = groups.sublist(at, at + pageSize < end ? at + pageSize : end);
      at += page.length;
      final reps = await _ready(s, [for (final g in page) g.members.first], contextWords);
      final sib = [for (final g in page) g.members.skip(1).take(mergedSiblingsCap).toList()];
      final sibIds = [for (final l in sib) for (final h in l) h.lineId];
      final where = db.lineLocations(sibIds);
      final refs = db.heRefs(sibIds);
      final done = at >= end;
      out.add(ResultPage([
        for (var i = 0; i < reps.length; i++)
          SearchHit(
            position: reps[i].position,
            lineId: reps[i].lineId,
            bookId: reps[i].bookId,
            bookTitle: reps[i].bookTitle,
            lineIndex: reps[i].lineIndex,
            reference: reps[i].reference,
            text: reps[i].text,
            snippet: reps[i].snippet,
            before: reps[i].before,
            after: reps[i].after,
            nextLineId: reps[i].nextLineId,
            groupSize: page[i].members.length,
            siblings: [
              for (final h in sib[i])
                GroupSibling(h.lineId, h.bookId, titles[h.bookId] ?? '', where[h.lineId]?.lineIndex ?? 0,
                    refs[h.lineId] ?? ''),
            ],
          ),
      ], s.hitCount, done, counts: done ? counts : null, terms: await s.takeNewTerms(), groupCount: groups.length));
    }
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

  /// Re-sorts the index into the host's own catalog order (book ids, first
  /// first; a book left out goes after the listed ones): one merge on the
  /// worker, then searches come back in that order. A later per-book update
  /// repacks in seforim.db's catalog order again.
  Future<void> resort(List<int> bookOrder, {SearchCancelToken? cancel}) async {
    final w = await worker.openWriter();
    await w.resortAndClose(bookOrder, cancel: cancel);
    await worker.reload();
  }

  /// Picks up an index changed by [buildOrUpdateIndex].
  Future<void> reload() => worker.reload();

  Future<void> close() async {
    await worker.close();
    db.close();
  }
}
