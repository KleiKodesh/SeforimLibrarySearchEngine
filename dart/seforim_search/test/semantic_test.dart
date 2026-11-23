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

/// The fixture Rust writes (seforim-vectors the_hosts_verified_fixture_is_current):
/// book id:3, 8 dims, Otzaria line p = axis p, keyed by the production recipe.
String _fixture() {
  var dir = Directory.current;
  for (var i = 0; i < 5; i++) {
    final f = File('${dir.path}/seforim-vectors/tests/data/verified.oxv');
    if (f.existsSync()) return f.path;
    dir = dir.parent;
  }
  throw StateError('verified.oxv not found');
}

/// The rows of book 3 (line ids 100..103; row 102 holds a newline: Otzaria lines 2 and 3).
const verifiedRows = [
  '<h1>Title of the book</h1>',
  'first line that is long enough to stand',
  'second\nthird line long enough to stand alone too',
  'fourth line long enough to stand alone here',
];

/// Writes book 3 with its chunk keys (as a build or a per-book update does).
void writeBook3(IndexWriter w, List<String> rows, {int firstId = 100, int position = 0}) {
  final ids = [for (var i = 0; i < rows.length; i++) firstId + i];
  w.deleteBook(3);
  w.setBookChunkKeys(ids, [for (final r in rows) Uint8List.fromList(utf8.encode(r))]);
  for (var i = 0; i < rows.length; i++) {
    w.add(LineDocument(lineId: ids[i], position: position + i, bookId: 3, text: rows[i]));
  }
}

Float32List axis(int a) => Float32List(8)..[a] = 1;

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
    writeBook3(w, verifiedRows);
    w.add(LineDocument(lineId: 200, position: 10, bookId: 9, text: 'first words elsewhere'));
    w.commit();
    w.close();
    index.close();
  });

  tearDownAll(() => tmp.deleteSync(recursive: true));

  test('Otzaria vector set: lazy resolution by the chunk key column, filter, hybrid, cancel (issue #18)', () {
    final info0 = readOxvInfo(oxv);
    expect(info0.books.map((b) => b.name), ['id:3']);

    final index = SeforimIndex.open(tmp.path);
    expect(index.vectorInfo(), isNull);
    expect(index.semanticSearch(axis(1)).hits, isEmpty);
    // opening maps nothing; the index holds the set's one book
    final info = index.openVectors(oxv);
    expect((info.dims, info.books, info.booksMapped, info.booksUnmapped, info.segments.length), (8, 1, 1, 0, 1));

    // Otzaria line p -> its row's line id (lines 2 and 3 are one row)
    for (final (p, id) in [(0, 100), (1, 101), (2, 102), (3, 102), (4, 103)]) {
      expect(index.semanticSearch(axis(p), const SemanticOptions(k: 1)).hits.single.lineId, id, reason: 'axis $p');
    }
    expect(index.semanticSearch(axis(1), const SemanticOptions(k: 5, books: [9])).hits, isEmpty);
    expect(index.semanticSearch(axis(1), const SemanticOptions(k: 5, books: [3])).hits.every((h) => h.bookId == 3), isTrue);

    // Otzaria's hybrid session: the lexical side (its exact query) fused with the semantic one
    final h = index.hybridSession('first', axis(1));
    final page = h.page(0, 10);
    final one = page.hits.firstWhere((x) => x.lineId == 101);
    expect(one.source, 'both');
    expect(page.hits.map((x) => x.lineId), contains(200));
    expect((page.meta.executedMode, page.meta.semanticAvailable, page.meta.fallbackKind), (HybridRetrieval.hybrid, true, null));
    h.close();
    final hs = index.hybridSession('first', axis(1), const HybridOptions(retrieval: HybridRetrieval.semanticOnly));
    final sp = hs.page(0, 10);
    expect(sp.hits.every((h) => h.source == 'semantic' && h.lexical == null), isTrue);
    expect((sp.meta.executedMode, sp.lexicalTotal > 0), (HybridRetrieval.semanticOnly, true));
    final w = hs.window(5);
    expect((w.window, w.semanticDone, w.hits.length <= 5), (5, true, true));
    hs.close();
    final lex = index.hybridSession('first', axis(1),
        const HybridOptions(retrieval: HybridRetrieval.lexicalOnly, lexicalMode: HybridLexicalMode.fuzzy));
    final lp = lex.page(0, 10);
    expect(lp.hits.every((h) => h.source == 'lexical'), isTrue);
    lex.close();

    final cancel = SearchCancelToken()..cancel();
    expect(() => index.semanticSearch(axis(1), const SemanticOptions(), cancel), throwsA(isA<SearchCancelledException>()));
    expect(() => index.semanticSearch(Float32List.fromList([1, 0])), throwsA(isA<SeforimEngineException>()));
    index.closeVectors();
    expect(index.vectorInfo(), isNull);
    index.close();
  });

  test('a per-book update rewrites the chunk keys: a changed line no longer holds its vector', () {
    final dir = Directory('${tmp.path}_upd')..createSync();
    try {
      final index = SeforimIndex.create(dir.path);
      var w = index.openWriter(heapBytes: 30000000, threads: 1);
      writeBook3(w, verifiedRows);
      w.commit();
      w.close();
      index.openVectors(oxv);
      expect(index.semanticSearch(axis(4), const SemanticOptions(k: 1)).hits.single.lineId, 103);
      final changed = [...verifiedRows]..[3] = 'fourth line CHANGED enough to stand alone here';
      w = index.openWriter(heapBytes: 30000000, threads: 1);
      writeBook3(w, changed, firstId: 110, position: 20);
      w.commit();
      w.close();
      index.reload();
      final r4 = index.semanticSearch(axis(4), const SemanticOptions(k: 1));
      expect((r4.hits.isEmpty, r4.unmapped), (true, 1));
      expect(index.semanticSearch(axis(1), const SemanticOptions(k: 1)).hits.single.lineId, 111);
      index.close();
    } finally {
      dir.deleteSync(recursive: true);
    }
  });

  test('line hashes from the index: identical texts alike, short lines none', () {
    final dir = Directory('${tmp.path}_hash')..createSync();
    try {
      final index = SeforimIndex.create(dir.path);
      final w = index.openWriter(heapBytes: 30000000, threads: 1);
      final text = String.fromCharCodes([for (var i = 0; i < 14; i++) 0x05D0 + i]);
      for (final (i, (id, t)) in [(1, text), (2, 'short'), (3, text)].indexed) {
        w.add(LineDocument(lineId: id, position: i, bookId: 1 + i, text: t));
      }
      w.commit();
      w.close();
      final h = index.lineHashes([1, 2, 3, 99]);
      expect((h[0] != 0, h[1], h[2] == h[0], h[3]), (true, 0, true, 0));
      index.close();
    } finally {
      dir.deleteSync(recursive: true);
    }
  });

  test('async on a SearchWorker', () async {
    final worker = await SearchWorker.spawn(tmp.path, nativePath: native);
    final info = await worker.openVectors(oxv);
    expect((info.dims, info.booksMapped), (8, 1));
    final sem = await worker.semanticSearch(axis(3), const SemanticOptions(k: 1));
    expect(sem.hits.single.lineId, 102);
    await worker.closeVectors();
    expect(await worker.vectorInfo(), isNull);
    await worker.close();
  });
}
