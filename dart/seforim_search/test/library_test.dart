// The host library (seforim_db.dart): index build from seforim.db with
// progress, the ready-made search (texts, snippets, highlights), counts,
// find in a book, line locations.
import 'dart:io';

import 'package:seforim_search/seforim_db.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:sqlite3/sqlite3.dart';
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
  late String dbPath, idx;
  final native = _nativePath();

  setUpAll(() {
    SeforimNative.load(native);
    tmp = Directory.systemTemp.createTempSync('se_dart_library_');
    dbPath = '${tmp.path}/seforim.db';
    idx = '${tmp.path}/idx';
    final w = sqlite3.open(dbPath);
    w.execute('CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER, '
        'orderIndex INTEGER)');
    w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER, '
        'contentHash INTEGER)');
    w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content, '
        'heRef TEXT, tocEntryId INTEGER)');
    w.execute("INSERT INTO category VALUES (1, NULL, 'c', 0, 1)");
    // catalog order: book 2 then book 1
    w.execute("INSERT INTO book VALUES (1, 1, 'one', 2, 100), (2, 1, 'two', 1, 200)");
    var id = 1;
    for (final b in [1, 2]) {
      for (var k = 0; k < 5; k++) {
        w.execute('INSERT INTO line VALUES (?, ?, ?, ?, ?, NULL)',
            [id++, b, k, 'w$k alpha beta gamma delta epsilon zeta eta theta alpha', 'ref $b:$k']);
      }
    }
    w.close();
  });

  tearDownAll(() {
    try {
      tmp.deleteSync(recursive: true);
    } catch (_) {}
  });

  test('build with progress, then search, counts, find in book', () async {
    expect(SeforimLibrary.needsBuild(idx), isTrue);
    final progress = await SeforimLibrary.buildOrUpdateIndex(dbPath, idx, nativePath: native).toList();
    expect(progress.last.stage, IndexBuildStage.done);
    expect(progress.last.done, 10);
    expect(progress.any((p) => p.stage == IndexBuildStage.indexing && p.total == 10), isTrue);
    expect(SeforimLibrary.needsBuild(idx), isFalse);

    // nothing changed: the update path, no rebuild
    final again = await SeforimLibrary.buildOrUpdateIndex(dbPath, idx, nativePath: native).toList();
    expect(again.first.stage, IndexBuildStage.updating);
    expect(again.last.stage, IndexBuildStage.done);

    final lib = await SeforimLibrary.open(idx, dbPath: dbPath, nativePath: native);
    try {
      final pages = await lib.search(SearchOptions('w3'), pageSize: 1).toList();
      final hits = [for (final p in pages) ...p.results];
      expect(hits.map((h) => h.lineId), [9, 4]); // catalog order: book 2 first
      expect(pages.last.complete, isTrue);
      expect(pages.last.hitCount, 2);
      final h = hits.first;
      expect(h.bookTitle, 'two');
      expect(h.lineIndex, 3);
      expect(h.reference, 'ref 2:3');
      expect(h.text, startsWith('w3 alpha'));
      expect(h.highlights, [(0, 2)]);

      final c = await lib.counts(SearchOptions('alpha'));
      expect(c.exact, isTrue);
      expect(c.total, 10);
      expect(c.byBook, {1: 5, 2: 5});

      final found = await lib.findInBook(1, ['alpha']);
      expect(found.map((m) => m.lineIndex), [0, 1, 2, 3, 4]);
      expect(found.first.matches.length, 2);

      expect(lib.db.lineLocations([7]), {7: (bookId: 2, lineIndex: 1)});
    } finally {
      await lib.close();
    }
  });
}
