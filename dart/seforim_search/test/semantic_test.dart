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

    final s = index.search(SearchOptions('alpha', order: ResultsOrder.relevance));
    final fused = s.fuseSemantic('alpha', sem);
    expect((fused.first.lineId, fused.first.source), (1, 'both'));
    expect(s.fuseSemantic('alpha', sem, const FusionOptions(strategy: FusionStrategy.rrf)).length, fused.length);
    s.close();

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

  test('async on a SearchWorker', () async {
    final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
    final info = await worker.openVectors(oxv);
    expect(info.dims, 4);
    await worker.mapVectorBook('id:5', 5, [1, 2]);
    await worker.setVectorMapping([const OxvBookMap('id:7', 6, [3])]);
    expect((await worker.vectorInfo())!.booksMapped, 2);
    final sem = await worker.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 1));
    expect(sem.hits.single.lineId, 1);
    final session = await worker.search(SearchOptions('delta', order: ResultsOrder.relevance));
    final fused = await session.fuseSemantic('delta', sem);
    expect(fused.map((f) => f.lineId).toSet(), {1, 3});
    await session.close();
    await worker.closeVectors();
    expect(await worker.vectorInfo(), isNull);
    await worker.close();
  });
}
