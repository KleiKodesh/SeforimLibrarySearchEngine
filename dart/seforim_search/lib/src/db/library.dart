// The ready-made host API: a few direct calls a screen makes (open, search
// -> ready results with text, snippet and highlights, counts, find in a book,
// build or update the index, user documents), with the engine on a worker
// isolate and seforim.db read by line id through the host's
// SeforimDataSource. Everything below it stays public for hosts that need the
// low-level calls.
import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import '../../seforim_search.dart';
import 'data_source.dart';
import 'grouping.dart';
import 'index_builder.dart';
import 'user_documents.dart';

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
    this.userSource,
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

  /// A user document's line ([SeforimIndex.isUserId]): its document key,
  /// locator (e.g. a PDF page) and line number; null for a library line.
  /// [bookTitle] is then the document key, [lineIndex] the line number and
  /// [reference] empty (the host names its documents).
  final UserLineSource? userSource;

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

/// seforim.db + its index, ready to search; optionally a user index of host
/// documents beside it (issue #44).
final class SeforimLibrary {
  SeforimLibrary._(this.source, this.worker, this.indexDir, this._ownsSource);

  /// The host's database access (titles, refs, texts by line id).
  final SeforimDataSource source;
  final bool _ownsSource;

  /// The engine's worker isolate (low-level calls).
  final SearchWorker worker;
  final String indexDir;
  Map<int, String>? _titles;
  Map<int, String>? _userTitles;

  /// Builds the index when missing or stale, else applies the per-book
  /// update; progress until [IndexBuildStage.done]. [open] opens the host's
  /// [SeforimDataSource] on the build isolate. Call before
  /// [SeforimLibrary.open] on a first run, and on a DB change (close the
  /// library first: a rebuild replaces the directory; then open again).
  static Stream<IndexBuildProgress> buildOrUpdateIndex(SeforimDataSourceOpener open, String indexDir,
          {IndexOptions options = const IndexOptions(), Future<void>? cancel, String? nativePath}) =>
      SeforimIndexBuilder.ensure(open, indexDir, options: options, cancel: cancel, nativePath: nativePath);

  /// True when [indexDir] needs [buildOrUpdateIndex] before [open].
  static bool needsBuild(String indexDir, {IndexOptions? options}) =>
      SeforimIndexBuilder.needsBuild(indexDir, options: options);

  /// Opens the index at [indexDir] on a worker isolate, reading the database
  /// through [source] (the host's own data layer). [closeSource]: [close]
  /// also closes [source]. [userIndexDir]: also opens (with
  /// [createUserIndex], creates) that user index and attaches it
  /// ([openUserIndex]).
  static Future<SeforimLibrary> open(String indexDir,
      {required SeforimDataSource source,
      bool closeSource = false,
      String? userIndexDir,
      bool createUserIndex = false,
      String? nativePath}) async {
    final worker = await SearchWorker.spawn(indexDir, nativePath: nativePath);
    final lib = SeforimLibrary._(source, worker, indexDir, closeSource);
    if (userIndexDir != null) {
      try {
        await lib.openUserIndex(userIndexDir, create: createUserIndex);
      } catch (_) {
        await worker.close();
        rethrow;
      }
    }
    return lib;
  }

  /// Book titles by id (virtual notes books included), cached.
  Future<Map<int, String>> bookTitles() async => _titles ??= await source.bookTitles();

  // -- user documents (issue #44) --

  /// Opens the user index at [directory] (with [create], a fresh one with the
  /// library's index options, WIPING the directory) and attaches it:
  /// searches read both by default ([SearchOptions.sources]).
  Future<void> openUserIndex(String directory, {bool create = false}) async {
    var options = const IndexOptions();
    if (create) {
      final i = await worker.info();
      options = IndexOptions(positions: i.positions, keepMarks: i.keepMarks);
    }
    _userTitles = null;
    await worker.openUserIndex(directory, create: create, options: options);
  }

  /// Detaches and closes the user index.
  Future<void> closeUserIndex() {
    _userTitles = null;
    return worker.closeUserIndex();
  }

