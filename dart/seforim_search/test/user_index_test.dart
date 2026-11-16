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
  test('user index (issue #44): documents, attach, sources, line sources and texts', () {
    SeforimNative.load(_nativePath());
    final tmp = Directory.systemTemp.createTempSync('se_user_');
    final lib = SeforimIndex.create('${tmp.path}/lib');
    var w = lib.openWriter(heapBytes: 30000000, threads: 1);
    w.add(const LineDocument(lineId: 5, position: 0, bookId: 1, text: 'alpha beta'));
    w.commit();
    w.close();

    final user = SeforimIndex.createUser('${tmp.path}/user');
    expect(user.isUserIndex, isTrue);
    final base = SeforimIndex.userIdBase;
    w = user.openWriter(heapBytes: 30000000, threads: 1);
    expect(w.putDocument('c:/a.pdf', 'h1', const [UserDocumentLine('gamma', locator: 1), UserDocumentLine('alpha', locator: 2)]),
        base);
    w.putDocument('c:/b.txt', 'h2', const [UserDocumentLine('alpha delta')]);
    w.commit();
    w.close();
    lib.attachUserIndex(user);

    List<int> ids(SearchSources s) {
      final r = lib.search(SearchOptions('alpha', sources: s));
      final hits = [for (final h in r.hits(0, 100)) h.lineId];
      r.close();
      return hits;
    }

    final a1 = base + 1, b0 = base + (1 << 24);
    expect(ids(SearchSources.all), [5, a1, b0]);
    expect(ids(SearchSources.library), [5]);
    expect(ids(SearchSources.user), [a1, b0]);
    final src = lib.lineSources([a1, 5]);
    expect(src[0]!.key, 'c:/a.pdf');
    expect(src[0]!.locator, 2);
    expect(src[1], isNull);
    expect(lib.lineTexts([a1, 5]), ['alpha', null]);

    w = user.openWriter(heapBytes: 30000000, threads: 1);
    expect(w.deleteDocuments(['c:/b.txt', 'none']), 1);
    w.commit();
    w.close();
    final docs = lib.documents();
    expect(docs.single.key, 'c:/a.pdf');
    expect(docs.single.stamp, 'h1');
    expect(ids(SearchSources.all), [5, a1]);

    lib.attachUserIndex(null);
    user.close();
    lib.close();
    tmp.deleteSync(recursive: true);
  });
}
