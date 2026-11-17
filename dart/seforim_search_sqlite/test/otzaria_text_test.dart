// Otzaria's text / query helpers and book stamps against otzaria_search_engine 7.2.0's
// own output on the same inputs (goldens written by calling the old engine). The Rust
// side compares every helper exhaustively (hebrew-fts/tests/otzaria_helpers.rs); this
// checks the Dart API and the C ABI plumbing, the pure Dart stamps and the DB read of
// the library book fingerprint.
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_search.dart';
import 'package:seforim_search_sqlite/seforim_search_sqlite.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:test/test.dart';

String? _nativePath() {
  final env = Platform.environment['SEFORIM_SEARCH_NATIVE'];
  if (env != null) return env;
  var dir = Directory.current;
  for (var i = 0; i < 5; i++) {
    final f = File('${dir.path}/target/release/${SeforimNative.defaultFileName}');
    if (f.existsSync()) return f.path;
    dir = dir.parent;
  }
  return null;
}

Map<String, dynamic> _json(String path) => jsonDecode(File(path).readAsStringSync()) as Map<String, dynamic>;

String _hex(int v) => fingerprintHex(v);

List<Object> _matches(List<HighlightMatch> ms) => [
      for (final m in ms)
        [m.start, m.end, [for (final r in m.wordRanges) [r.start, r.end]]]
    ];

