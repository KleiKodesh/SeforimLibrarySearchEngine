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
    w.add(const LineDocument(lineId: 7, position: 1, bookId: 1, lineIndex: 0, text: text));
    w.add(const LineDocument(lineId: 8, position: 0, bookId: 2, lineIndex: 0, tocEntryId: 3, text: text2));
    w.add(const LineDocument(lineId: 9, position: 2, bookId: 1, lineIndex: 1, text: 'temp'));
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
    final s = index.search(SearchRequest(groups: [
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
    final s = index.search(SearchRequest(groups: [const QueryGroup.wildcard('ברא*')]));
    expect(s.allHits().map((h) => h.lineId).toList(), [8]);
    expect(s.hits(0, 1).single.tocEntryId, 3);
    s.close();
    final none = index.search(SearchRequest(groups: [
      const QueryGroup.word('בראשית'),
      const QueryGroup.word('למשה'),
    ]));
    expect(none.hitCount, 0);
    none.close();
  });

  test('distance filter drops far matches in locate', () {
    final s = index.search(SearchRequest(
      groups: [const QueryGroup.word('בראשית'), const QueryGroup.word('הארץ')],
      maxWordDistance: 1,
    ));
    expect(s.hitCount, 1);
    expect(s.locateBatch([text2]).single, isNull);
    s.close();
  });

  test('any + fuzzy + neighbours', () {
    final s = index.search(SearchRequest(groups: [
      const QueryGroup.any([QueryGroup.word('משה'), QueryGroup.fuzzy('למשא', 1)]),
    ]));
    expect(s.hitCount, 1);
    final own = s.locate(text)!;
    final wide = s.locateWithNeighbours('לפני', text, 'אחרי', own);
    expect(wide, isNotNull);
    s.close();
  });

  test('errors surface as exceptions', () {
    expect(() => SeforimIndex.open('${tmp.path}/nope'), throwsA(isA<SeforimEngineException>()));
  });
}
