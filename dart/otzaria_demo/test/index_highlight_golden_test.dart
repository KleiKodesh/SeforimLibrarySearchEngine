// Golden test: Otzaria's index highlight patterns (generateIndexHighlightPattern /
// Fuzzy / Smart) on this engine's matched terms against otzaria_search_engine 7.2.0's
// output over the same lines. `fixtures/index_highlight/golden.json` was written by the
// app fork's `tool/seforim_index_goldens.dart` (the old engine's own small index over
// `inputs.json`'s texts); here the same texts are indexed by this engine and each
// word's terms come from its search (SearchSession.groupTerms).
import 'dart:convert';
import 'dart:io';

import 'package:otzaria_demo/otzaria_query.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

String _nativePath() {
  final env = Platform.environment['SEFORIM_SEARCH_NATIVE'];
  if (env != null && env.isNotEmpty) return env;
  final name = Platform.isWindows
      ? 'seforim_search_native.dll'
      : Platform.isMacOS
          ? 'libseforim_search_native.dylib'
          : 'libseforim_search_native.so';
  return '../../target/release/$name';
}

Object? _matches(List<HighlightMatch> ms) => [
      for (final m in ms)
        [m.start, m.end, [for (final r in m.wordRanges) [r.start, r.end]]]
    ];

Object? _pattern(HighlightPattern? p, List<String> texts) {
  if (p == null) return null;
  return {
    'combined': p.combinedPattern,
    'words': p.wordPatterns,
    'eligible': p.wordBoundaryEligible,
    'crossLine': p.matcher.allowsCrossLine(),
    'find': [for (final t in texts) _matches(p.matcher.findMatches(t, requireTokenBoundaries: p.wordBoundaryEligible))],
    'findWords': [
      for (final t in texts) _matches(p.matcher.findWordMatches(t, requireTokenBoundaries: p.wordBoundaryEligible))
    ],
  };
}

Map<int, Set<String>> _wordOptions(Map<String, dynamic> m) => {
      for (final e in m.entries)
        if (int.tryParse(e.key.substring(e.key.lastIndexOf('_') + 1)) case final i?)
          i: {for (final o in (e.value as Map).entries) if (o.value == true) o.key as String}
    };

Map<int, int>? _spacing(Map<String, dynamic> m) => m.isEmpty
    ? null
    : {
        for (final e in m.entries)
          if ((int.tryParse(e.key.split('-').first), int.tryParse((e.value as String).trim())) case (final i?, final v?))
            i: v < 0 ? 0 : v
      };

final _dump = <String, Object?>{};

void _record(String key, Object? value) {
  if (Platform.environment['INDEX_HIGHLIGHT_DUMP'] != null) _dump[key] = value;
}

// Optimal string alignment distance (a swap of two adjacent letters is one edit).
int _osa(String a, String b) {
  final x = a.runes.toList(), y = b.runes.toList();
  final d = List.generate(x.length + 1, (i) => List.filled(y.length + 1, 0));
  for (var i = 0; i <= x.length; i++) {
    d[i][0] = i;
  }
  for (var j = 0; j <= y.length; j++) {
    d[0][j] = j;
  }
  for (var i = 1; i <= x.length; i++) {
    for (var j = 1; j <= y.length; j++) {
      final cost = x[i - 1] == y[j - 1] ? 0 : 1;
      var v = [d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + cost].reduce((p, q) => p < q ? p : q);
      if (i > 1 && j > 1 && x[i - 1] == y[j - 2] && x[i - 2] == y[j - 1] && d[i - 2][j - 2] + 1 < v) v = d[i - 2][j - 2] + 1;
      d[i][j] = v;
    }
  }
  return d[x.length][y.length];
}

// Otzaria's quote-transparent fuzzy rule: the term as written or without its quotes.
bool _otzariaFuzzyMatch(String word, String term, int d) =>
    _osa(word, term) <= d || _osa(word, term.replaceAll(RegExp(r'''['"]'''), '')) <= d;

// The divine-name placeholder spelling with U+05E7 (issue #39) of a query without it.
bool _placeholderSpelling(String term, String query) => term.contains('\u05E7') && !query.contains('\u05E7');

