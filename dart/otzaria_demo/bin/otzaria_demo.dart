import 'dart:io';

import 'package:otzaria_demo/otzaria_query.dart';
import 'package:otzaria_demo/seforim_db.dart';
import 'package:otzaria_demo/acronym_lexicon.dart';
import 'package:otzaria_demo/grouping.dart';
import 'package:otzaria_demo/translation_lexicon.dart';
import 'package:seforim_search/seforim_search.dart';

const _usage = '''
otzaria_demo build  [--db path] [--index dir] [--max-lines N] [--keep-marks] [--no-positions]
otzaria_demo search [--db path] [--index dir] [options] <query>

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
  --limit N (100) --offset N (0)  --context N (8)
  --count             exact totals: all lines passing every filter, and per book
  --order catalog|relevance  results order (Otzaria's sortBy; default catalog)
  word alternatives: write a,b in the query (Otzaria sets them per word in the UI)
  library paths: --native path (seforim_search_native), --zstd path (libzstd)
''';

Map<String, String?> _parse(List<String> args, List<String> rest) {
  final m = <String, String?>{};
  const valued = {
    'db', 'index', 'max-lines', 'mode', 'prefix-max', 'suffix-max', 'partial-max', 'grammar-prefix-max', //
    'distance', 'fuzzy-distance', 'book', 'limit', 'offset', 'context', 'native', 'zstd', 'spacing', 'match',
    'match-count', 'exclude', 'exclude-scope', 'exclude-options', 'dictionary', 'acronyms', 'group', 'group-scan',
    'order', 'scope',
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

void main(List<String> args) {
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
      case 'search':
        _search(db, indexDir, o, rest.join(' '));
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
  final sw = Stopwatch()..start();
  stdout.writeln('DB: ${db.path} (split=${db.splitContent}, zstd=${db.compressed})');
  final index = SeforimIndex.create(
      dir, IndexOptions(positions: !o.containsKey('no-positions'), keepMarks: o.containsKey('keep-marks')));
  final w = index.openWriter(heapBytes: 200000000);
  var position = 0;
  try {
    outer:
    for (final bookId in db.catalogBookOrder()) {
      for (final line in db.bookLines(bookId)) {
        if (maxLines > 0 && position >= maxLines) break outer;
        w.add(LineDocument(
            lineId: line.id,
            position: position++,
            bookId: line.bookId,
            text: line.text,
            // the section (TOC entry; 0 = none: a section of its own) and
            // Otzaria's rule: a heading interrupts the text (no cross-line phrase)
            sectionId: line.tocEntryId == 0 ? null : line.tocEntryId,
            noCross: line.text.startsWith('<h')));
        if (position % 100000 == 0) stdout.writeln('  $position lines, ${sw.elapsed.inSeconds}s');
      }
    }
    w.commit('{"lines":$position,"db":"${db.fingerprint()}"}');
  } catch (_) {
    w.rollback();
    rethrow;
  } finally {
    w.close();
  }
  // One segment (the walks and their caps run per segment), then the
  // FM-index sidecar for leading-* wildcards and fuzzy words.
  index.openWriter(heapBytes: 200000000).mergeAndClose();
  index.buildFmSidecar();
  stdout.writeln('Indexed $position lines into $dir in ${sw.elapsed.inMilliseconds} ms; '
      'payload ${index.info().payload}');
  index.close();
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

void _search(SeforimDb db, String dir, Map<String, String?> o, String query) {
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
  req.warnings.toSet().forEach((w) => stderr.writeln('warning: $w'));
  request.crossLine = o.containsKey('cross-line');
  request.sameSection = o['scope'] == 'section';

  final limit = _int(o, 'limit') ?? 100, offset = _int(o, 'offset') ?? 0;
  final titles = db.bookTitles();
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

  (String, Snippet) widen(PlacedHit h, String text, Snippet s) {
    if (!s.isShort(contextWords) || s.breakAt != null) return (text, s);
    // Widen over up to 2 neighbouring lines per side (as the C# demo):
    // previous + ' ' + line + ' ' + next, empty sides dropped (FtsLib's
    // JoinWithNeighbourLines); the engine just locates the longer text. The
    // demo shows the joined text's snippet as is; KitveiHakodesh's searcher
    // (C# demo) keeps the matched line's score and distance.
    // the engine returns line ids only: lineIndex comes from SQLite
    final lineIndex = db.lineIndexesOf([h.lineId])[h.lineId];
    if (lineIndex == null) return (text, s);
    final around = db.lineIdsAround(h.bookId, lineIndex - 2, lineIndex + 2);
    // the lines go to the engine one by one (id + text); it joins them by
    // single spaces and, on an index with positions, reads the matched words
    // from the postings
    final lines = <(int, String)>[
      for (var d = -2; d <= 2; d++)
        if (d == 0) (h.lineId, text) else if (around[lineIndex + d] case final l? when l.$2.isNotEmpty) l
    ];
    final joined = [for (final l in lines) if (l.$2.isNotEmpty) l.$2].join(' ');
    final wide = session.locateLines([HitLines(-1, lines)])[0];
    return wide == null ? (text, s) : (joined, wide);
  }

  final grouping = switch (o['group']) {
    'section' => Grouping.sameSection,
    'identical' => Grouping.identicalText,
    _ => null,
  };
  if (grouping != null) {
    _grouped(db, session, hits, request, o, grouping, titles, limit, offset, sw, searchMs, widen);
    index.close();
    return;
  }

  // Page through hits in hit order (--order), locate snippets per batch, keep the
  // ones that pass the filters, until offset+limit results exist.
  final results = <(PlacedHit, String, Snippet)>[];
  var passed = 0;
  final it = hits.iterator;
  var hitNo = 0; // the next hit's index in the search's hits
  var more = true;
  while (more && results.length < limit) {
    final batch = <PlacedHit>[];
    final batchStart = hitNo;
    while (batch.length < 256 && (more = it.moveNext())) {
      batch.add(it.current);
      hitNo++;
    }
    if (batch.isEmpty) break;
    final texts = db.lineTexts([...batch.map((h) => h.lineId), for (final h in batch) if (h.nextLineId != null) h.nextLineId!]);
    final lineTexts = [for (final h in batch) texts[h.lineId] ?? ''];
    final nextTexts = [for (final h in batch) h.nextLineId == null ? null : texts[h.nextLineId] ?? ''];
    // a cross-line hit is located over its line and the next one (joined at a
    // line break); otherwise the hits go as their lines (positions index: the
    // matched words come from the postings)
    final snippets = batch.any((h) => h.nextLineId != null)
        ? session.locatePairs(lineTexts, nextTexts)
        : session.locateLines([
            for (var i = 0; i < batch.length; i++) HitLines(batchStart + i, [(batch[i].lineId, lineTexts[i])])
          ]);
    for (var i = 0; i < batch.length && results.length < limit; i++) {
      final s = snippets[i];
      if (s == null) continue;
      if (passed++ < offset) continue;
      final joined = s.breakAt == null ? lineTexts[i] : '${lineTexts[i]}\n${nextTexts[i]}';
      final (text, shown) = widen(batch[i], joined, s);
      results.add((batch[i], text, shown));
    }
  }
  final refs = db.heRefs(results.map((r) => r.$1.lineId));
  final terms = session.takeNewTerms();

  stdout.writeln('Query: ${request.query}, matching lines: ${session.hitCount}'
      ', search ${searchMs}ms, '
      'page ${sw.elapsedMilliseconds}ms');
  stdout.writeln('Matched words: ${terms.take(30).join(', ')}${terms.length > 30 ? ' …' : ''}');
  if (o.containsKey('count')) _printCounts(db, session, titles);
  var n = offset;
  for (final (h, text, s) in results) {
    stdout.writeln('${++n}. ${titles[h.bookId] ?? h.bookId} | ${refs[h.lineId] ?? ''}  (line ${h.lineId})');
    stdout.writeln('   ${_render(text, s)}');
  }
  session.close();
  index.close();
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
        position += db.db.select('SELECT COUNT(*) FROM line WHERE bookId = ?', [bookId]).first.columnAt(0) as int;
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
    stdout.writeln('${++n}. ${titles[book] ?? book} | ${refs[id] ?? ''}  (line $id)');
    stdout.writeln('   ${b.toString().replaceAll(RegExp(r'<[^>]*>'), '')}');
  }
}

/// Exact totals (Otzaria's count / countByBook) from the engine: when no
/// text-level check applies the hit list is the answer, otherwise the texts
/// are fed in hit order (a hit's next line too, cross-line) and the engine
/// validates them.
void _printCounts(SeforimDb db, SearchSession session, Map<int, String> titles) {
  final sw = Stopwatch()..start();
  if (session.countNeedsText) {
    for (var o = 0; o < session.hitCount; o += 4096) {
      final batch = session.hits(o, 4096);
      final texts = db.lineTexts([...batch.map((h) => h.lineId), for (final h in batch) if (h.nextLineId != null) h.nextLineId!]);
      session.countBatchPairs([for (final h in batch) texts[h.lineId] ?? ''],
          [for (final h in batch) h.nextLineId == null ? null : texts[h.nextLineId] ?? '']);
    }
  }
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
  (String, Snippet) Function(PlacedHit, String, Snippet) widen,
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
          final (text, sn) = widen(part[j], lineTexts[j], s);
          rep = (part[j], text, sn);
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
  for (final (g, h, text, s) in cards) {
    final count = '${g.members.length}${truncated ? '+' : ''}';
    stdout.writeln('${++n}. ${titles[h.bookId] ?? h.bookId} | ${refs[h.lineId] ?? ''}  (line ${h.lineId}) '
        '[$count results]');
    stdout.writeln('   ${_render(text, s)}');
    for (final m in g.members.where((m) => m != h).take(mergedSiblingsCap)) {
      stdout.writeln('     - ${titles[m.bookId] ?? m.bookId} | ${refs[m.lineId] ?? ''}  (line ${m.lineId})');
    }
  }
  session.close();
}
