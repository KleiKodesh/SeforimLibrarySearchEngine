import 'dart:io';

import 'package:otzaria_demo/book_updates.dart';
import 'package:otzaria_demo/otzaria_query.dart';
import 'package:otzaria_demo/seforim_db.dart';
import 'package:otzaria_demo/note_ids.dart';
import 'package:otzaria_demo/acronym_lexicon.dart';
import 'package:otzaria_demo/magic_dictionary.dart';
import 'package:otzaria_demo/facets.dart';
import 'package:otzaria_demo/grouping.dart';
import 'package:otzaria_demo/snippet_texts.dart';
import 'package:otzaria_demo/translation_lexicon.dart';
import 'package:seforim_search/seforim_search.dart';

const _usage = '''
otzaria_demo build  [--db path] [--index dir] [--max-lines N] [--keep-marks] [--no-positions]
otzaria_demo search [--db path] [--index dir] [options] <query>
otzaria_demo update [--db path] [--index dir] [--wait] [--watch]
                    per-book update by book.contentHash, then the background merge

search options (Otzaria names):
  --mode exact|advanced|fuzzy     (default advanced)
  --grammar-prefixes  קידומות דקדוקיות     --grammar-suffixes  סיומות דקדוקיות
  --prefixes          קידומות              --suffixes          סיומות
  --ktiv              כתיב מלא/חסר          --partial           חלק ממילה
  --typos             שגיאות כתיב           --ignore-gershayim  התעלם מגרשיים
  --aramaic-prefixes  קידומות ארמיות        --aramaic-suffixes  סיומות ארמיות
  --nikud / --teamim  require typed marks (needs a --keep-marks index)
  --prefix-max N --suffix-max N --partial-max N --grammar-prefix-max N   (walk limits)
  --distance N        word gap (default 0)  --any-order
  --spacing a,b,..    gap per consecutive word pair (customSpacing; needs positions)
  --fuzzy-distance N  (fuzzy mode, 1..2)
  --lexical path      Otzaria's lexical.db (MagicDictionary): fuzzy mode adds each word's
                      dictionary forms (blacklisted lemmas searched, not marked)
  --match all|any|most|atleast  (wordMatchMode; default all)   --match-count N  (for atleast)
  --acronym           ראשי תיבות (needs --acronyms path to Otzaria's assets/Acronyms.json)
  --translation       תרגום ארמי (needs --dictionary path to Otzaria's assets/dictionary.json)
  --exclude "words"   negative query (same distance/order; a,b = alternatives of a negative word)
  --exclude-options grammar-prefixes,ktiv,...   the negative words' own options (Otzaria's
                      negative options, on every negative word; "" = none). Absent: the options above
  --exclude-scope distance|paragraph|section   (negativeScope; default distance; section: in the engine)
  --scope section     Otzaria's SameSection scope (words anywhere under one heading)
  --cross-line        a match may continue onto the next line (Otzaria's continues_to_next_line)
  --book TEXT         only books whose title/category contains TEXT
  --group section|identical  (Otzaria's grouping; limit/offset then count groups)
  --group-scan N      identical: hits read for signatures (default 20000)
  --limit N (100: rows per page) --offset N (0) --pages N (1: pages shown one after another)  --context N (8)
  --count             exact totals: all lines passing every filter, and per book
  --order catalog|relevance  results order (Otzaria's sortBy; default catalog)
  word alternatives: write a,b in the query (Otzaria sets them per word in the UI)
  library paths: --native path (seforim_search_native), --zstd path (libzstd)
''';

Map<String, String?> _parse(List<String> args, List<String> rest) {
  final m = <String, String?>{};
  const valued = {
    'db', 'index', 'max-lines', 'mode', 'prefix-max', 'suffix-max', 'partial-max', 'grammar-prefix-max', //
    'distance', 'fuzzy-distance', 'book', 'limit', 'offset', 'pages', 'context', 'native', 'zstd', 'spacing', 'match',
    'match-count', 'exclude', 'exclude-scope', 'exclude-options', 'dictionary', 'acronyms', 'group', 'group-scan',
    'order', 'scope', 'lexical',
  };
  for (var i = 0; i < args.length; i++) {
    final a = args[i];
    if (a.startsWith('--')) {
      final k = a.substring(2);
      m[k] = valued.contains(k) && i + 1 < args.length ? args[++i] : null;
    } else {
      rest.add(a);
    }
  }
  return m;
}

int? _int(Map<String, String?> o, String k) => o[k] == null ? null : int.parse(o[k]!);

