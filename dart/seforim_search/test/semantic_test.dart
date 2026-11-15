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

Float32List v(List<double> x) => Float32List.fromList(x);

void main() {
  late Directory tmp;
  final native = _nativePath();

  setUpAll(() {
    SeforimNative.load(native);
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

  test('vectors, semantic-only search, book filter, hybrid fusion (issue #18)', () {
    final index = SeforimIndex.open(tmp.path);
    expect(index.vectorInfo(), isNull);
    final stats = index.updateVectors(VectorUpdate(3, add: [
      LineVector(1, 5, v([1, 0, 0])),
      LineVector(2, 5, v([0.9, 0.1, 0])),
      LineVector(3, 6, v([0, 0, 1])),
    ]));
    expect(stats.lines, 3);
    expect(index.vectorInfo()!.dims, 3);

    final sem = index.semanticSearch(v([1, 0, 0]), const SemanticOptions(k: 2));
    expect(sem.hits.map((h) => h.lineId), [1, 2]);
    final filtered = index.semanticSearch(v([1, 0, 0]), const SemanticOptions(k: 5, books: [6]));
    expect(filtered.hits.single.bookId, 6);
    expect(filtered.exact, isTrue);

    final s = index.search(SearchOptions('delta', order: ResultsOrder.relevance));
    final fused = s.fuseSemantic('delta', sem);
    expect(fused.length, 3);
    expect(fused.firstWhere((f) => f.lineId == 3).source, 'lexical');
    expect(fused.firstWhere((f) => f.lineId == 1).position, isNull);
    expect(s.fuseSemantic('delta', sem, const FusionOptions(strategy: FusionStrategy.rrf)).length, 3);
    s.close();

    final cancel = SearchCancelToken()..cancel();
    expect(() => index.semanticSearch(v([1, 0, 0]), const SemanticOptions(), cancel), throwsA(isA<SearchCancelledException>()));
    expect(() => index.semanticSearch(v([1, 0])), throwsA(isA<SeforimEngineException>()));
    expect(index.updateVectors(VectorUpdate(3, removeBooks: [5])).lines, 1);
    index.close();
  });

  test('async on a SearchWorker', () async {
    final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
    final stats = await worker.updateVectors(VectorUpdate(3, add: [LineVector(7, 6, v([0, 1, 0]))]));
    expect(stats.added, 1);
    final sem = await worker.semanticSearch(v([0, 1, 0]), const SemanticOptions(k: 1));
    expect(sem.hits.single.lineId, 7);
    expect((await worker.vectorInfo())!.lines, greaterThan(0));
    final session = await worker.search(SearchOptions('delta', order: ResultsOrder.relevance));
    final fused = await session.fuseSemantic('delta', sem);
    expect(fused.map((f) => f.lineId).toSet(), {3, 7});
    await session.close();
    await worker.close();
  });

  test('Otzaria vector file imported by the host line ids, chunks kept, hybrid (issue #18)', () async {
    // the fixture Rust writes (seforim-vectors the_hosts_oxv_fixture_is_current)
    var dir = Directory.current;
    File? oxv;
    for (var i = 0; i < 5 && oxv == null; i++) {
      final f = File('${dir.path}/seforim-vectors/tests/data/synthetic.oxv');
      if (f.existsSync()) oxv = f;
      dir = dir.parent;
    }
    final info = readOxvInfo(oxv!.path);
    expect((info.format, info.dims, info.codec, info.libraryReleaseTag), (3, 4, 'f32', 'v1-synthetic'));
    expect(info.books.map((b) => (b.name, b.records, b.lines)), [('id:5', 3, 2), ('id:7', 1, 1)]);

    final d = Directory.systemTemp.createTempSync('se_dart_oxv_');
    final index = SeforimIndex.create(d.path);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    w.add(LineDocument(lineId: 50, position: 0, bookId: 5, text: 'alpha'));
    w.add(LineDocument(lineId: 51, position: 1, bookId: 5, text: 'beta'));
    w.commit();
    w.close();
    final report = index.importOxv(oxv.path, [
      const OxvBookMap('id:5', 5, [50, 51]),
      const OxvBookMap('id:9', 9, [90]),
    ]);
    expect((report.booksImported, report.booksUnmapped, report.vectors, report.lines), (1, 1, 3, 2));
    expect(report.booksNotInFile, ['id:9']);
    expect(index.vectorInfo()!.vectors, 3);
    // line 50 is found by its second chunk, penalized 0.04
    final sem = index.semanticSearch(v([0, 0, 1, 0]), const SemanticOptions(k: 2));
    expect((sem.hits.first.lineId, sem.hits.first.chunk), (50, 1));
    expect(sem.hits.first.score, closeTo(0.96, 1e-3));
    final s = index.search(SearchOptions('alpha', order: ResultsOrder.relevance));
    expect(s.fuseSemantic('alpha', sem).first.source, 'both');
    s.close();
    index.close();
    final worker = await SearchWorker.spawn(d.path, nativePath: native);
    final again = await worker.importOxv(oxv.path, [const OxvBookMap('id:5', 5, [50, 51])]);
    expect(again.vectors, 3);
    await worker.close();
    d.deleteSync(recursive: true);
  });
}