  /// The user index's documents (key, stamp, book id, line count).
  Future<List<UserDocumentInfo>> documents() => worker.documents();

  /// Where user lines come from (null for a library line).
  Future<List<UserLineSource?>> lineSources(List<int> lineIds) => worker.lineSources(lineIds);

  /// The stored texts of user lines (null for a library line).
  Future<List<String?>> userLineTexts(List<int> lineIds) => worker.lineTexts(lineIds);

  /// Adds or replaces the documents [put] (by key) and deletes the documents
  /// [delete] in ONE commit of the user index; with [merge] (default) then
  /// merges it to one segment and rebuilds its FM sidecar (leading `*`,
  /// fuzzy). Returns how many of [delete] were indexed. [threads] (0 = the
  /// engine's default) and [lowPriority] (indexing threads at background
  /// priority) are passed to the writer: a host's economy indexing mode.
  Future<int> updateUserDocuments(
      {List<UserDocument> put = const [],
      List<String> delete = const [],
      bool merge = true,
      int threads = 0,
      bool lowPriority = false}) async {
    _userTitles = null;
    final w = await worker.openUserWriter(threads: threads, lowPriority: lowPriority);
    var deleted = 0;
    try {
      if (delete.isNotEmpty) deleted = await w.deleteDocuments(delete);
      for (final d in put) {
        await w.putDocument(d.key, d.stamp, d.lines);
      }
      await w.commit();
    } catch (_) {
      await w.close();
      rethrow;
    }
    if (merge) {
      await w.mergeAndClose();
      await worker.reloadUser();
      await worker.buildUserFmSidecar();
    } else {
      await w.close();
    }
    return deleted;
  }

  Future<Map<int, String>> _userBookTitles() async =>
      _userTitles ??= {for (final d in await worker.documents()) d.bookId: d.key};

  /// The texts of [ids]: library lines from [source], user lines from the
  /// user index (it stores them).
  Future<Map<int, String>> _texts(Iterable<int> ids) async {
    final user = <int>[], lib = <int>[];
    for (final id in ids) {
      (_isUser(id) ? user : lib).add(id);
    }
    final out = lib.isEmpty ? <int, String>{} : Map<int, String>.of(await source.lineTexts(lib));
    if (user.isNotEmpty) {
      final t = await worker.lineTexts(user);
      for (var i = 0; i < user.length; i++) {
        if (t[i] case final x?) out[user[i]] = x;
      }
    }
    return out;
  }

  /// Book and line index of [ids] (a user line: its document's book id and
  /// line number), and the user lines' sources.
  Future<(Map<int, LineLocation>, Map<int, UserLineSource>)> _locations(Iterable<int> ids) async {
    final user = <int>[], lib = <int>[];
    for (final id in ids) {
      (_isUser(id) ? user : lib).add(id);
    }
    final where = lib.isEmpty ? <int, LineLocation>{} : Map<int, LineLocation>.of(await source.lineLocations(lib));
    final sources = <int, UserLineSource>{};
    if (user.isNotEmpty) {
      final s = await worker.lineSources(user);
      for (var i = 0; i < user.length; i++) {
        if (s[i] case final x?) {
          sources[user[i]] = x;
          where[user[i]] = (bookId: _userBookOf(user[i]), lineIndex: x.line);
        }
      }
    }
    return (where, sources);
  }

  // the user id range (SeforimIndex.userIdBase, README "User index"): a
  // constant here, so an id check needs no native call on this isolate
  static const _userBase = 1 << 51;
  static bool _isUser(int id) => id >= _userBase && id < 2 * _userBase;

  // user ids: book 2^51 + n, line 2^51 + (n << 24) + line (README "User index")
  static int _userBookOf(int lineId) => _userBase + ((lineId - _userBase) >> 24);

  /// The sameSection key of a user line: its document + locator (e.g. one
  /// PDF page). Negative, so it never meets a library TOC entry id.
  static int _userSectionKey(int lineId, int locator) =>
      -1 - ((((lineId - _userBase) >> 24) << 32) | (locator & 0xffffffff));

