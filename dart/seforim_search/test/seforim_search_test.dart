import 'dart:ffi';
import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:seforim_search/src/native.dart' show SeBuffer;
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

  test('matchMode allMatches: whole text, every occurrence (issues #14 / #23)', () {
    final word = text.split(' ').first;
    final s = index.searchRequest(SearchRequest(groups: [QueryGroup.word(word)], contextWords: 0));
    final line = '$word x x x x <b>$word</b> y $word';
    final best = s.locateBatch([line])[0]!;
    expect(s.matchMode, SnippetMatchMode.bestWindow);
    s.matchMode = SnippetMatchMode.allMatches;
    final all = s.locateBatch([line])[0]!;
    expect(all.start, 0);
    expect(all.end, line.length);
    expect(all.marks.map((m) => m.start).toList(), [0, 18, 31]);
    s.matchMode = SnippetMatchMode.bestWindow;
    final again = s.locateBatch([line])[0]!;
    expect([again.start, again.end, again.marks.length], [best.start, best.end, best.marks.length]);
    s.close();
  });

  test('findAllMatches: words, phrase, marks, quotes, UTF-16 (issues #14 / #23)', () {
    const a = 'אב', b = 'גד';
    final t = '$a $b $a <b>$b</b> $a';
    expect(findAllMatches(t, [a]).map((m) => m.start).toList(), [0, 6, 19]);
    expect(findAllMatches(t, [a, b]).map((m) => m.term).toList(), [0, 1, 0, 1, 0]);
    expect(findAllMatches(t, ['$a $b'], phrase: true).map((m) => m.start).toList(), [0, 3, 6, 12]);
    expect(findAllMatches('$a $a $a', [a, a], phrase: true).length, 3);
    expect(findAllMatches('אָב x', [a]), [const TextMatch(0, 3, 0)]);
    expect(findAllMatches('$a אָב', ['אָב'], matchMarks: true).map((m) => m.start).toList(), [3]);
    const q = 'אב"ג', plain = 'אבג';
    expect(findAllMatches('x $q', [plain]), [const TextMatch(2, 4, 0)]); // quotes read as the walks do
    expect(findAllMatches('x $plain', [q]), isEmpty);
    expect(findAllMatches('x $q', [plain], ignoreQuotes: true), [const TextMatch(2, 4, 0)]);
    expect(findAllMatches('\u{1F600} $a', [a]).single.start, 3);
    expect(findAllMatches('', [a]), isEmpty);
  });

  test('locateLines: same snippets as locateBatch on the joined lines', () {
    final word = text.split(' ').first;
    final a = index.searchRequest(SearchRequest(groups: [QueryGroup.word(word)]));
    final b = index.searchRequest(SearchRequest(groups: [QueryGroup.word(word)]));
    String k(List<Snippet?> l) => [
          for (final s in l)
            s == null
                ? '-'
                : '${s.score} ${s.wordDistance} ${s.windowWordCount} ${s.start} ${s.end} ${s.flags} '
                    '${s.marks.map((m) => '${m.start}/${m.length}/${m.termId}').join(';')}'
        ].join('|');
    final hit = a.hits(0, 1).single;
    expect(k(b.locateLines([HitLines(0, [(hit.lineId, text)])])), k(a.locateBatch([text])));
    // a short window widened over a neighbour line (looked up by id), joined by a space
    expect(k(b.locateLines([HitLines(-1, [(8, text2), (7, text), (99, '')])])), k(a.locateBatch(['$text2 $text'])));
    expect(b.takeNewTerms(), a.takeNewTerms());
    a.close();
    b.close();
  });

  test('catalog order and AND of groups', () {
    final s = index.searchRequest(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')]));
    expect(s.allHits().map((h) => h.lineId).toList(), [8]);
    expect(s.hits(0, 1).single.bookId, 2);
    // paging edges: a non-positive page size is rejected before iteration,
    // negative offsets / counts throw, past-the-end and empty pages are empty
    expect(() => s.allHits(pageSize: 0), throwsArgumentError);
    expect(() => s.allHits(pageSize: -1), throwsArgumentError);
    expect(s.allHits(pageSize: 1).map((h) => h.lineId).toList(), [8]);
    expect(() => s.hits(-1, 1), throwsArgumentError);
    expect(() => s.hits(0, -1), throwsArgumentError);
    expect(s.hits(0, 0), isEmpty);
    expect(s.hits(1, 5), isEmpty);
    expect(s.hits(0, 1 << 40).length, 1);
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
    // an index with positions drops the far match during the search (issue #21)
    expect(s.hitCount, 0);
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
    // an index with positions decides the hits during the search (issue
    // #21): the far match is gone and the counts are exact at once
    expect(far.countNeedsText, isFalse);
    expect(far.hitCount, 0);
    final c = far.counts();
    expect(c.exact, isTrue);
    expect(c.total, 0);
    expect(c.byBook, isEmpty);
    expect(() => far.countBatch([text2]), throwsA(isA<SeforimEngineException>()));
    far.close();
    // without positions the text decides it (FtsLib)
    final dir = Directory.systemTemp.createTempSync('se_dart_flat_counts_');
    final flat = SeforimIndex.create(dir.path, const IndexOptions(positions: false));
    final w = flat.openWriter(heapBytes: 30000000, threads: 1);
    w.add(LineDocument(lineId: 2, position: 0, bookId: 2, text: text2));
    w.commit();
    w.close();
    final ff = flat.searchRequest(SearchRequest(
      groups: [const QueryGroup.word(bereshit), const QueryGroup.word(haaretz)],
      maxWordDistance: 1,
    ));
    expect(ff.countNeedsText, isTrue);
    expect(ff.counts().exact, isFalse);
    expect(ff.countBatch([text2]), 0);
    expect(ff.counts().exact, isTrue);
    expect(ff.counts().total, 0);
    ff.close();
    flat.close();
    dir.deleteSync(recursive: true);
    final plain = index.searchRequest(SearchRequest(groups: [const QueryGroup.word(bereshit)]));
    expect(plain.countNeedsText, isFalse);
    final p = plain.counts();
    expect([p.total, p.validated, p.exact], [1, 1, true]);
    expect(p.byBook, {2: 1});
    plain.close();
  });

  test('second writer: lock error on the same and on another handle (issue #31)', () {
    final dir = Directory.systemTemp.createTempSync('se_dart_lock_');
    final one = SeforimIndex.create(dir.path);
    final other = SeforimIndex.open(dir.path);
    final first = one.openWriter(heapBytes: 30000000, threads: 1);
    for (final handle in [one, other]) {
      expect(
          () => handle.openWriter(heapBytes: 30000000, threads: 1),
          throwsA(isA<SeforimEngineException>()
              .having((e) => e.toString(), 'message', contains('LockBusy'))));
    }
    first.add(const LineDocument(lineId: 1, position: 0, bookId: 1, text: 'alpha'));
    first.commit();
    first.close();
    other.openWriter(heapBytes: 30000000, threads: 1).close();
    other.close();
    one.close();
    dir.deleteSync(recursive: true);
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

  test('SearchOptions: a negative word has its own options', () {
    const nitna = 'ניתנה';
    const torah = 'תורה';
    int hits(SearchOptions o) {
      final s = index.search(o);
      final n = s.hitCount;
      s.close();
      return n;
    }

    // the global options are the negative word's default...
    expect(hits(SearchOptions(nitna, exclude: ExcludeOptions(torah))), 1);
    expect(hits(SearchOptions(nitna, grammarPrefixes: true, exclude: ExcludeOptions(torah))), 0);
    // ...and its own override wins, either way
    expect(hits(SearchOptions(nitna, exclude: ExcludeOptions(torah, words: {0: WordOptions(grammarPrefixes: true)}))), 0);
    expect(
        hits(SearchOptions(nitna,
            grammarPrefixes: true, exclude: ExcludeOptions(torah, words: {0: WordOptions(grammarPrefixes: false)}))),
        1);
    // its alternatives join its slot
    expect(
        hits(SearchOptions(nitna,
            exclude: ExcludeOptions('ספר', words: {0: WordOptions(alternatives: ['למשה'])}))),
        0);
    // a phrase alternative of the negative query (positions: in the query)
    expect(
        hits(SearchOptions(nitna,
            exclude: ExcludeOptions('', phraseAlternatives: [const PhraseOptions('$nitna למשה')]))),
        0);
  });

  test('paired readings keep their second reading', () {
    expect(SeforimIndex.reducePairedReadings('א (בג) [דה] ו'), 'א  דה  ו');
    expect(SeforimIndex.reducePairedReadings('(א) ב'), '(א) ב');
  });

  test('ABI edges: embedded NUL, id domain, SeBuffer layout', () {
    // ptr + length output: an embedded NUL survives
    expect(SeforimIndex.reducePairedReadings('a b'), 'a b');
    expect(SeforimIndex.reducePairedReadings(''), '');
    // ids are 0..=i64::MAX: a negative Dart int is a u64 above it, rejected
    final dir = Directory.systemTemp.createTempSync('se_dart_ids_');
    final i = SeforimIndex.create(dir.path);
    final w = i.openWriter(heapBytes: 30000000, threads: 1);
    expect(() => w.add(const LineDocument(lineId: -1, position: 0, bookId: 1, text: 'x')),
        throwsA(isA<SeforimEngineException>()));
    expect(() => w.delete(-1), throwsA(isA<SeforimEngineException>()));
    w.add(const LineDocument(lineId: 0x7fffffffffffffff, position: 0, bookId: 1, text: 'x'));
    w.close();
    i.close();
    dir.deleteSync(recursive: true);
    // the struct returned by value: three pointer-sized words on every target
    expect(sizeOf<SeBuffer>(), 3 * sizeOf<IntPtr>());
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
    // resume after a position: the rest of the catalog-order run
    expect(ids(SearchOptions(q, grammarPrefixes: true, afterPosition: 0)), [7]);
    expect(ids(SearchOptions(q, grammarPrefixes: true, afterPosition: 1)), <int>[]);
    // issue #12: the resume carries the first page's order stamp; a stale one throws
    final first = index.search(SearchOptions(q, grammarPrefixes: true));
    final stamp = first.info().orderStamp;
    first.close();
    expect(stamp.length, 16);
    expect(ids(SearchOptions(q, grammarPrefixes: true, afterPosition: 0, resumeStamp: stamp)), [7]);
    expect(() => index.search(SearchOptions(q, grammarPrefixes: true, afterPosition: 0, resumeStamp: 'stale')),
        throwsA(isA<StaleResumeException>()));
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

  test('relevance of a phrase without positions: catalog order + notice', () {
    final dir = Directory.systemTemp.createTempSync('se_dart_flat_');
    final flat = SeforimIndex.create(dir.path, const IndexOptions(positions: false));
    final w = flat.openWriter(heapBytes: 30000000, threads: 1);
    w.add(const LineDocument(lineId: 1, position: 1, bookId: 1, text: 'alpha beta'));
    w.add(const LineDocument(lineId: 2, position: 0, bookId: 1, text: 'alpha gamma beta'));
    w.commit();
    w.close();
    final s = flat.search(SearchOptions('alpha beta',
        proximity: const Proximity.ordered([0]), order: ResultsOrder.relevance));
    expect(s.allHits().map((h) => h.lineId).toList(), [2, 1]);
    expect(s.info().notices, [SearchInfo.relevanceNeedsPositions]);
    s.close();
    final c = flat.search(SearchOptions('alpha beta', proximity: const Proximity.ordered([0])));
    expect(c.info().notices, isEmpty);
    c.close();
    flat.close();
    dir.deleteSync(recursive: true);
  });

  test('addAll: a batch indexes as add per line', () {
    const alpha = '\u05d0\u05dc\u05e4\u05d4\u05d0', beta = '\u05d1\u05d9\u05ea\u05d0';
    final docs = [
      const LineDocument(lineId: 1, position: 0, bookId: 1, text: '$alpha $beta', sectionId: 5),
      const LineDocument(lineId: 2, position: 1, bookId: 1, text: '', noCross: true),
      const LineDocument(lineId: 3, position: 2, bookId: 2, text: '<b>$beta</b> $alpha'),
    ];
    List<int> hits(void Function(IndexWriter) feed) {
      final dir = Directory.systemTemp.createTempSync('se_dart_batch_');
      final idx = SeforimIndex.create(dir.path);
      final w = idx.openWriter(heapBytes: 30000000, threads: 1);
      feed(w);
      w.commit();
      w.close();
      final s = idx.search(SearchOptions(beta));
      final r = s.allHits().map((h) => h.lineId).toList();
      s.close();
      idx.close();
      dir.deleteSync(recursive: true);
      return r;
    }

    expect(hits((w) => w.addAll(docs)), [1, 3]);
    expect(hits((w) => docs.forEach(w.add)), [1, 3]);
    final dir = Directory.systemTemp.createTempSync('se_dart_batch_bad_');
    final idx = SeforimIndex.create(dir.path);
    final w = idx.openWriter(heapBytes: 30000000, threads: 1);
    w.addAll(const []);
    expect(() => w.addAll(const [LineDocument(lineId: -1, position: 0, bookId: 1, text: 'x')]),
        throwsA(isA<SeforimEngineException>().having((e) => e.toString(), 'message', contains('record 0'))));
    w.close();
    idx.close();
    dir.deleteSync(recursive: true);
  });

  test('user regex of the line kind: candidates, text matching, full scan', () {
    final dir = Directory.systemTemp.createTempSync('se_dart_line_');
    final idx = SeforimIndex.create(dir.path);
    final w = idx.openWriter(heapBytes: 30000000, threads: 1);
    // Hebrew words (Latin letters fold to [Aa] classes under the
    // case-insensitive parse, as in Otzaria, and make no fragments);
    // same lengths as alpha / beta / gamma.
    const alpha = '\u05d0\u05dc\u05e4\u05d4\u05d0', beta = '\u05d1\u05d9\u05ea\u05d0', gamma = '\u05d2\u05de\u05dc\u05d4\u05d0';
    const texts = ['$alpha $beta $gamma', '$alpha $gamma', 'a c'];
    for (var i = 0; i < texts.length; i++) {
      w.add(LineDocument(lineId: 10 + i, position: i, bookId: 1, text: texts[i]));
    }
    w.commit();
    w.close();
    expect(SeforimIndex.userRegexKind('/$alpha .* $gamma/'), 'line');
    final s = idx.search(SearchOptions('/$alpha .* $gamma/', userRegex: true));
    final st = s.lineRegex()!;
    expect(st.fullScan, isFalse);
    final hits = s.allHits().toList();
    expect(hits.map((h) => h.lineId), [10, 11]);
    final found = s.lineMatchBatch([for (final h in hits) texts[h.lineId - 10]]);
    expect(found[0], [(0, 16)]);
    expect(found[1], isEmpty);
    expect(s.lineRegex()!.done, isTrue);
    expect(s.lineHits(0, 10).map((h) => h.lineId), [10]);
    s.close();
    final f = idx.search(SearchOptions(r'/\w[ ]c/', userRegex: true));
    expect(f.lineRegex()!.fullScan, isTrue);
    expect(f.lineRegex()!.scopeLines, 3);
    final r = f.lineScanBatch([for (var i = 0; i < 3; i++) PlacedHit(i, 10 + i, 1)], texts);
    expect(r[2], [(0, 3)]);
    f.close();
    idx.close();
    dir.deleteSync(recursive: true);
  });
  test('FM sidecar auto-repair: built in the background, switchable (issue #28)', () {
    final dir = Directory.systemTemp.createTempSync('se_dart_fm_');
    final idx = SeforimIndex.create(dir.path);
    final w = idx.openWriter(heapBytes: 30000000, threads: 1);
    w.add(const LineDocument(lineId: 1, position: 0, bookId: 1, text: 'alpha beta'));
    w.add(const LineDocument(lineId: 2, position: 1, bookId: 1, text: 'gamma alphabet'));
    w.mergeAndClose();
    idx.fmAutoRepair = false;
    idx.search(SearchOptions('*lph*')).close();
    expect(idx.fmSidecarRebuilding, isFalse);
    expect(idx.hasFmSidecar, isFalse);
    idx.fmAutoRepair = true;
    final s = idx.search(SearchOptions('*lph*'));
    expect(s.hitCount, 2);
    s.close();
    final until = DateTime.now().add(const Duration(seconds: 30));
    while (idx.fmSidecarRebuilding && DateTime.now().isBefore(until)) {
      sleep(const Duration(milliseconds: 10));
    }
    expect(idx.fmSidecarRebuilding, isFalse);
    expect(idx.hasFmSidecar, isTrue);
    expect(idx.fmSidecarProblem, isNull);
    idx.close();
    dir.deleteSync(recursive: true);
  });
}
