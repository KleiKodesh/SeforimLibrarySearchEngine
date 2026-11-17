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

final _env = Platform.environment;
final _modelDir = _env['OTZARIA_MEIVIN_MODEL'] ?? '${_env['APPDATA']}/otzaria/books/meivin-qa491-onnx';
final _ort = _env['OTZARIA_ONNX_RUNTIME'] ??
    '${_env['USERPROFILE']}/.nuget/packages/microsoft.ml.onnxruntime/1.28.0/runtimes/win-x64/native/onnxruntime.dll';

void main() {
  late Directory tmp;
  late String oxv;
  final native = _nativePath();
  const texts = {1: 'alpha beta', 2: 'gamma', 3: 'alpha beta delta'};

  void build(String dir, bool positions) {
    final index = SeforimIndex.create(dir, IndexOptions(positions: positions));
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    w.add(LineDocument(lineId: 1, position: 0, bookId: 5, text: texts[1]!));
    w.add(LineDocument(lineId: 2, position: 1, bookId: 5, text: texts[2]!));
    w.add(LineDocument(lineId: 3, position: 2, bookId: 6, text: texts[3]!));
    w.commit();
    w.close();
    index.close();
  }

  setUpAll(() {
    SeforimNative.load(native);
    oxv = _fixture();
    tmp = Directory.systemTemp.createTempSync('se_dart_async_hybrid_');
    build('${tmp.path}/pos', true);
    build('${tmp.path}/nopos', false);
  });

  tearDownAll(() => tmp.deleteSync(recursive: true));

  test('worker hybrid session: page, window, cancel, close (issue #18)', () async {
    final worker = await SearchWorker.spawn('${tmp.path}/pos', nativePath: native);
    try {
      await worker.openVectors(oxv);
      await worker.mapVectorBook('id:5', 5, [1, 2]);
      final h = await worker.hybridSession('alpha', vector: v([0, 0, 1, 0]));
      final page = await h.page(0, 10);
      expect((page.hits.first.lineId, page.hits.first.source), (1, 'both'));
      expect((page.meta.executedMode, page.meta.semanticAvailable), (HybridRetrieval.hybrid, true));
      // the same answers as the synchronous session
      final index = SeforimIndex.open('${tmp.path}/pos');
      index.openVectors(oxv);
      index.mapVectorBook('id:5', 5, [1, 2]);
      final sync = index.hybridSession('alpha', v([0, 0, 1, 0]));
      final sp = sync.page(0, 10);
      expect([for (final x in page.hits) (x.lineId, x.source, x.fused)], [for (final x in sp.hits) (x.lineId, x.source, x.fused)]);
      final sw = sync.window(5);
      final w = await h.window(5);
      expect((w.window, w.fusedCount), (sw.window, sw.fusedCount));
      expect(w.hits.map((x) => x.lineId).toList(), sw.hits.map((x) => x.lineId).toList());
      sync.close();
      index.close();

      // a cancelled token stops a step that fuses (a fresh session: a served window is cached)
      final cancel = SearchCancelToken()..cancel();
      final hc = await worker.hybridSession('alpha', vector: v([0, 0, 1, 0]));
      await expectLater(hc.page(0, 10, cancel: cancel), throwsA(isA<SearchCancelledException>()));
      await hc.close();
      await h.close();
      await h.close(); // idempotent
      await expectLater(h.page(0, 10), throwsStateError);

      // no vector and no embedder: the lexical side alone
      final lex = await worker.hybridSession('alpha');
      final lp = await lex.page(0, 10);
      expect(lp.hits.every((x) => x.source == 'lexical'), isTrue);
      await lex.close();
    } finally {
      await worker.close();
    }
  });

  test('worker hybrid session on an index without positions asks the host for texts', () async {
    final worker = await SearchWorker.spawn('${tmp.path}/nopos', nativePath: native);
    try {
      Map<int, Uint8List> raw(List<int> ids) =>
          {for (final id in ids) if (texts[id] != null) id: Uint8List.fromList(utf8.encode(texts[id]!))};
      final asked = <int>[];
      final h = await worker.hybridSession('alpha beta', texts: (ids) async {
        asked.addAll(ids);
        return raw(ids);
      });
      final page = await h.page(0, 10);
      expect(asked, isNotEmpty);
      expect(page.hits.map((x) => x.lineId).toSet(), {1, 3});
      // without a provider the step fails, on the worker and here alike
      final bare = await worker.hybridSession('alpha beta');
      await expectLater(bare.window(10), throwsStateError);
      // a per-call provider works too
      final w = await bare.window(10, texts: raw);
      expect(w.hits.map((x) => x.lineId).toSet(), {1, 3});
      await bare.close();
      await h.close();
    } finally {
      await worker.close();
    }
  });

  final haveModel = File('$_modelDir/model.json').existsSync() && File(_ort).existsSync();

  test('worker embedder: query vector, passages, highlights, hybrid by query text', () async {
    final worker = await SearchWorker.spawn('${tmp.path}/pos', nativePath: native);
    final e = MeivinEmbedder.open(_modelDir, _ort);
    try {
      expect(await worker.hasEmbedder(), isFalse);
      await expectLater(worker.embedQuery('alpha'), throwsStateError);
      expect(await worker.loadEmbedder(_modelDir, _ort), e.dims);
      expect(await worker.embedQuery('alpha'), e.embedQuery('alpha'));
      expect((await worker.embedQueries(['a', 'b']))[1], e.embedQuery('b'));
      expect((await worker.embedPassages(['alpha beta'])).single, e.embedPassages(['alpha beta']).single);
      const targets = [PassageTarget('alpha beta, gamma delta; and more words here')];
      final hl = await worker.semanticPassageHighlights('alpha', targets);
      final ref = semanticPassageHighlights('alpha', targets, e);
      expect([for (final x in hl) (x.snippetHtml, x.isHighlighted)], [for (final x in ref) (x.snippetHtml, x.isHighlighted)]);
      // the worker embeds the query of a hybrid session itself (no vector set open: lexical fallback)
      final h = await worker.hybridSession('alpha');
      final p = await h.page(0, 10);
      expect(p.hits.map((x) => x.lineId), contains(1));
      await h.close();
      await worker.unloadEmbedder();
      expect(await worker.hasEmbedder(), isFalse);
    } finally {
      e.dispose();
      await worker.close();
    }
  }, skip: haveModel ? false : 'Otzaria model not installed: set OTZARIA_MEIVIN_MODEL and OTZARIA_ONNX_RUNTIME');
}