  static const _neighbourRadius = 2;

  /// Snippet context of short user rows: the non-empty lines of the same
  /// document within 2 lines on each side, joined by single spaces in
  /// document order (the library's rule, [SeforimDataSource.snippetTexts]).
  Future<Map<int, SnippetText>> _userContext(List<int> ids, Map<int, String> texts) async {
    bool sameDoc(int a, int b) => a >= 0 && a >> 24 == b >> 24;
    final want = <int>{
      for (final id in ids)
        for (var d = -_neighbourRadius; d <= _neighbourRadius; d++)
          if (d != 0 && sameDoc(id + d, id) && !texts.containsKey(id + d)) id + d
    };
    final more = want.isEmpty ? const <int, String>{} : await _texts(want);
    String side(int id, int from, int to) => [
          for (var d = from; d <= to; d++)
            if (sameDoc(id + d, id))
              if ((texts[id + d] ?? more[id + d]) case final t? when t.isNotEmpty) t
        ].join(' ');
    return {
      for (final id in ids)
        if (texts[id] case final t?)
          id: SnippetText(id, t, side(id, -_neighbourRadius, -1), side(id, 1, _neighbourRadius)),
    };
  }

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
    final sections = <int, int>{};
    if (grouping == Grouping.sameSection) {
      final lib = [for (final id in ids) if (!_isUser(id)) id];
      if (lib.isNotEmpty) sections.addAll(await source.sectionsOf(lib));
      // a user line's section is its document + locator (e.g. one PDF page);
      // negative keys never meet a library TOC entry id
      final user = [for (final id in ids) if (_isUser(id)) id];
      if (user.isNotEmpty) {
        final (_, src) = await _locations(user);
        for (final id in user) {
          if (src[id] case final x?) sections[id] = _userSectionKey(id, x.locator);
        }
      }
    }
    final texts = grouping == Grouping.identicalText ? await _texts(ids) : const <int, String>{};
    final groups = groupInOrder<PlacedHit>(
        all,
        (h) => groupKey(grouping,
            tocEntryId: sections[h.lineId] ?? 0, lineId: h.lineId, text: texts[h.lineId]));
    final counts = withCounts ? await _exactCounts(s) : null;
    final end = limit > 0 && offset + limit < groups.length ? offset + limit : groups.length;
    final titles = await bookTitles();
    final userTitles = await _userBookTitles();
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
      final (where, _) = await _locations(sibIds);
      final refs = await source.heRefs([for (final id in sibIds) if (!_isUser(id)) id]);
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
                GroupSibling(h.lineId, h.bookId, titles[h.bookId] ?? userTitles[h.bookId] ?? '',
                    where[h.lineId]?.lineIndex ?? 0, refs[h.lineId] ?? ''),
            ],
            userSource: reps[i].userSource,
          ),
      ], s.hitCount, done, counts: done ? counts : null, terms: await s.takeNewTerms(), groupCount: groups.length));
    }
  }

  Future<List<SearchHit>> _ready(AsyncSearchSession s, List<PlacedHit> placed, int contextWords) async {
    if (placed.isEmpty) return const [];
    final ids = <int>{for (final h in placed) h.lineId, for (final h in placed) ?h.nextLineId};
    final texts = await _texts(ids);
    final pairs = placed.any((h) => h.nextLineId != null);
    final lineTexts = [for (final h in placed) texts[h.lineId] ?? ''];
    final snippets = pairs
        ? await s.locatePairs(lineTexts, [for (final h in placed) h.nextLineId == null ? null : texts[h.nextLineId]])
        : await s.locateBatch(lineTexts);
    final short = [
      for (var i = 0; i < placed.length; i++)
        if (placed[i].nextLineId == null && (snippets[i]?.isShort(contextWords) ?? false)) placed[i].lineId
    ];
    final shortLib = [for (final id in short) if (!_isUser(id)) id];
    final shortUser = [for (final id in short) if (_isUser(id)) id];
    final context = <int, SnippetText>{
      if (shortLib.isNotEmpty) ...await source.snippetTexts(shortLib, shortLib),
      if (shortUser.isNotEmpty) ...await _userContext(shortUser, texts),
    };
    final (where, userSources) = await _locations(placed.map((h) => h.lineId));
    final libIds = [for (final h in placed) if (!_isUser(h.lineId)) h.lineId];
    final refs = libIds.isEmpty ? const <int, String>{} : await source.heRefs(libIds);
    final titles = await bookTitles();
    final userTitles = userSources.isEmpty ? const <int, String>{} : await _userBookTitles();
    return [
      for (var i = 0; i < placed.length; i++)
        SearchHit(
          position: placed[i].position,
          lineId: placed[i].lineId,
          bookId: placed[i].bookId,
          bookTitle: titles[placed[i].bookId] ?? userTitles[placed[i].bookId] ?? '',
          lineIndex: where[placed[i].lineId]?.lineIndex ?? 0,
          reference: refs[placed[i].lineId] ?? '',
          text: placed[i].nextLineId == null
              ? lineTexts[i]
              : '${lineTexts[i]}\n${texts[placed[i].nextLineId] ?? ''}',
          snippet: snippets[i],
          before: context[placed[i].lineId]?.before ?? '',
          after: context[placed[i].lineId]?.after ?? '',
          nextLineId: placed[i].nextLineId,
          userSource: userSources[placed[i].lineId],
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
        final texts = await _texts({for (final h in page) h.lineId, for (final h in page) ?h.nextLineId});
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
    final texts = await _texts(ids);
    final (where, _) = await _locations(ids);
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

  // -- semantic and hybrid search (issue #18) --

  /// Opens Otzaria's vector set at [path] on the worker (map its books with
  /// [worker]'s mapVectorBookRows / setVectorMapping: the rows are the
  /// host's).
  Future<VectorSetInfo> openVectors(String path) => worker.openVectors(path);

  /// Closes the vector set.
  Future<void> closeVectors() => worker.closeVectors();

  /// Loads Otzaria's query model (and optionally its passage model) on the
  /// worker: [SearchWorker.loadEmbedder].
  Future<int> loadEmbedder(String modelDir, String ortLibrary,
          {int threads = 1, String graphFile = 'query.int8.onnx', String? passageModelDir, String? passageGraphFile}) =>
      worker.loadEmbedder(modelDir, ortLibrary,
          threads: threads, graphFile: graphFile, passageModelDir: passageModelDir, passageGraphFile: passageGraphFile);

  /// Frees the worker's embedders.
  Future<void> unloadEmbedder() => worker.unloadEmbedder();

  /// The query's embedding by the loaded model, on the worker.
  Future<Float32List> embedQuery(String query) => worker.embedQuery(query);

  /// Otzaria's hybrid search over the library in one call: the worker embeds
  /// [query] with the loaded embedder (none loaded or no vector set: the
  /// lexical side alone, [HybridMeta.fallbackKind]) and the session reads
  /// line texts from [source] (library lines) and the user index (user
  /// lines) when it needs them. Page it with [AsyncHybridSession.page] (or
  /// [AsyncHybridSession.window] to group), then close it.
  Future<AsyncHybridSession> hybridSearch(String query, {HybridOptions options = const HybridOptions(), Float32List? vector}) =>
      worker.hybridSession(query, vector: vector, options: options, texts: _rawTexts);

  Future<Map<int, Uint8List>> _rawTexts(List<int> ids) async =>
      {for (final e in (await _texts(ids)).entries) e.key: utf8.encode(e.value)};

  /// Otzaria's semantic passage highlights, on the worker with its embedders
  /// ([SearchWorker.semanticPassageHighlights]).
  Future<List<PassageHighlight>> passageHighlights(String query, List<PassageTarget> targets,
          {List<String> forms = const [], int? chunkingIdentity, int maxChars = 800}) =>
      worker.semanticPassageHighlights(query, targets, forms: forms, chunkingIdentity: chunkingIdentity, maxChars: maxChars);

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
    if (_ownsSource) source.close();
  }
}