Future<void> main(List<String> args) async {
  if (args.isEmpty) {
    stdout.write(_usage);
    exit(64);
  }
  final rest = <String>[];
  final o = _parse(args.sublist(1), rest);
  SeforimNative.load(o['native']);
  final dbPath = o['db'] ?? defaultDbPath();
  if (dbPath == null) {
    stderr.writeln('seforim.db not found; pass --db');
    exit(66);
  }
  final indexDir = o['index'] ?? '${Directory.systemTemp.path}/otzaria_demo_index';
  final db = SeforimDb.open(dbPath, zstdLib: o['zstd']);
  try {
    switch (args[0]) {
      case 'build':
        _build(db, indexDir, o);
      case 'update':
        // issue #29: changed / new / removed books re-indexed, then the background merge
        final index = SeforimIndex.open(indexDir);
        try {
          exitCode = await runUpdateCommand(index, db, wait: o.containsKey('wait'), watch: o.containsKey('watch'));
        } finally {
          index.close();
        }
      case 'search':
        db.clearUnreadable();
        await _search(db, indexDir, o, rest.join(' '));
        // issue #13: unreadable rows were read as empty, never silently
        if (db.unreadableLineIds.isNotEmpty) {
          final ids = db.unreadableLineIds.take(20).join(', ');
          stderr.writeln('${db.unreadableLineIds.length} rows unreadable (line ids: $ids'
              '${db.unreadableLineIds.length > 20 ? ', ...' : ''})');
        }
      default:
        stdout.write(_usage);
        exit(64);
    }
  } finally {
    db.close();
  }
}

void _build(SeforimDb db, String dir, Map<String, String?> o) {
  final maxLines = _int(o, 'max-lines') ?? 0;
  db.checkNoteIds(); // issue #13: fail loudly before any line goes in
  final sw = Stopwatch()..start();
  stdout.writeln('DB: ${db.path} (split=${db.splitContent}, zstd=${db.compressed})');
  final index = SeforimIndex.create(
      dir, IndexOptions(positions: !o.containsKey('no-positions'), keepMarks: o.containsKey('keep-marks')));
  final w = index.openWriter(); // auto mode (issue #19)
  var position = 0;
  // lines go to the engine in batches (one FFI call per 1000 lines)
  final batch = <LineDocument>[];
  // this feeding thread (DB reads) in the OS background mode too; commits,
  // the final merge and the FM sidecar leave it by themselves
  runInBackgroundPriority(() {
    try {
      outer:
      for (final bookId in db.catalogBookOrder()) {
        for (final line in db.bookLines(bookId)) {
          if (maxLines > 0 && position >= maxLines) break outer;
          batch.add(LineDocument(
              lineId: line.id,
              position: position++,
              bookId: line.bookId,
              text: line.text,
              // the section (TOC entry; 0 = none: a section of its own) and
              // Otzaria's rule: a heading interrupts the text (no cross-line phrase)
              sectionId: line.tocEntryId == 0 ? null : line.tocEntryId,
              noCross: line.text.startsWith('<h') || isVirtualId(line.id))); // a footnote stands alone (issue #13)
          if (batch.length == 1000) {
            w.addAll(batch);
            batch.clear();
          }
          if (position % 100000 == 0) stdout.writeln('  $position lines, ${sw.elapsed.inSeconds}s');
        }
      }
      w.addAll(batch);
      w.commit('{"lines":$position,"db":"${db.fingerprint()}","notes":${db.hasNotes ? 1 : 0}}');
    } catch (_) {
      w.rollback();
      rethrow;
    } finally {
      w.close();
    }
  });
  // One segment (the walks and their caps run per segment), then the
  // FM-index sidecar for leading-* wildcards and fuzzy words.
  index.openWriter().mergeAndClose();
  final fmWatch = Stopwatch()..start();
  try {
    final built = index.buildFmSidecar();
    final fmi = Directory(dir).listSync().whereType<File>().where((f) => f.path.endsWith('.fmi'))
        .fold<int>(0, (n, f) => n + f.lengthSync());
    stdout.writeln(built
        ? 'FM sidecar: ${fmWatch.elapsedMilliseconds} ms, ${(fmi / 1e6).toStringAsFixed(1)} MB'
        : 'FM sidecar not built (several segments or no terms): leading-* and fuzzy use the walk');
  } catch (e) {
    stderr.writeln('FM sidecar FAILED: $e (the index is complete; leading-* and fuzzy use the walk)');
    rethrow;
  }
  stdout.writeln('Indexed $position lines into $dir in ${sw.elapsed.inMilliseconds} ms; '
      'payload ${index.info().payload}');
  index.close();
}

/// Issue #13: a hit on a notes line names its source.
String _noteTag(SeforimDb db, int lineId, int bookId) {
  final s = db.noteSource(lineId, bookId);
  return s == null ? '' : ' [note ${s.noteIndex} of line ${s.sourceLineId}, book ${s.sourceBookId}]';
}

const _open = '⟦', _close = '⟧';