void main() {
  late Directory tmp;
  late SeforimIndex index;
  // Otzaria's nikud / te'amim options read its vocalized field: here an index that
  // keeps the marks (the app's keep-marks index).
  late SeforimIndex marksIndex;
  final base = 'test/fixtures/index_highlight';
  final input = jsonDecode(File('$base/inputs.json').readAsStringSync()) as Map<String, dynamic>;
  final golden = jsonDecode(File('$base/golden.json').readAsStringSync()) as Map<String, dynamic>;
  final texts = (input['texts'] as List).cast<String>();
  final queries = (input['queries'] as List).cast<String>();

  setUpAll(() {
    SeforimNative.load(_nativePath());
    tmp = Directory.systemTemp.createTempSync('se_index_highlight_');
    SeforimIndex build(String dir, IndexOptions options) {
      final created = SeforimIndex.create(dir, options);
      final w = created.openWriter(heapBytes: 30000000, threads: 1);
      for (var i = 0; i < texts.length; i++) {
        w.add(LineDocument(lineId: i + 1, position: i, bookId: 1, text: texts[i]));
      }
      w.commit();
      w.close();
      created.close();
      return SeforimIndex.open(dir);
    }

    index = build('${tmp.path}/plain', const IndexOptions());
    marksIndex = build('${tmp.path}/marks', const IndexOptions(keepMarks: true));
  });

  tearDownAll(() {
    final dump = Platform.environment['INDEX_HIGHLIGHT_DUMP'];
    if (dump != null) {
      File(dump).writeAsStringSync(jsonEncode(_dump).replaceAllMapped(
          RegExp(r'[^\x00-\x7f]'), (m) => '\\u${m[0]!.codeUnitAt(0).toRadixString(16).padLeft(4, '0')}'));
    }
    index.close();
    marksIndex.close();
    try {
      tmp.deleteSync(recursive: true);
    } catch (_) {}
  });

  List<List<String>> groupTerms(OtzariaSearch s, {bool marks = false}) {
    final session = (marks ? marksIndex : index).search(s.toOptions());
    try {
      return session.groupTerms();
    } finally {
      session.close();
    }
  }

  // a word's fuzzy terms: this engine's fuzzy walk of the word alone (none at 0)
  List<String> fuzzyTerms(String word, int d) {
    if (d == 0) return const [];
    final g = groupTerms(OtzariaSearch(query: word, mode: SearchMode.fuzzy, fuzzyDistance: d));
    // A named deviation: this engine's fuzzy mode measures a word that itself holds a
    // quote on its quote-free form; Otzaria's automaton keeps the word's quote (a term
    // matches as written or without its quotes). Its terms are a superset; the pattern
    // is compared on the old engine's own set.
    return g.isEmpty ? const [] : [for (final t in g.first) if (_otzariaFuzzyMatch(word, t, d)) t];
  }

  final cases = (input['highlight'] as List).cast<Map<String, dynamic>>();
  for (var i = 0; i < cases.length; i++) {
    test('index highlight case $i', () {
      final c = cases[i];
      final query = c['query'] as String;
      final alternatives = {
        for (final e in (c['alternativeWords'] as Map).entries) int.parse(e.key as String): (e.value as List).cast<String>()
      };
      final options = (c['searchOptions'] as Map).cast<String, dynamic>();
      final marks = _wordOptions(options).values.any((o) => o.contains(OtzariaOption.nikud) || o.contains(OtzariaOption.teamim));
      final searched = groupTerms(
          OtzariaSearch(
              query: query,
              mode: SearchMode.advanced,
              wordOptions: _wordOptions(options),
              alternativeWords: alternatives,
              distance: c['distance'] as int,
              anyOrder: c['anyOrder'] as bool,
              marksIndex: marks,
              customSpacing: _spacing((c['customSpacing'] as Map).cast<String, dynamic>())),
          marks: marks);
      // A named deviation: this engine always searches a divine name's placeholder
      // spellings too (issue #39), so its matched terms hold them where the old
      // engine's scan did not; the pattern is compared on the old engine's terms.
      final terms = [
        for (final g in searched) [for (final t in g) if (!_placeholderSpelling(t, query)) t]
      ];
      final p = generateIndexHighlightPattern(
          query: query,
          distance: c['distance'] as int,
          perWordTerms: terms,
          customSpacing: (c['customSpacing'] as Map).cast<String, String>(),
          alternativeWords: alternatives,
          searchOptions: {for (final e in options.entries) e.key: (e.value as Map).cast<String, bool>()},
          anyOrder: c['anyOrder'] as bool);
      _record('h$i', {'terms': terms, 'out': _pattern(p, texts)});
      expect(jsonEncode(_pattern(p, texts)), jsonEncode((golden['highlight'] as List)[i]));
    });
  }

  for (var i = 0; i < queries.length; i++) {
    test('index fuzzy + smart query $i', () {
      final q = queries[i];
      for (final d in [0, 1, 2]) {
        final tokens = fuzzyIndexHighlightTokens(q);
        final p = generateIndexFuzzyHighlightPattern(
            query: q, perTokenTerms: [for (final t in tokens ?? const <String>[]) fuzzyTerms(t, d)]);
        _record('f$i/$d', {'tokens': tokens, 'terms': [for (final t in tokens ?? const <String>[]) fuzzyTerms(t, d)], 'out': _pattern(p, texts)});
        expect(jsonEncode(_pattern(p, texts)), jsonEncode(((golden['fuzzy'] as List)[i] as List)[d]), reason: 'fuzzy d=$d');
      }
      final smart = (golden['smart'] as List)[i] as Map<String, dynamic>;
      expect(smartIndexHighlightWords(q, exact: true), isEmpty);
      expect(jsonEncode(_pattern(generateIndexSmartHighlightPattern(query: q, exact: true), texts)),
          jsonEncode(smart['exact']),
          reason: 'smart exact');
      for (final d in [0, 1, 2]) {
        final words = smartIndexHighlightWords(q, exact: false);
        final p = generateIndexSmartHighlightPattern(
            query: q, exact: false, terms: {for (final w in words) w: fuzzyTerms(w, d)});
        expect(jsonEncode(_pattern(p, texts)), jsonEncode((smart['fuzzy'] as List)[d]), reason: 'smart fuzzy d=$d');
      }
    });
  }
}
