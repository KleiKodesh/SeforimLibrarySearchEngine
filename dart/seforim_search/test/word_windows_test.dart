// Word-level descriptors (word_windows): the window decided from the index
// positions during the search, finished with the line text, equals the
// snippet located on the same text; sync and worker isolate.
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

String _key(Snippet? s) => s == null
    ? 'none'
    : '${s.score} ${s.wordDistance} ${s.windowWordCount} ${s.start} ${s.end} ${s.flags} ${s.breakAt} '
        '${[for (final m in s.marks) '${m.start}/${m.length}/${m.termId}'].join(',')}';

void main() {
  late Directory tmp;
  final native = _nativePath();
  final texts = {
    for (var i = 0; i < 40; i++)
      100 + i: 'w$i alpha ${'pad ' * (i % 7)}beta gamma alpha delta ${'x ' * (i % 5)}beta end$i',
  };

  setUpAll(() {
    SeforimNative.load(native);
    tmp = Directory.systemTemp.createTempSync('se_dart_ww_');
    final index = SeforimIndex.create(tmp.path);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    var pos = 0;
    for (final e in texts.entries) {
      w.add(LineDocument(lineId: e.key, position: pos++, bookId: 1, text: e.value));
    }
    w.commit();
    w.close();
    index.close();
  });

  tearDownAll(() {
    try {
      tmp.deleteSync(recursive: true);
    } catch (_) {}
  });

  for (final (query, distance, ordered) in [
    ('alpha beta', null, false),
    ('alpha beta', 2, false),
    ('beta alpha', 3, true),
    ('alpha', null, false),
  ]) {
    test('finished == located: $query d=$distance ordered=$ordered', () async {
      final index = SeforimIndex.open(tmp.path);
      SearchOptions opts(bool ww) => SearchOptions(query, maxWordDistance: distance, requireOrdered: ordered, contextWords: 3)
        ..wordWindows = ww;
      expect(index.search(opts(false)).wordWindows(0, 100), isEmpty);
      final s = index.search(opts(true));
      final hits = s.allHits().toList();
      expect(hits, isNotEmpty);
      final windows = s.wordWindows(0, hits.length);
      expect(windows.length, hits.length);
      expect(windows.every((w) => !w.isNone), isTrue);
      final lineTexts = [for (final h in hits) texts[h.lineId]!];
      final finished = s.finishWindows(windows, lineTexts);
      final located = s.locateBatch(lineTexts);
      for (var i = 0; i < hits.length; i++) {
        expect(finished[i], isNotNull);
        expect(_key(finished[i]), _key(located[i]));
      }
      // worker isolate: same
      final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
      final a = await worker.search(opts(true));
      final aw = await a.wordWindows(0, hits.length);
      expect(aw, windows);
      final af = await a.finishWindows(aw, lineTexts);
      expect([for (final f in af) _key(f)], [for (final f in finished) _key(f)]);
      await a.close();
      await worker.close();
      s.close();
      index.close();
    });
  }
}