/// Snippet text with marks bracketed and HTML stripped. Each piece is
/// stripped on its own, so a mark that ends inside a tag cannot swallow
/// its bracket.
String _render(String text, Snippet s) {
  String plain(String t) => t.replaceAll('&nbsp;', ' ').replaceAll(RegExp(r'<[^>]*>?'), ' ').replaceAll(RegExp(r'^[^<]*>'), ' ');
  final b = StringBuffer();
  if (s.leadingEllipsis) b.write('… ');
  var at = s.start;
  for (final m in s.marks) {
    if (m.start < at || m.start + m.length > s.end) continue;
    b
      ..write(plain(text.substring(at, m.start)))
      ..write(_open)
      ..write(plain(text.substring(m.start, m.start + m.length)).trim())
      ..write(_close);
    at = m.start + m.length;
  }
  b.write(plain(text.substring(at, s.end)));
  if (s.trailingEllipsis) b.write(' …');
  return b.toString().replaceAll(RegExp(r'\s+'), ' ').trim();
}

/// The page's withContext (KitveiHakodesh fullTextSearchSnippets.ts): plain
/// context for a short window, the last [words] words of the text before and
/// the first [words] after, tags stripped, "…" where the context is cut. A side
/// the window itself already cut (its own "…") gets no context.
String _withContext(String window, SnippetText text, Snippet s, int words) {
  List<String> wordsOf(String t) =>
      t.replaceAll('&nbsp;', ' ').replaceAll(RegExp(r'<[^>]*>?'), ' ').split(RegExp(r'\s+')).where((w) => w.isNotEmpty).toList();
  var out = window;
  if (text.before.isNotEmpty && !s.leadingEllipsis) {
    final all = wordsOf(text.before);
    final kept = all.length > words ? all.sublist(all.length - words) : all;
    if (kept.isNotEmpty) out = '${kept.length < all.length ? '…' : ''}${kept.join(' ')} $out';
  }
  if (text.after.isNotEmpty && !s.trailingEllipsis) {
    final all = wordsOf(text.after);
    final kept = all.length > words ? all.sublist(0, words) : all;
    if (kept.isNotEmpty) out = '$out ${kept.join(' ')}${kept.length < all.length ? '…' : ''}';
  }
  return out;
}