void main() {
  const shared = '../../hebrew-fts/tests/fixtures/otzaria_helpers';
  setUpAll(() => SeforimNative.load(_nativePath()));

  test('query helpers equal the old engine', () {
    final input = _json('$shared/inputs.json');
    final golden = _json('$shared/golden.json');
    final queries = (input['queries'] as List).cast<String>();
    for (var i = 0; i < queries.length; i++) {
      final q = queries[i];
      final g = golden['queries'][i] as Map<String, dynamic>;
      expect(sanitizeQuery(q), g['sanitize'], reason: 'sanitize $i');
      expect(splitQueryWords(q), g['split'], reason: 'split $i');
      expect([for (final s in queryWordSpans(q)) [s.word, s.start, s.end]], g['spans'], reason: 'spans $i');
      expect(generateLiteralHighlightPattern(q), g['literal'], reason: 'literal $i');
      final smart = generateSmartHighlightPattern(query: q, exact: true);
      expect(smart?.combinedPattern, (g['smartExact'] as Map?)?['combined'], reason: 'smart $i');
    }
  });

  test('highlight patterns and their matchers equal the old engine', () {
    final input = _json('$shared/inputs.json');
    final golden = _json('$shared/golden.json');
    final texts = (input['texts'] as List).cast<String>();
    final cases = (input['highlight'] as List).cast<Map<String, dynamic>>();
    for (var i = 0; i < cases.length; i++) {
      final c = cases[i];
      final g = golden['highlight'][i] as Map<String, dynamic>?;
      final p = generateHighlightPattern(
        query: c['query'] as String,
        distance: c['distance'] as int,
        customSpacing: (c['customSpacing'] as Map).cast<String, String>(),
        alternativeWords: {
          for (final e in (c['alternativeWords'] as Map).entries) int.parse(e.key as String): (e.value as List).cast<String>()
        },
        searchOptions: {
          for (final e in (c['searchOptions'] as Map).entries) e.key as String: (e.value as Map).cast<String, bool>()
        },
        anyOrder: c['anyOrder'] as bool,
      );
      if (g == null) {
        expect(p, isNull, reason: 'highlight $i');
        continue;
      }
      expect(p!.combinedPattern, g['combined'], reason: 'combined $i');
      expect(p.wordPatterns, g['words'], reason: 'words $i');
      expect(p.wordBoundaryEligible, g['eligible'], reason: 'eligible $i');
      expect(p.matcher.allowsCrossLine(), g['crossLine'], reason: 'crossLine $i');
      expect([for (final t in texts) _matches(p.matcher.findMatches(t, requireTokenBoundaries: p.wordBoundaryEligible))],
          g['find'],
          reason: 'find $i');
      expect([for (final t in texts) _matches(p.matcher.findWordMatches(t, requireTokenBoundaries: p.wordBoundaryEligible))],
          g['findWords'],
          reason: 'findWords $i');
    }
  });

  test('user regex plan and matcher equal the old engine', () {
    final input = _json('$shared/inputs.json');
    final golden = _json('$shared/golden.json');
    final texts = (input['texts'] as List).cast<String>();
    final regex = (input['regex'] as List).cast<String>();
    for (var i = 0; i < regex.length; i++) {
      final g = golden['regex'][i] as Map<String, dynamic>;
      final plan = parseUserRegex(regex[i]);
      expect(plan.kind.name, g['kind'], reason: 'kind $i');
      expect(plan.pattern, g['pattern'], reason: 'pattern $i');
      expect(plan.words, g['words'], reason: 'words $i');
      final m = prepareUserRegexMatcher(regex[i]);
      expect(m == null ? null : [for (final t in texts) _matches(m.findMatches(t))], g['find'], reason: 'find $i');
    }
  });

  test('stamps equal the old engine', () {
    final input = _json('$shared/inputs.json')['fingerprints'] as Map<String, dynamic>;
    final golden = _json('$shared/golden.json')['fingerprints'] as Map<String, dynamic>;
    final content = (input['content'] as List).cast<String>();
    expect([for (final t in content) _hex(contentFingerprint(t))], golden['content']);
    expect([for (final b in input['contentBytes'] as List) _hex(contentFingerprintBytes((b as List).cast<int>()))],
        golden['contentBytes']);
    expect([
      for (final m in (input['metadata'] as List).cast<Map<String, dynamic>>())
        _hex(bookMetadataFingerprint(m['title'] as String, m['topics'] as String, (m['facets'] as List?)?.cast<String>()))
    ], golden['metadata']);
    expect([
      for (final m in (input['book'] as List).cast<Map<String, dynamic>>())
        _hex(bookFingerprint(
            m['text'] as String, m['title'] as String, m['topics'] as String, (m['facets'] as List?)?.cast<String>()))
    ], golden['book']);
  });

  test('library book textHash equals the old engine (rows, BOM, data URIs, notes)', () {
    final spec = _json('test/fixtures/otzaria_books.json');
    final golden = _json('test/fixtures/otzaria_books_golden.json');
    final tmp = Directory.systemTemp.createTempSync('se_otzaria_books_');
    try {
      final path = '${tmp.path}/seforim.db';
      final w = sqlite3.open(path);
      w.execute('CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER, '
          'orderIndex INTEGER)');
      w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER)');
      w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT)');
      w.execute('CREATE TABLE line_note (lineId INTEGER, noteIndex INTEGER, charPos INTEGER, hasMarker INTEGER, body TEXT)');
      var lineId = 0;
      final books = (spec['books'] as List).cast<Map<String, dynamic>>();
      for (final b in books) {
        w.execute('INSERT INTO book VALUES (?, 1, ?, ?)', [b['id'], b['title'], b['id']]);
        final ids = <int>[];
        final rows = b['rows'] as List;
        for (var i = 0; i < rows.length; i++) {
          ids.add(++lineId);
          final r = rows[i] as String?;
          if (r == null) {
            w.execute('INSERT INTO line VALUES (?, ?, ?, NULL)', [lineId, b['id'], i]);
          } else {
            w.execute('INSERT INTO line VALUES (?, ?, ?, CAST(? AS TEXT))',
                [lineId, b['id'], i, Uint8List.fromList(base64.decode(r))]);
          }
        }
        for (final n in (b['notes'] as List).cast<List>()) {
          w.execute('INSERT INTO line_note VALUES (?, ?, ?, ?, ?)', [ids[n[0] as int], n[1], n[2], n[3], n[4]]);
        }
      }
      w.close();
      final db = SeforimDb.open(path);
      try {
        String? fp(int id, [String? title]) {
          final v = db.bookContentFingerprint(id, dbTitle: title);
          return v == null ? null : _hex(v);
        }

        expect([for (final b in books) fp(b['id'] as int)], golden['books']);
        expect([for (final c in (spec['titleChecks'] as List).cast<List>()) fp(c[0] as int, c[1] as String)],
            golden['titleChecks']);
      } finally {
        db.close();
      }
    } finally {
      tmp.deleteSync(recursive: true);
    }
  });
}
