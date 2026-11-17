import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_search.dart';
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

/// The fixture Rust writes (seforim-vectors the_hosts_oxv_fixture_is_current), 4 dims:
/// book id:5 = ordinal 0 with windows 0 (axis 1) and 1 (axis 2), ordinal 1 (axis 0);
/// book id:7 = ordinal 0 (axis 3).
String _fixture() {
  var dir = Directory.current;
  for (var i = 0; i < 5; i++) {
    final f = File('${dir.path}/seforim-vectors/tests/data/synthetic.oxv');
    if (f.existsSync()) return f.path;
    dir = dir.parent;
  }
  throw StateError('synthetic.oxv not found');
}

Float32List v(List<double> x) => Float32List.fromList(x);

void main() {
  late Directory tmp;
  late String oxv;
  final native = _nativePath();

  setUpAll(() {
    SeforimNative.load(native);
    oxv = _fixture();
    tmp = Directory.systemTemp.createTempSync('se_dart_semantic_');
    final index = SeforimIndex.create(tmp.path);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    w.add(LineDocument(lineId: 1, position: 0, bookId: 5, text: 'alpha beta'));
    w.add(LineDocument(lineId: 2, position: 1, bookId: 5, text: 'gamma'));
    w.add(LineDocument(lineId: 3, position: 2, bookId: 6, text: 'delta'));
    w.commit();
    w.close();
    index.close();
  });

  tearDownAll(() => tmp.deleteSync(recursive: true));

  test('Otzaria vector set: mapping, unmapped, book filter, hybrid, cancel (issue #18)', () {
    final info0 = readOxvInfo(oxv);
    expect(info0.books.map((b) => b.name), ['id:5', 'id:7']);

    final index = SeforimIndex.open(tmp.path);
    expect(index.vectorInfo(), isNull);
    expect(index.semanticSearch(v([0, 0, 1, 0])).hits, isEmpty);
    final info = index.openVectors(oxv);
    expect((info.dims, info.books, info.booksMapped, info.segments.length), (4, 2, 0, 1));
    index.mapVectorBook('id:5', 5, [1, 2]);
    expect((index.vectorInfo()!.booksMapped, index.vectorInfo()!.booksUnmapped), (1, 1));

    // line 1 is found by its second window, penalized 0.04
    final sem = index.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 2));
    expect((sem.hits.first.lineId, sem.hits.first.bookId, sem.hits.first.chunk), (1, 5, 1));
    expect(sem.hits.first.score, closeTo(0.96, 1e-3));
    final none = index.semanticSearch(v([0, 0, 0, 1]), const SemanticOptions(k: 1));
    expect(none.hits, isEmpty);
    expect(none.unmapped, 1);
    expect(index.semanticSearch(v([1, 0, 0, 0]), const SemanticOptions(k: 1)).hits.single.lineId, 2);
    expect(index.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 5, books: [6])).hits, isEmpty);
    expect(index.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 5, books: [5])).hits.every((h) => h.bookId == 5),
        isTrue);
    index.setVectorMapping([const OxvBookMap('id:7', 6, [3])]);
    final seven = index.semanticSearch(v([0, 0, 0, 1]), const SemanticOptions(k: 1));
    expect((seven.hits.single.lineId, seven.unmapped), (3, 0));

    // Otzaria's hybrid session: the lexical side (its exact query) fused with the semantic one
    final h = index.hybridSession('alpha', v([0, 0, 1, 0]));
    final page = h.page(0, 10);
    expect((page.hits.first.lineId, page.hits.first.source), (1, 'both'));
    expect((page.more, page.window, page.exhausted), (false, 21, true));
    expect((page.meta.executedMode, page.meta.semanticAvailable, page.meta.fallbackKind), (HybridRetrieval.hybrid, true, null));
    h.close();
    // Otzaria's retrieval modes: semantic only discards the lexical candidates (counted once)
    final hs = index.hybridSession('alpha', v([0, 0, 1, 0]), const HybridOptions(retrieval: HybridRetrieval.semanticOnly));
    final sp = hs.page(0, 10);
    expect(sp.hits.every((h) => h.source == 'semantic' && h.lexical == null), isTrue);
    expect((sp.meta.executedMode, sp.meta.requestedMode, sp.lexicalTotal > 0), (HybridRetrieval.semanticOnly, HybridRetrieval.semanticOnly, true));
    final w = hs.window(5);
    expect((w.window, w.semanticDone, w.hits.length <= 5), (5, true, true));
    hs.close();
    final lex = index.hybridSession('alpha', v([0, 0, 1, 0]),
        const HybridOptions(retrieval: HybridRetrieval.lexicalOnly, lexicalMode: HybridLexicalMode.fuzzy));
    final lp = lex.page(0, 10);
    expect(lp.hits.every((h) => h.source == 'lexical'), isTrue);
    expect((lp.meta.executedMode, lp.meta.semanticAvailable), (HybridRetrieval.lexicalOnly, false));
    lex.close();
    final rrf = index.hybridSession('alpha', v([0, 0, 1, 0]), const HybridOptions(ranking: HybridOptions.balanced));
    // RRF (Otzaria's Fast profile) leaves out semantic lines under 0.55 normalized
    expect(rrf.page(0, 10).hits.map((h) => h.lineId), contains(1));
    rrf.close();

    final cancel = SearchCancelToken()..cancel();
    expect(() => index.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(), cancel),
        throwsA(isA<SearchCancelledException>()));
    expect(() => index.semanticSearch(v([1, 0])), throwsA(isA<SeforimEngineException>()));
    index.setVectorMapping([], replace: true);
    expect(index.vectorInfo()!.booksMapped, 0);
    index.closeVectors();
    expect(index.vectorInfo(), isNull);
    index.close();
  });

  test('rows path: text check against real chunk keys (verified.oxv)', () async {
    // seforim-vectors the_hosts_verified_fixture_is_current: book id:3, 8 dims, line p = axis p
    final verified = '${File(oxv).parent.path}/verified.oxv';
    final rows = [
      '<h1>Title of the book</h1>',
      'first line that is long enough to stand',
      'second\nthird line long enough to stand alone too',
      'fourth line long enough to stand alone here',
    ].map((r) => Uint8List.fromList(utf8.encode(r))).toList();
    const ids = [100, 101, 102, 103];
    const identity = 320260894818558297;
    Float32List axis(int a) => Float32List(8)..[a] = 1;
    final index = SeforimIndex.open(tmp.path);
    index.openVectors(verified);
    final vb = index.mapVectorBookRows('id:3', 3, ids, rows, const [], identity);
    expect([vb.lines, vb.records, vb.verified, vb.unverified], [5, 5, 5, 0]);
    expect(index.semanticSearch(axis(3), const SemanticOptions(k: 1)).hits.single.lineId, 102);
    rows[3] = Uint8List.fromList(utf8.encode('fourth line CHANGED enough to stand alone here'));
    final vt = index.mapVectorBookRows('id:3', 3, ids, rows, const [], identity);
    expect(vt.unverified, greaterThanOrEqualTo(1));
    final r4 = index.semanticSearch(axis(4), const SemanticOptions(k: 1));
    expect(r4.hits, isEmpty);
    expect(r4.unmapped, 1);
    index.closeVectors();
    index.close();

    final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
    await worker.openVectors(verified);
    final wv = await worker.mapVectorBookRows('id:3', 3, ids, rows, const [], identity);
    expect(wv.unverified, greaterThanOrEqualTo(1));
    await worker.close();
  });

  test('async on a SearchWorker', () async {
    final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
    final info = await worker.openVectors(oxv);
    expect(info.dims, 4);
    await worker.mapVectorBook('id:5', 5, [1, 2]);
    await worker.setVectorMapping([const OxvBookMap('id:7', 6, [3])]);
    expect((await worker.vectorInfo())!.booksMapped, 2);
    final sem = await worker.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 1));
    expect(sem.hits.single.lineId, 1);
    await worker.closeVectors();
    expect(await worker.vectorInfo(), isNull);
    await worker.close();
  });
}