Future<void> _search(SeforimDb db, String dir, Map<String, String?> o, String query) async {
  if (query.trim().isEmpty) {
    stderr.writeln('empty query');
    exit(64);
  }
  final index = SeforimIndex.open(dir);
  final info = index.info();
  final words = splitQueryWords(query);
  // a,b alternatives per word (Otzaria's alternativeWords map).
  final alts = <int, List<String>>{};
  final mainWords = <String>[];
  for (var i = 0; i < words.length; i++) {
    final parts = words[i].split(',').where((p) => p.isNotEmpty).toList();
    mainWords.add(parts.first);
    if (parts.length > 1) alts[i] = parts.sublist(1);
  }
  const flagToOption = {
    'grammar-prefixes': OtzariaOption.grammaticalPrefixes,
    'grammar-suffixes': OtzariaOption.grammaticalSuffixes,
    'prefixes': OtzariaOption.prefixes,
    'suffixes': OtzariaOption.suffixes,
    'ktiv': OtzariaOption.fullOrDefective,
    'partial': OtzariaOption.partial,
    'typos': OtzariaOption.typoTolerance,
    'ignore-gershayim': OtzariaOption.ignoreGershayim,
    'aramaic-prefixes': OtzariaOption.aramaicPrefixes,
    'aramaic-suffixes': OtzariaOption.aramaicSuffixes,
    'nikud': OtzariaOption.nikud,
    'teamim': OtzariaOption.teamim,
    'translation': OtzariaOption.aramaicTranslation,
    'acronym': OtzariaOption.acronyms,
  };
  // The negative words: a,b alternatives per word (negativeAlternativeWords)
  // and, with --exclude-options, their own option set on every negative word
  // (Otzaria's negative options as the app sends them, per word).
  final negWords = <String>[];
  final negAlts = <int, List<String>>{};
  for (final w in splitQueryWords(o['exclude'] ?? '')) {
    final parts = w.split(',').where((p) => p.isNotEmpty).toList();
    if (parts.isEmpty) continue;
    if (parts.length > 1) negAlts[negWords.length] = parts.sublist(1);
    negWords.add(parts.first);
  }
  final negOptions = o['exclude-options']
      ?.split(',')
      .map((f) => f.trim())
      .where((f) => f.isNotEmpty)
      .map((f) => flagToOption[f] ?? (throw ArgumentError('--exclude-options: unknown option "$f"')))
      .toSet();
  final contextWords = _int(o, 'context') ?? 8;
  final req = OtzariaSearch(
    query: mainWords.join(' '),
    mode: SearchMode.values.byName(o['mode'] ?? 'advanced'),
    globalOptions: {for (final e in flagToOption.entries) if (o.containsKey(e.key)) e.value: true},
    alternativeWords: alts,
    distance: _int(o, 'distance') ?? 0,
    anyOrder: o.containsKey('any-order'),
    wordMatchMode: switch (o['match'] ?? 'all') {
      'any' => WordMatchMode.anyWord,
      'most' => WordMatchMode.mostWords,
      'atleast' => WordMatchMode.atLeast,
      _ => WordMatchMode.all,
    },
    wordMatchCount: _int(o, 'match-count') ?? 1,
    negativeQuery: negWords.join(' '),
    negativeWordOptions: negOptions == null ? null : {for (var i = 0; i < negWords.length; i++) i: negOptions},
    negativeAlternativeWords: negAlts,
    acronymLexicon: o['acronyms'] == null ? null : AcronymLexicon.load(o['acronyms']!),
    translations: o['dictionary'] == null ? null : TranslationLexicon.load(o['dictionary']!),
    magicDictionary: o['lexical'] == null ? null : MagicDictionary.open(o['lexical']!),
    negativeScope: switch (o['exclude-scope'] ?? 'distance') {
      'paragraph' => NegativeScope.sameParagraph,
      'section' => NegativeScope.sameSection,
      _ => NegativeScope.wordDistance,
    },
    fuzzyDistance: _int(o, 'fuzzy-distance') ?? 1,
    prefixMaxLetters: _int(o, 'prefix-max'),
    suffixMaxLetters: _int(o, 'suffix-max'),
    partOfWordMaxLetters: _int(o, 'partial-max'),
    grammarPrefixMaxLetters: _int(o, 'grammar-prefix-max'),
    marksIndex: info.keepMarks,
    customSpacing: o['spacing'] == null
        ? null
        : {for (final (i, g) in o['spacing']!.split(',').indexed) i: int.parse(g.trim())},
  );
  final request = req.toOptions(contextWords: contextWords);
  final bookFilter = o['book'] == null ? null : db.booksMatching(o['book']!);
  // filtered search by book: done by the engine (book_id is in the index)
  if (bookFilter != null) request.books = bookFilter.toList();
  // Otzaria's sortBy. Its generation order needs the app's per-book
  // generationOrder (chronologicalOrderForBook), which this demo does not port.
  request.order = switch (o['order'] ?? 'catalog') {
    'relevance' => ResultsOrder.relevance,
    'catalog' || 'catalogue' => ResultsOrder.catalog,
    final other => throw ArgumentError('--order: catalog or relevance, not "$other"'),
  };
  // relevance: base books (isBaseBook) first within each tier (issue #6)
  if (request.order == ResultsOrder.relevance) request.baseBooks.addAll(db.baseBooks());
  req.warnings.toSet().forEach((w) => stderr.writeln('warning: $w'));
  request.crossLine = o.containsKey('cross-line');
  request.sameSection = o['scope'] == 'section';
  // positions index: each hit keeps the window decided during the search
  // (word-level descriptor); display only finishes it with the text
  request.wordWindows = info.positions;

  final limit = _int(o, 'limit') ?? 100, offset = _int(o, 'offset') ?? 0;
  final titles = db.bookTitles();
  // The plain result page goes through the package's non-blocking API
  // (issue #16): the search runs on a worker isolate, hits and snippets come
  // back as Futures, texts are still read here. Grouping, exact counts and a
  // line regex keep the sync session.
  if (!const {'section', 'identical'}.contains(o['group']) &&
      !o.containsKey('count') &&
      !(request.userRegex && SeforimIndex.userRegexKind(request.query) == 'line')) {
    index.close();
    await _pageAsync(db, dir, o['native'], request, contextWords, titles, limit, offset, _int(o, 'pages') ?? 1,
        info.positions);
    return;
  }
  final sw = Stopwatch()..start();
  final session = index.search(request);
  final searchMs = sw.elapsedMilliseconds;
  final searchInfo = session.info();
  for (final n in searchInfo.notices) {
    stderr.writeln('notice: $n');
  }
  if (searchInfo.truncated) stderr.writeln('truncated: a walk hit its expansion cap');
  final lineRegex = session.lineRegex();
  if (lineRegex != null) {
    _lineRegex(db, session, lineRegex, request.books, titles, limit, offset, sw);
    session.close();
    index.close();
    return;
  }

  // The negative sameSection scope is the engine's (the index keeps each
  // line's section id): the hits come back already filtered.
  final hits = session.allHits().toList();

  // KitveiHakodesh (Core 5e1091b8): a hit's snippet is the window the check
  // located over the matched line alone; a SHORT window (fewer words than the
  // context) is shown with its same-book +-2 neighbour lines as PLAIN context,
  // never located again. One SQL statement fetches the context of every short
  // row on the page.
  List<String> renderAll(List<(PlacedHit, String, Snippet)> rows) {
    bool isShort(Snippet s) => s.isShort(contextWords) && s.breakAt == null;
    final context = db.snippetTexts(const [], [for (final r in rows) if (isShort(r.$3)) r.$1.lineId]);
    return [
      for (final (h, text, s) in rows)
        isShort(s) && context[h.lineId] != null
            ? _withContext(_render(text, s), context[h.lineId]!, s, contextWords)
            : _render(text, s)
    ];
  }

  final grouping = switch (o['group']) {
    'section' => Grouping.sameSection,
    'identical' => Grouping.identicalText,
    _ => null,
  };
  if (grouping != null) {
    _grouped(db, session, hits, request, o, grouping, titles, limit, offset, sw, searchMs, renderAll);
    index.close();
    return;
  }

  // The plain result pages: KitveiHakodesh's snippet model (_showPages).
  await _showPages(db, titles, info.positions, contextWords, limit, offset, _int(o, 'pages') ?? 1, session.hitCount,
      (from, count) async => session.hits(from, count),
      (from, count) async => session.wordWindows(from, count),
      (windows, lineTexts, nextTexts) async => session.finishWindows(windows, lineTexts, nextTexts),
      (first, batch, lineTexts, nextTexts) async => batch.any((h) => h.nextLineId != null)
          ? session.locatePairs(lineTexts, nextTexts)
          : session.locateLines([
              for (var i = 0; i < batch.length; i++) HitLines(first + i, [(batch[i].lineId, lineTexts[i])])
            ]), () async {
    final terms = session.takeNewTerms();
    stdout.writeln('Query: ${request.query}, matching lines: ${session.hitCount}'
        ', search ${searchMs}ms, '
        'page ${sw.elapsedMilliseconds}ms');
    stdout.writeln('Matched words: ${terms.take(30).join(', ')}${terms.length > 30 ? ' …' : ''}');
    if (o.containsKey('count')) _printCounts(db, session, titles);
  });
  session.close();
  index.close();
}

