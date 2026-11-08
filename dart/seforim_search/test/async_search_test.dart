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
}
