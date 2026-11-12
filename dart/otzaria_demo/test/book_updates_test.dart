// Issue #29: per-book index update (contentHash), crash mid-update, background merge.
import 'dart:convert';
import 'dart:io';

import 'package:otzaria_demo/book_updates.dart';
import 'package:otzaria_demo/seforim_db.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:test/test.dart';

String _nativePath() {
  final env = Platform.environment['SEFORIM_SEARCH_NATIVE'];
  if (env != null && env.isNotEmpty) return env;
  final name = Platform.isWindows
      ? 'seforim_search_native.dll'
      : Platform.isMacOS
          ? 'libseforim_search_native.dylib'
          : 'libseforim_search_native.so';
  return '../../target/release/$name';
}

void main() {
  late Directory dir;
  late String dbPath;

  void sql(String s, [List<Object?> p = const []]) {
    final w = sqlite3.open(dbPath);
    w.execute(s, p);
    w.close();
  }

  // the demo's build: catalog order, positions from 0, payload as bin/otzaria_demo.dart
  void build(SeforimDb db, String idx) {
    final index = SeforimIndex.create(idx);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    var position = 0;
    for (final b in db.catalogBookOrder()) {
      for (final l in db.bookLines(b)) {
        w.add(LineDocument(lineId: l.id, position: position++, bookId: l.bookId, text: l.text));
      }
    }
    w.commit('{"lines":$position,"db":"${db.fingerprint()}","notes":0}');
    w.mergeAndClose();
    index.close();
  }

  List<int> hits(SeforimIndex index, String q) {
    final s = index.search(SearchOptions(q));
    return [for (final h in s.hits(0, 1000)) h.lineId];
  }

  List<(int, int)> positions(SeforimIndex index, String q) {
    final s = index.search(SearchOptions(q));
    return [for (final h in s.hits(0, 1000)) (h.position, h.lineId)];
  }

  setUp(() {
    SeforimNative.load(_nativePath());
    dir = Directory.systemTemp.createTempSync('book_updates_test');
    dbPath = '${dir.path}/seforim.db';
    final w = sqlite3.open(dbPath);
    w.execute('CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER, '
        'orderIndex INTEGER)');
    w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER, '
        'contentHash INTEGER)');
    w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content, '
        'heRef TEXT, tocEntryId INTEGER)');
    w.execute("INSERT INTO category VALUES (1, NULL, 'c', 0, 1)");
    // catalog order: book 2, book 1, book 3
    w.execute("INSERT INTO book VALUES (1, 1, 'one', 2, 100), (2, 1, 'two', 1, 200), (3, 1, 'three', 3, 300)");
    var id = 1;
    for (final b in [1, 2, 3]) {
      for (var k = 0; k < 3; k++) {
        w.execute('INSERT INTO line VALUES (?, ?, ?, ?, NULL, NULL)', [id++, b, k, 'alpha b$b']);
      }
    }
    w.close();
  });

  tearDown(() {
    try {
      dir.deleteSync(recursive: true);
    } catch (_) {}
  });

  test('changed, grown, removed books; crash mid-update; background merge', () {
    final idx = '${dir.path}/idx';
    var db = SeforimDb.open(dbPath);
    build(db, idx);
    var index = SeforimIndex.open(idx);
    // adoption: the build's payload gets the per-book record
    var r = updateChangedBooks(index, db);
    expect(r.applicable, isTrue);
    expect(jsonDecode(index.info().payload!)['books'], isNotNull);
    r = updateChangedBooks(index, db);
    expect(r.changed + r.added + r.removed, 0);

    // book 1 changed, same count: only it is rewritten, positions kept
    final before = positions(index, 'alpha');
    sql("UPDATE line SET content = 'alpha omega' WHERE id = 2; UPDATE book SET contentHash = 101 WHERE id = 1");
    r = updateChangedBooks(index, db);
    expect((r.changed, r.repack, r.linesWritten), (1, false, 3));
    index.waitBackgroundMerge();
    expect(positions(index, 'alpha'), before);
    expect(hits(index, 'omega'), [2]);

    // book 2 (first) grows: repack, books after it renumbered densely
    sql("INSERT INTO line VALUES (10, 2, 3, 'alpha psi', NULL, NULL); UPDATE book SET contentHash = 201 WHERE id = 2");
    r = updateChangedBooks(index, db);
    expect((r.changed, r.repack, r.mergeStarted), (1, true, true));
    index.waitBackgroundMerge();
    expect(index.backgroundMergePending, isFalse);
    expect(positions(index, 'alpha'),
        [(0, 4), (1, 5), (2, 6), (3, 10), (4, 1), (5, 2), (6, 3), (7, 7), (8, 8), (9, 9)]);

    // crash mid-update: books 1 and 3 change, one commit per book, stop after the first
    sql("UPDATE line SET content = 'rho' WHERE id IN (3, 9); "
        'UPDATE book SET contentHash = contentHash + 1 WHERE id IN (1, 3)');
    var calls = 0;
    expect(
        () => updateChangedBooks(index, db, linesPerCommit: 1, cancelled: () => calls++ == 1),
        throwsA(isA<BookUpdateCancelled>()));
    expect(jsonDecode(index.info().payload!)['pending'], [3]);
    expect(hits(index, 'rho'), [3]); // book 1 committed, book 3 whole and old
    index.close();
    db.close();
    // restart: open the index again (the hosts' app start), then update
    db = SeforimDb.open(dbPath);
    index = SeforimIndex.open(idx);
    r = updateChangedBooks(index, db);
    expect(r.changed, 1);
    index.waitBackgroundMerge();
    expect(hits(index, 'rho'), [3, 9]);
    expect(jsonDecode(index.info().payload!)['pending'], isNull);

    // a removed book: its lines go, the next book moves up
    sql('DELETE FROM line WHERE bookId = 2; DELETE FROM book WHERE id = 2');
    r = updateChangedBooks(index, db);
    expect((r.removed, r.repack), (1, true));
    index.waitBackgroundMerge();
    expect(positions(index, 'alpha'), [(0, 1), (1, 2), (3, 7), (4, 8)]); // line 3 (rho) holds position 2
    index.close();
    db.close();
  });

  test('a book moved in the tree with a same-count change: repack, not overlap', () {
    final idx = '${dir.path}/idx';
    final db = SeforimDb.open(dbPath);
    build(db, idx);
    final index = SeforimIndex.open(idx);
    updateChangedBooks(index, db); // adopt
    // book 3 moves first; book 1 changes with the same line count
    sql("UPDATE book SET orderIndex = 0 WHERE id = 3; UPDATE line SET content = 'alpha z' WHERE id = 1; "
        'UPDATE book SET contentHash = 101 WHERE id = 1');
    final r = updateChangedBooks(index, db);
    expect((r.changed, r.repack), (1, true));
    index.waitBackgroundMerge();
    expect(positions(index, 'alpha'),
        [(0, 7), (1, 8), (2, 9), (3, 4), (4, 5), (5, 6), (6, 1), (7, 2), (8, 3)]);
    index.close();
    db.close();
  });

  test('a DB without contentHash or an index without the record and a changed DB: not applicable', () {
    final idx = '${dir.path}/idx';
    final db = SeforimDb.open(dbPath);
    build(db, idx);
    final index = SeforimIndex.open(idx);
    // the file stamp moves: the build payload cannot tell which books changed
    sql("UPDATE line SET content = 'x' WHERE id = 1");
    File(dbPath).setLastModifiedSync(DateTime.now().add(const Duration(seconds: 5)));
    expect(updateChangedBooks(index, db).applicable, isFalse);
    index.close();
    db.close();
  });
}