/// The result page through [SearchWorker] (issue #16): the same hits,
/// snippets and output as the sync path, with the engine on a worker isolate.
Future<void> _pageAsync(SeforimDb db, String dir, String? nativePath, SearchOptions request, int contextWords,
    Map<int, String> titles, int limit, int offset, int pages, bool positions) async {
  final worker = await SearchWorker.spawn(dir, nativePath: nativePath);
  final sw = Stopwatch()..start(); // as the sync path: the search, not the isolate start
  final cancel = SearchCancelToken();
  try {
    final session = await worker.search(request, cancel: cancel);
    final searchMs = sw.elapsedMilliseconds;
    try {
      final searchInfo = await session.info();
      for (final n in searchInfo.notices) {
        stderr.writeln('notice: $n');
      }
      if (searchInfo.truncated) stderr.writeln('truncated: a walk hit its expansion cap');
      await _showPages(db, titles, positions, contextWords, limit, offset, pages, session.hitCount,
          (from, count) => session.hits(from, count),
          (from, count) => session.wordWindows(from, count),
          (windows, lineTexts, nextTexts) => session.finishWindows(windows, lineTexts, nextTexts),
          (first, batch, lineTexts, nextTexts) => batch.any((h) => h.nextLineId != null)
              ? session.locatePairs(lineTexts, nextTexts)
              : session.locateLines([
                  for (var i = 0; i < batch.length; i++) HitLines(first + i, [(batch[i].lineId, lineTexts[i])])
                ]), () async {
        final terms = await session.takeNewTerms();
        stdout.writeln('Query: ${request.query}, matching lines: ${session.hitCount}'
            ', search ${searchMs}ms, page ${sw.elapsedMilliseconds}ms (worker isolate)');
        stdout.writeln('Matched words: ${terms.take(30).join(', ')}${terms.length > 30 ? ' …' : ''}');
      });
    } finally {
      await session.close();
    }
  } finally {
    await worker.close();
  }
}

