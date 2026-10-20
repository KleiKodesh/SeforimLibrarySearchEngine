import 'dart:io';

import 'package:otzaria_demo/otzaria_query.dart';
import 'package:otzaria_demo/seforim_db.dart';
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
  --fuzzy-distance N  (fuzzy mode, 1..2)   --any-word  (wordMatchMode anyWord)
  --book TEXT         only books whose title/category contains TEXT
  --limit N (100) --offset N (0)  --context N (8)
  word alternatives: write a,b in the query (Otzaria sets them per word in the UI)
  library paths: --native path (seforim_search_native), --zstd path (libzstd)
''';

Map<String, String?> _parse(List<String> args, List<String> rest) {
  final m = <String, String?>{};
  const valued = {
    'db', 'index', 'max-lines', 'mode', 'prefix-max', 'suffix-max', 'partial-max', 'grammar-prefix-max', //
    'distance', 'fuzzy-distance', 'book', 'limit', 'offset', 'context', 'native', 'zstd', 'spacing',
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
            lineIndex: line.lineIndex,
            tocEntryId: line.tocEntryId,
            text: line.text));
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
  };
  final contextWords = _int(o, 'context') ?? 8;
  final req = OtzariaSearch(
    query: mainWords.join(' '),
    mode: SearchMode.values.byName(o['mode'] ?? 'advanced'),
    globalOptions: {for (final e in flagToOption.entries) if (o.containsKey(e.key)) e.value: true},
    alternativeWords: alts,
    distance: _int(o, 'distance') ?? 0,
    anyOrder: o.containsKey('any-order'),
    anyWord: o.containsKey('any-word'),
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
  final request = req.toRequest(contextWords: contextWords);
  req.warnings.toSet().forEach((w) => stderr.writeln('warning: $w'));

  final limit = _int(o, 'limit') ?? 100, offset = _int(o, 'offset') ?? 0;
  final bookFilter = o['book'] == null ? null : db.booksMatching(o['book']!);
  final titles = db.bookTitles();
  final sw = Stopwatch()..start();
  final session = index.search(request);
  final searchMs = sw.elapsedMilliseconds;

  // Page through hits in catalog order, locate snippets per batch, keep the
  // ones that pass the filters, until offset+limit results exist.
  final results = <(PlacedHit, String, Snippet)>[];
  var passed = 0;
  final hits = bookFilter == null ? session.allHits() : session.allHits().where((h) => bookFilter.contains(h.bookId));
  final it = hits.iterator;
  var more = true;
  while (more && results.length < limit) {
    final batch = <PlacedHit>[];
    while (batch.length < 256 && (more = it.moveNext())) {
      batch.add(it.current);
    }
    if (batch.isEmpty) break;
    final texts = db.lineTexts(batch.map((h) => h.lineId));
    final lineTexts = [for (final h in batch) texts[h.lineId] ?? ''];
    final snippets = session.locateBatch(lineTexts);
    for (var i = 0; i < batch.length && results.length < limit; i++) {
      var s = snippets[i];
      if (s == null) continue;
      if (passed++ < offset) continue;
      var text = lineTexts[i];
      if (s.isShort(contextWords)) {
        // Widen over up to 2 neighbouring lines per side (as the C# demo).
        final h = batch[i];
        final around = db.linesAround(h.bookId, h.lineIndex - 2, h.lineIndex + 2);
        String side(int a, int b) =>
            [for (var d = a; d <= b; d++) around[h.lineIndex + d] ?? ''].where((t) => t.isNotEmpty).join(' ');
        final prev = side(-2, -1), next = side(1, 2);
        final wide = session.locateWithNeighbours(prev, text, next, s);
        if (wide != null) {
          s = wide;
          text = [prev, text, next].where((t) => t.isNotEmpty).join(' ');
        }
      }
      results.add((batch[i], text, s));
    }
  }
  final refs = db.heRefs(results.map((r) => r.$1.lineId));
  final terms = session.takeNewTerms();

  stdout.writeln('Query groups: ${request.groups.length}, matching lines: ${session.hitCount}'
      '${bookFilter != null ? ' (before book filter)' : ''}, search ${searchMs}ms, '
      'page ${sw.elapsedMilliseconds}ms');
  stdout.writeln('Matched words: ${terms.take(30).join(', ')}${terms.length > 30 ? ' …' : ''}');
  var n = offset;
  for (final (h, text, s) in results) {
    stdout.writeln('${++n}. ${titles[h.bookId] ?? h.bookId} | ${refs[h.lineId] ?? ''}  (line ${h.lineId})');
    stdout.writeln('   ${_render(text, s)}');
  }
  session.close();
  index.close();
}
