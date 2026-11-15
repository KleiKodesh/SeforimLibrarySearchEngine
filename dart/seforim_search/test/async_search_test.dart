import 'dart:async';
import 'dart:io';

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

void main() {
  late Directory tmp;
  late SearchWorker worker;
  final native = _nativePath();
  // one word repeated on many lines, so paging yields several batches
  const word = 'בראשית';
  const lines = 50;

  setUpAll(() async {
    SeforimNative.load(native);
    tmp = Directory.systemTemp.createTempSync('se_dart_async_');
    final index = SeforimIndex.create(tmp.path);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    for (var i = 0; i < lines; i++) {
      w.add(LineDocument(lineId: 100 + i, position: i, bookId: 1 + i % 3, text: '$word x$i'));
    }
    w.commit();
    w.close();
    index.close();
    worker = await SearchWorker.spawn(tmp.path, nativePath: native);
  });

  tearDownAll(() async {
    await worker.close();
    tmp.deleteSync(recursive: true);
  });

  List<PlacedHit> syncHits() {
    final index = SeforimIndex.open(tmp.path);
    final s = index.search(SearchOptions(word));
    final hits = s.allHits().toList();
    s.close();
    index.close();
    return hits;
  }

  test('stream yields the same hits as the sync API, in batches with a request id', () async {
    final batches = await worker.searchStream(SearchOptions(word), pageSize: 7).toList();
    expect(batches.length, 8);
    expect(batches.map((b) => b.requestId).toSet().length, 1);
    expect(batches.last.isLast, isTrue);
    final hits = [for (final b in batches) ...b.hits];
    final want = syncHits();
    expect(hits.length, lines);
    expect([for (final h in hits) h.lineId], [for (final h in want) h.lineId]);
  });

  test('streamed search (issue #21): pulled until complete, same hits, exact counts', () async {
    final index = SeforimIndex.open(tmp.path);
    final s = index.searchStreaming(SearchOptions(word));
    while (!s.pull(s.hitCount + 1)) {}
    expect(s.isComplete, isTrue);
    expect([for (final h in s.allHits()) h.lineId], [for (final h in syncHits()) h.lineId]);
    expect(s.counts().exact, isTrue);
    expect(s.counts().total, lines);
    s.close();
    index.close();
    final a = await worker.searchStreaming(SearchOptions(word));
    while (!await a.pull(a.hitCount + 1)) {}
    expect(a.isComplete, isTrue);
    expect(a.hitCount, lines);
    expect((await a.counts()).exact, isTrue);
    await a.close();
  });

  test('session API: paging, counts, locate on the worker', () async {
    final s = await worker.search(SearchOptions(word));
    expect(s.hitCount, lines);
    expect((await s.hits(10, 5)).map((h) => h.position), [10, 11, 12, 13, 14]);
    expect(await s.countNeedsText, isFalse);
    final c = await s.counts();
    expect(c.total, lines);
    final snip = await s.locateBatch(['$word x0', 'nothing']);
    expect(snip[0], isNotNull);
    expect(snip[1], isNull);
    expect((await s.info()).truncated, isFalse);
    await s.close();
    await s.close(); // idempotent
    expect(s.hits(0, 1), throwsA(isA<StateError>()));
  });

  test('cancel mid-stream: typed error, session freed, worker reused', () async {
    final token = SearchCancelToken();
    final s = await worker.search(SearchOptions(word), cancel: token);
    token.cancel();
    await expectLater(s.locateBatch(['$word x0']), throwsA(isA<SearchCancelledException>()));
    await s.close();
    // a cancelled token rejects a new stream with a typed error
    await expectLater(worker.searchStream(SearchOptions(word), cancel: token).toList(),
        throwsA(isA<SearchCancelledException>()));
    // listener cancel after the first batch frees the session
    final got = <HitBatch>[];
    late StreamSubscription<HitBatch> sub;
    final done = Completer<void>();
    sub = worker.searchStream(SearchOptions(word), pageSize: 5).listen((b) {
      got.add(b);
      sub.cancel().then((_) => done.complete());
    });
    await done.future;
    expect(got.length, 1);
    // the same worker still serves searches
    final again = await worker.searchStream(SearchOptions(word)).toList();
    expect(again.single.hits.length, lines);
  });

  test('errors propagate as typed exceptions', () async {
    await expectLater(
        worker.search(SearchOptions(word, afterPosition: 3, resumeStamp: 'not-this-index')),
        throwsA(isA<StaleResumeException>()));
    final bad = SearchWorker.spawn('${tmp.path}/missing', nativePath: native);
    await expectLater(bad, throwsA(isA<SeforimEngineException>()));
  });

  test('index writer on the worker (issue #38): add, delete, commit, merge, resort with progress + cancel', () async {
    final dir = Directory.systemTemp.createTempSync('se_dart_async_w_');
    SeforimIndex.create(dir.path).close();
    final ww = await SearchWorker.spawn(dir.path, nativePath: native);
    try {
      final w = await ww.openWriter(heapBytes: 30000000, threads: 1);
      expect(await w.memoryBudgetPerThread(), greaterThan(0));
      await w.addAll([
        for (var i = 0; i < 30; i++) LineDocument(lineId: 1 + i, position: i, bookId: 1 + i % 3, text: '$word y$i')
      ]);
      await w.add(LineDocument(lineId: 99, position: 30, bookId: 1, text: '$word z'));
      await w.commit('p1');
      expect(await ww.numLines(), 31);
      await w.delete(99);
      await w.deleteBook(3);
      await w.commit();
      expect((await ww.searchStream(SearchOptions(word)).toList()).single.hits.length, 20);
      await w.mergeAndClose();
      await expectLater(w.commit(), throwsA(isA<StateError>()));
      // a cancelled re-sort leaves the index and frees the writer
      final cancel = SearchCancelToken()..cancel();
      final w2 = await ww.openWriter(heapBytes: 30000000, threads: 1);
      await expectLater(w2.resortAndClose([2, 1], cancel: cancel), throwsA(isA<SearchCancelledException>()));
      final progress = ResortProgressCell();
      final w3 = await ww.openWriter(heapBytes: 30000000, threads: 1);
      expect(await w3.resortAndClose([2, 1], progress: progress), 20);
      expect(progress.read().stage, ResortStage.done);
      final hits = (await ww.searchStream(SearchOptions(word)).toList()).single.hits;
      expect(hits.first.lineId % 3, 2); // book 2 first now
      expect(await ww.buildFmSidecar(), isA<bool>());
      expect(await ww.startBackgroundMerge(), isFalse); // one segment: nothing to do
      final w4 = await ww.openWriter(heapBytes: 30000000, threads: 1);
      await w4.close();
      await w4.close();
    } finally {
      await ww.close();
      dir.deleteSync(recursive: true);
    }
  });
}