/// The plain result pages, page by page, in KitveiHakodesh's snippet model
/// (Core SearchInBatches / BuildBatch, the service's ftsSnippetTexts, the
/// page's ensureSnippets):
///
/// - each hit's window is decided ONCE. Index without positions (FtsLib): the
///   search reads the texts of a batch of 256 hits, locates them, keeps the ones
///   that pass; index with positions: every hit passed during the search
///   (issue #21) with its window decided from the positions (word-level
///   descriptor, `wordWindows`), so no text is read during the search; a row
///   reached for display is finished with its text (`finishWindows`: context,
///   marks and offsets only, no window search; a cross-line pair, which has no
///   descriptor, is located from its two lines).
/// - the first [seedSnippetTextHitCount] passing hits' texts ride along (the
///   batch already read them); a later row's text is fetched when its page is
///   shown, at most [snippetFetchBatch] ids per request, [snippetLookAhead] rows
///   ahead / [snippetLookBehind] behind, a SHORT row's +-2 neighbour lines in the
///   same statement, cached ([SnippetTextCache]).
Future<void> _showPages(
  SeforimDb db,
  Map<int, String> titles,
  bool positions,
  int contextWords,
  int limit,
  int offset,
  int pages,
  int hitCount,
  Future<List<PlacedHit>> Function(int from, int count) hits,
  Future<List<WordWindow>> Function(int from, int count) windowsOf,
  Future<List<Snippet?>> Function(List<WordWindow> windows, List<String> lineTexts, List<String?> nextTexts) finish,
  Future<List<Snippet?>> Function(int first, List<PlacedHit> batch, List<String> lineTexts, List<String?> nextTexts)
      locate,
  Future<void> Function() header,
) async {
  final cache = SnippetTextCache((ids, shorts) => db.snippetTexts(ids, shorts));
  bool isShort(Snippet s) => s.isShort(contextWords) && s.breakAt == null;
  SnippetRow rowOf(PlacedHit h, Snippet s) => (lineId: h.lineId, short: isShort(s));
  List<SnippetRow> linesOf(PlacedHit h) =>
      [(lineId: h.lineId, short: false), if (h.nextLineId != null) (lineId: h.nextLineId!, short: false)];

  final rows = <(PlacedHit, Snippet)>[]; // passing hits from [offset] on, located
  var hitNo = 0, passed = 0;
  if (positions) hitNo = passed = offset < hitCount ? offset : hitCount; // every hit passes

  // Locates hits until [need] rows exist or the hits run out.
  Future<void> fill(int need) async {
    while (rows.length < need && hitNo < hitCount) {
      if (positions) {
        // the rows to show plus the lookahead: their texts in one request each 200
        final batch = await hits(hitNo, need - rows.length + snippetLookAhead);
        if (batch.isEmpty) break;
        final shown = batch.sublist(0, need - rows.length < batch.length ? need - rows.length : batch.length);
        await cache.ensure([for (final h in batch) ...linesOf(h)]);
        String textOf(int id) => cache.textOf((lineId: id, short: false))?.text ?? '';
        final windows = await windowsOf(hitNo, shown.length);
        final snippets = await finish(windows.length == shown.length ? windows : List.filled(shown.length, WordWindow.none),
            [for (final h in shown) textOf(h.lineId)],
            [for (final h in shown) h.nextLineId == null ? null : textOf(h.nextLineId!)]);
        hitNo += shown.length;
        for (var i = 0; i < shown.length; i++) {
          if (snippets[i] != null) rows.add((shown[i], snippets[i]!));
        }
        continue;
      }
      final batch = await hits(hitNo, 256);
      if (batch.isEmpty) break;
      final first = hitNo;
      hitNo += batch.length;
      final texts = db.lineTexts([...batch.map((h) => h.lineId), for (final h in batch) if (h.nextLineId != null) h.nextLineId!]);
      final lineTexts = [for (final h in batch) texts[h.lineId] ?? ''];
      final nextTexts = [for (final h in batch) h.nextLineId == null ? null : texts[h.nextLineId] ?? ''];
      final snippets = await locate(first, batch, lineTexts, nextTexts);
      final seedShort = <int>[];
      for (var i = 0; i < batch.length; i++) {
        final s = snippets[i];
        if (s == null) continue;
        // seed: the first passing hits keep the texts the batch read (Core SeedSnippetTexts)
        if (passed < seedSnippetTextHitCount) {
          if (isShort(s)) {
            seedShort.add(batch[i].lineId);
          } else {
            cache.seed([
              SnippetText(batch[i].lineId, lineTexts[i], '', ''),
              if (batch[i].nextLineId != null) SnippetText(batch[i].nextLineId!, nextTexts[i]!, '', ''),
            ]);
          }
        }
        if (passed++ >= offset) rows.add((batch[i], s));
      }
      // the seeded short rows' context: one statement for the batch
      if (seedShort.isNotEmpty) cache.seed(db.snippetTexts(const [], seedShort).values, withContext: true);
    }
  }

  await fill(limit);
  await header();
  for (var page = 0; page < pages; page++) {
    final from = page * limit;
    if (page > 0) await fill(from + limit);
    if (from >= rows.length) break;
    final count = from + limit > rows.length ? rows.length - from : limit;
    // the rows shown, with the lookahead, before any of them is printed
    final display = [for (final (h, s) in rows) rowOf(h, s)];
    await cache.ensureAround(display, from, count);
    await cache.ensure([for (final (h, _) in rows.sublist(from, from + count)) ...linesOf(h).skip(1)]);
    final shown = rows.sublist(from, from + count);
    final refs = db.heRefs(shown.map((r) => r.$1.lineId));
    if (pages > 1) stdout.writeln('-- page ${page + 1} (rows ${offset + from + 1}-${offset + from + count}) --');
    var n = offset + from;
    for (final (h, s) in shown) {
      final t = cache.textOf(rowOf(h, s)) ?? SnippetText(h.lineId, '', '', '');
      final text = s.breakAt == null
          ? t.text
          : '${t.text}\n${cache.textOf((lineId: h.nextLineId!, short: false))?.text ?? ''}';
      final window = _render(text, s);
      final row = isShort(s) ? _withContext(window, t, s, contextWords) : window;
      stdout.writeln('${++n}. ${titles[h.bookId] ?? h.bookId} | ${refs[h.lineId] ?? ''}  (line ${h.lineId})'
          '${_noteTag(db, h.lineId, h.bookId)}');
      stdout.writeln('   $row');
    }
  }
  stderr.writeln('snippet texts: ${cache.requests} on-demand requests (at most $snippetFetchBatch ids each)');
}

