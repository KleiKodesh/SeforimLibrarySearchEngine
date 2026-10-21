import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

/// Finds the native library: SEFORIM_SEARCH_NATIVE, else ../../target/release.
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

void main() {
  late Directory tmp;
  late SeforimIndex index;
  const text = 'והתורה ניתנה למשה';
  const text2 = 'בראשית ברא אלהים את השמים ואת הארץ';

  setUpAll(() {
    SeforimNative.load(_nativePath());
    tmp = Directory.systemTemp.createTempSync('se_dart_');
    index = SeforimIndex.create(tmp.path);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    w.add(const LineDocument(lineId: 7, position: 1, bookId: 1, text: text));
    w.add(const LineDocument(lineId: 8, position: 0, bookId: 2, text: text2));
    w.add(const LineDocument(lineId: 9, position: 2, bookId: 1, text: 'temp'));
    w.commit('done=1');
    w.delete(9);
    w.commit('done=2');
    w.close();
    w.close(); // idempotent
  });

  tearDownAll(() {
    index.close();
    index.close();
    tmp.deleteSync(recursive: true);
  });

  test('version and info', () {
    expect(SeforimIndex.nativeVersion, isNotEmpty);
    expect(SeforimIndex.exists(tmp.path), isTrue);
    final info = index.info();
    expect(info.numLines, 2);
    expect(info.payload, 'done=2');
    expect(info.positions, isTrue);
  });

  test('word with grammar prefixes, snippet offsets are Dart indices', () {
    final s = index.searchRequest(SearchRequest(groups: [
      const QueryGroup.word('תורה', WalkOptions(grammarPrefixes: true)),
    ]));
    expect(s.hitCount, 1);
    final hit = s.hits(0, 10).single;
    expect(hit.lineId, 7);
    expect(hit.bookId, 1);
    final snip = s.locate(text)!;
    expect(snip.marks, hasLength(1));
    final m = snip.marks.single;
    expect(text.substring(m.start, m.start + m.length), 'והתורה');
    expect(s.takeNewTerms(), ['והתורה']);
    s.close();
    s.close();
  });

  test('catalog order and AND of groups', () {
    final s = index.searchRequest(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')]));
    expect(s.allHits().map((h) => h.lineId).toList(), [8]);
    expect(s.hits(0, 1).single.bookId, 2);
    s.close();
    final inBook1 = index.searchRequest(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')], books: [1]));
    expect(inBook1.hitCount, 0);
    inBook1.close();
    final inBook2 = index.searchRequest(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')], books: [2, 5]));
    expect(inBook2.allHits().map((h) => h.lineId).toList(), [8]);
    inBook2.close();
    final noBooks = index.searchRequest(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')], books: []));
    expect(noBooks.hitCount, 0);
    noBooks.close();
    final none = index.searchRequest(SearchRequest(groups: [
      const QueryGroup.word('בראשית'),
      const QueryGroup.word('למשה'),
    ]));
    expect(none.hitCount, 0);
    none.close();
  });

  test('distance filter drops far matches in locate', () {
    final s = index.searchRequest(SearchRequest(
      groups: [const QueryGroup.word('בראשית'), const QueryGroup.word('הארץ')],
      maxWordDistance: 1,
    ));
    expect(s.hitCount, 1);
    expect(s.locateBatch([text2]).single, isNull);
    s.close();
  });

  test('exact counts: with and without a text-level filter', () {
    const bereshit = 'בראשית';
    const haaretz = 'הארץ';
    final far = index.searchRequest(SearchRequest(
      groups: [const QueryGroup.word(bereshit), const QueryGroup.word(haaretz)],
      maxWordDistance: 1,
    ));
    expect(far.countNeedsText, isTrue);
    expect(far.counts().exact, isFalse);
    expect(far.countBatch([text2]), 0);
    final c = far.counts();
    expect(c.exact, isTrue);
    expect(c.total, 0);
    expect(c.byBook, isEmpty);
    expect(() => far.countBatch([text2]), throwsA(isA<SeforimEngineException>()));
    far.close();
    final plain = index.searchRequest(SearchRequest(groups: [const QueryGroup.word(bereshit)]));
    expect(plain.countNeedsText, isFalse);
    final p = plain.counts();
    expect([p.total, p.validated, p.exact], [1, 1, true]);
    expect(p.byBook, {2: 1});
    plain.close();
  });

  test('cancel token stops the session and later searches', () {
    const bereshit = '\u05D1\u05E8\u05D0\u05E9\u05D9\u05EA';
    final token = SearchCancelToken();
    final s = index.search(SearchOptions(bereshit), cancel: token);
    expect(s.hitCount, 1);
    expect(s.locateBatch([text2]).single, isNotNull);
    token.cancel();
    expect(() => s.locateBatch([text2]), throwsA(isA<SearchCancelledException>()));
    expect(() => s.countBatch([text2]), throwsA(isA<SearchCancelledException>()));
    s.close();
    expect(() => index.search(SearchOptions(bereshit), cancel: token), throwsA(isA<SearchCancelledException>()));
    // another token from its address
    final other = SearchCancelToken();
    final s2 = index.search(SearchOptions(bereshit), cancel: other);
    SearchCancelToken.cancelAt(other.address);
    expect(() => s2.locateBatch([text2]), throwsA(isA<SearchCancelledException>()));
    s2.close();
  });

  test('any + fuzzy + a host-joined longer text', () {
    final s = index.searchRequest(SearchRequest(groups: [
      const QueryGroup.any([QueryGroup.word('משה'), QueryGroup.fuzzy('למשא', 1)]),
    ]));
    expect(s.hitCount, 1);
    final own = s.locate(text)!;
    final wide = s.locate('לפני $text אחרי');
    expect(wide, isNotNull);
    expect(wide!.windowWordCount, greaterThan(own.windowWordCount));
    s.close();
  });

  test('SearchOptions: query string, global options, per-word overrides', () {
    // global grammar prefixes reach והתורה
    var s = index.search(SearchOptions('תורה', grammarPrefixes: true));
    expect(s.hitCount, 1);
    expect(s.locate(text), isNotNull);
    s.close();
    // word 0 overrides the global option
    s = index.search(SearchOptions('תורה', grammarPrefixes: true, words: {0: WordOptions(grammarPrefixes: false)}));
    expect(s.hitCount, 0);
    s.close();
    // FtsLib syntax in the string; a per-word alternative joins the slot
    s = index.search(SearchOptions('ברא* ספר', words: {1: WordOptions(alternatives: ['הארץ'])}));
    expect(s.hits(0, 10).map((h) => h.lineId), [8]);
    s.close();
    s = index.search(SearchOptions('בראשית הארץ', maxWordDistance: 1));
    expect(s.locateBatch([text2]).single, isNull);
    s.close();
  });

  test('paired readings keep their second reading', () {
    expect(SeforimIndex.reducePairedReadings('א (בג) [דה] ו'), 'א  דה  ו');
    expect(SeforimIndex.reducePairedReadings('(א) ב'), '(א) ב');
  });

  test('errors surface as exceptions', () {
    expect(() => SeforimIndex.open('${tmp.path}/nope'), throwsA(isA<SeforimEngineException>()));
  });

  test('results order: catalog, relevance, generation', () {
    // line 7 holds the first word as typed (tier 0), line 8 the second with a prefix (tier 1)
    const q = 'ניתנה | ראשית';
    List<int> ids(SearchOptions o) {
      final s = index.search(o);
      final r = s.allHits().map((h) => h.lineId).toList();
      s.close();
      return r;
    }

    expect(ids(SearchOptions(q, grammarPrefixes: true)), [8, 7]);
    expect(ids(SearchOptions(q, grammarPrefixes: true, order: ResultsOrder.relevance)), [7, 8]);
    expect(
        ids(SearchOptions(q,
            grammarPrefixes: true, order: ResultsOrder.generation, bookGenerations: {1: 0, 2: 1})),
        [7, 8]);
    final json = SearchOptions(q, order: ResultsOrder.generation, bookGenerations: {2: 3}).toJson();
    expect(json['order'], 'generation');
    expect(json['book_generations'], [
      [2, 3]
    ]);
    expect(SearchOptions(q).toJson().containsKey('order'), isFalse);
  });
}