/// A user regex of Otzaria's Line kind: the engine matches each line's text.
/// Candidates: their texts are fetched by line id and fed in hit order. Full
/// scan: every line in scope is streamed in catalog order (book by book, as
/// Otzaria reads the books; positions as the build numbered them). Stops at
/// the engine's 100,000-line cap. Prints the first results with the matches
/// in [ ].
void _lineRegex(SeforimDb db, SearchSession session, LineRegexStatus status, List<int>? books,
    Map<int, String> titles, int limit, int offset, Stopwatch sw) {
  final shown = <(int, int, String, List<(int, int)>)>[];
  var found = 0;
  void take(List<PlacedHit> lines, List<String> texts, List<List<(int, int)>> ranges) {
    for (var i = 0; i < lines.length; i++) {
      if (ranges[i].isEmpty) continue;
      if (found++ >= offset && shown.length < limit) shown.add((lines[i].lineId, lines[i].bookId, texts[i], ranges[i]));
    }
  }

  if (!status.fullScan) {
    for (var o = 0; o < session.hitCount && !session.lineRegex()!.done; o += 2048) {
      final batch = session.hits(o, 2048);
      final texts = db.lineTexts(batch.map((h) => h.lineId));
      final lineTexts = [for (final h in batch) texts[h.lineId] ?? ''];
      take(batch, lineTexts, session.lineMatchBatch(lineTexts));
    }
  } else {
    final scope = books?.toSet();
    var position = 0;
    final lines = <PlacedHit>[], texts = <String>[];
    void flush() {
      take(lines, texts, session.lineScanBatch(lines, texts));
      lines.clear();
      texts.clear();
    }

    outer:
    for (final bookId in db.catalogBookOrder()) {
      if (scope != null && !scope.contains(bookId)) {
        // keep the build's numbering without reading the book
        position += db.bookLineCount(bookId);
        continue;
      }
      for (final line in db.bookLines(bookId)) {
        lines.add(PlacedHit(position++, line.id, bookId));
        texts.add(line.text);
        if (lines.length >= 4096) {
          flush();
          if (session.lineRegex()!.done) break outer;
        }
      }
    }
    if (lines.isNotEmpty) flush();
  }
  final end = session.lineRegex()!;
  stdout.writeln('Line regex: ${end.fullScan ? 'full scan' : '${end.scopeLines} candidate lines'}, '
      '${end.matched} matching lines${end.truncated ? ' (truncated at the cap)' : ''}, '
      '${sw.elapsedMilliseconds < 1000 ? '${sw.elapsedMilliseconds} ms' : '${(sw.elapsedMilliseconds / 60000).toStringAsFixed(2)} min'}');
  final refs = db.heRefs(shown.map((r) => r.$1));
  var n = offset;
  for (final (id, book, text, ranges) in shown) {
    final b = StringBuffer();
    var at = 0;
    for (final (s, e) in ranges) {
      b
        ..write(text.substring(at, s))
        ..write('[')
        ..write(text.substring(s, e))
        ..write(']');
      at = e;
    }
    b.write(text.substring(at));
    stdout.writeln('${++n}. ${titles[book] ?? book} | ${refs[id] ?? ''}  (line $id)${_noteTag(db, id, book)}');
    stdout.writeln('   ${b.toString().replaceAll(RegExp(r'<[^>]*>'), '')}');
  }
}

/// Exact totals (Otzaria's count / countByBook) from the engine: when no
/// text-level check applies the hit list is the answer, otherwise the texts
/// are fed in hit order (a hit's next line too, cross-line) and the engine
/// validates them.
void _printCounts(SeforimDb db, SearchSession session, Map<int, String> titles) {
  final sw = Stopwatch()..start();
  completeCounts(db, session);
  final c = session.counts();
  stdout.writeln('Exact count: ${c.total} lines in ${c.byBook.length} books '
      '(${session.countNeedsText ? 'texts validated' : 'no text needed'}, '
      '${sw.elapsedMilliseconds < 1000 ? '${sw.elapsedMilliseconds} ms' : '${(sw.elapsedMilliseconds / 60000).toStringAsFixed(2)} min'})');
  final top = c.byBook.entries.toList()..sort((a, b) => b.value.compareTo(a.value));
  for (final e in top.take(5)) {
    stdout.writeln('  ${titles[e.key] ?? e.key}: ${e.value}');
  }
}

/// Otzaria's grouped results (see lib/grouping.dart). Groups are formed over
/// the raw hits, as Otzaria's GroupCollector does. A card's representative is
/// its first member, in hit order, whose snippet passes the filters; a
/// group with none is not shown. Counts are raw members (Otzaria's "found X
/// results in range"); for identical text only the first --group-scan hits are
/// read, so counts may be a lower bound (shown with +), as Otzaria's truncated.
void _grouped(
  SeforimDb db,
  SearchSession session,
  List<PlacedHit> all,
  SearchOptions request,
  Map<String, String?> o,
  Grouping grouping,
  Map<int, String> titles,
  int limit,
  int offset,
  Stopwatch sw,
  int searchMs,
  List<String> Function(List<(PlacedHit, String, Snippet)>) renderAll,
) {
  var scanned = all;
  var truncated = false;
  final texts = <int, String>{};
  // The engine returns line ids only; sections come from the DB in one pass.
  final sections = grouping == Grouping.sameSection ? db.sectionsOf(scanned.map((h) => h.lineId)) : const <int, int>{};
  if (grouping == Grouping.identicalText) {
    final cap = _int(o, 'group-scan') ?? 20000;
    if (all.length > cap) {
      scanned = all.sublist(0, cap);
      truncated = true;
    }
    for (var i = 0; i < scanned.length; i += 2048) {
      final end = i + 2048 > scanned.length ? scanned.length : i + 2048;
      texts.addAll(db.lineTexts(scanned.sublist(i, end).map((h) => h.lineId)));
    }
  }
  final groups = groupInOrder<PlacedHit>(
    scanned,
    (h) => groupKey(grouping, tocEntryId: sections[h.lineId] ?? 0, lineId: h.lineId, text: texts[h.lineId]),
  );

  final cards = <(HitGroup<PlacedHit>, PlacedHit, String, Snippet)>[];
  var shown = 0;
  for (final g in groups) {
    if (cards.length >= limit) break;
    // the representative: first member whose snippet passes
    (PlacedHit, String, Snippet)? rep;
    for (var i = 0; i < g.members.length && rep == null; i += 64) {
      final part = g.members.sublist(i, i + 64 > g.members.length ? g.members.length : i + 64);
      final have = part.every((h) => texts.containsKey(h.lineId));
      final t = have ? texts : db.lineTexts(part.map((h) => h.lineId));
      final lineTexts = [for (final h in part) t[h.lineId] ?? ''];
      final snippets = session.locateBatch(lineTexts);
      for (var j = 0; j < part.length; j++) {
        final s = snippets[j];
        if (s != null) {
          rep = (part[j], lineTexts[j], s);
          break;
        }
      }
    }
    if (rep == null) continue;
    if (shown++ < offset) continue;
    cards.add((g, rep.$1, rep.$2, rep.$3));
  }

  final refs = db.heRefs([
    for (final c in cards) ...[
      c.$2.lineId,
      ...c.$1.members.where((m) => m != c.$2).take(mergedSiblingsCap).map((m) => m.lineId)
    ]
  ]);
  stdout.writeln('Query: ${request.query}, matching lines: ${session.hitCount}, '
      'groups: ${groups.length}${truncated ? '+' : ''} (${grouping.name}), search ${searchMs}ms, '
      'page ${sw.elapsedMilliseconds}ms');
  var n = offset;
  final rendered = renderAll([for (final c in cards) (c.$2, c.$3, c.$4)]);
  var k = 0;
  for (final (g, h, _, _) in cards) {
    final count = '${g.members.length}${truncated ? '+' : ''}';
    stdout.writeln('${++n}. ${titles[h.bookId] ?? h.bookId} | ${refs[h.lineId] ?? ''}  (line ${h.lineId})${_noteTag(db, h.lineId, h.bookId)} '
        '[$count results]');
    stdout.writeln('   ${rendered[k++]}');
    for (final m in g.members.where((m) => m != h).take(mergedSiblingsCap)) {
      stdout.writeln('     - ${titles[m.bookId] ?? m.bookId} | ${refs[m.lineId] ?? ''}  (line ${m.lineId})');
    }
  }
  session.close();
}
