// The host library (seforim_db.dart): index build from seforim.db with
// progress, the ready-made search (texts, snippets, highlights), counts,
// find in a book, line locations.
import 'dart:io';

import 'package:seforim_search_sqlite/seforim_search_sqlite.dart';
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
        w.execute('INSERT INTO line VALUES (?, ?, ?, ?, ?, ?)',
            [id++, b, k, 'w$k alpha beta gamma delta epsilon zeta eta theta alpha', 'ref $b:$k', k < 3 ? 10 + b : null]);
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
    final progress = await SeforimLibrary.buildOrUpdateIndex(SeforimDb.opener(dbPath), idx, nativePath: native).toList();
    expect(progress.last.stage, IndexBuildStage.done);
    expect(progress.last.done, 10);
    expect(progress.any((p) => p.stage == IndexBuildStage.indexing && p.total == 10), isTrue);
    expect(SeforimLibrary.needsBuild(idx), isFalse);

    // nothing changed: the update path, no rebuild
    final again = await SeforimLibrary.buildOrUpdateIndex(SeforimDb.opener(dbPath), idx, nativePath: native).toList();
    expect(again.first.stage, IndexBuildStage.updating);
    expect(again.last.stage, IndexBuildStage.done);

    final lib = await openSeforimLibrary(idx, dbPath: dbPath, nativePath: native);
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

      // load more + limit + counts from the same search
      final more = await lib.search(SearchOptions('alpha'), pageSize: 2, offset: 3, limit: 4, withCounts: true).toList();
      expect([for (final p in more) ...p.results].map((h) => h.position), [3, 4, 5, 6]);
      expect(more.last.complete, isTrue);
      expect(more.last.counts!.total, 10);
      expect(more.last.counts!.byBook, {1: 5, 2: 5});

      // matched terms for highlighting; all-matches marks every occurrence
      final all = await lib.search(SearchOptions('alpha'), limit: 1, matchMode: SnippetMatchMode.allMatches).toList();
      expect(all.expand((p) => p.terms), contains('alpha'));
      expect(all.first.results.first.highlights.length, 2);

      // grouping by section: book 2 lines 0-2 (section 12) are one group
      final g = await lib.search(SearchOptions('alpha'), grouping: Grouping.sameSection, withCounts: true).toList();
      final reps = [for (final p in g) ...p.results];
      expect(g.last.groupCount, 6); // 2 sections + 4 lines without one
      expect(reps.first.groupSize, 3);
      expect(reps.first.siblings.map((x) => x.lineIndex), [1, 2]);
      expect(g.last.counts!.total, 10);

      final c = await lib.counts(SearchOptions('alpha'));
      expect(c.exact, isTrue);
      expect(c.total, 10);
      expect(c.byBook, {1: 5, 2: 5});

      final found = await lib.findInBook(1, ['alpha']);
      expect(found.map((m) => m.lineIndex), [0, 1, 2, 3, 4]);
      expect(found.first.matches.length, 2);

      expect(lib.source.lineLocations([7]), {7: (bookId: 2, lineIndex: 1)});

      // user documents (issue #44): a text file and a PDF's pages, searched
      // with the library, texts and pages from the user index
      await lib.openUserIndex('${tmp.path}/user', create: true);
      final txt = UserDocuments.fromText('c:/notes.txt', 'first line\nomega alpha here\n<h2>head</h2>\nlast');
      final pdf = UserDocuments.fromPdfPages('c:/a.pdf', 's1',
          const [PdfPageText(0, 'cover page'), PdfPageText(4, 'x\nomega on page five')]);
      await lib.updateUserDocuments(put: [txt, pdf]);
      final docs = await lib.documents();
      expect(docs.map((d) => d.key), containsAll(['c:/notes.txt', 'c:/a.pdf']));
      final u = [for (final p in await lib.search(SearchOptions('omega')).toList()) ...p.results];
      expect(u.length, 2);
      expect(u.every((h) => h.userSource != null), isTrue);
      final onPdf = u.firstWhere((h) => h.userSource!.key == 'c:/a.pdf');
      expect(onPdf.userSource!.locator, 4);
      expect(onPdf.text, 'omega on page five');
      expect(onPdf.bookTitle, 'c:/a.pdf');
      expect(onPdf.highlights, [(0, 5)]);
      final onTxt = u.firstWhere((h) => h.userSource!.key == 'c:/notes.txt');
      expect(onTxt.lineIndex, 1);
      expect(onTxt.before, 'first line'); // short row: same-document context
      final libOnly = [for (final p in await lib.search(SearchOptions('omega', sources: SearchSources.library)).toList()) ...p.results];
      expect(libOnly, isEmpty);
      await lib.updateUserDocuments(delete: ['c:/notes.txt'], threads: 1, lowPriority: true); // economy mode
      final left = [for (final p in await lib.search(SearchOptions('omega')).toList()) ...p.results];
      expect(left.single.userSource!.key, 'c:/a.pdf');
      // a grouped hit's sibling in another PDF keeps its page (locator);
      // identical text needs enough Hebrew letters to group
      const twin = '\u05d1\u05e8\u05d0\u05e9\u05d9\u05ea \u05d1\u05e8\u05d0 \u05d0\u05dc\u05d4\u05d9\u05dd \u05d0\u05ea \u05d4\u05e9\u05de\u05d9\u05dd';
      await lib.updateUserDocuments(put: [
        UserDocuments.fromPdfPages('c:/b.pdf', 's2', [PdfPageText(2, 'sigma $twin')]),
        UserDocuments.fromPdfPages('c:/c.pdf', 's3', [PdfPageText(7, 'sigma $twin')]),
      ]);
      final grouped = [
        for (final p in await lib.search(SearchOptions('sigma'), grouping: Grouping.identicalText).toList()) ...p.results
      ];
      expect(grouped.single.userSource!.locator, 2);
      expect(grouped.single.siblings.single.userSource!.key, 'c:/c.pdf');
      expect(grouped.single.siblings.single.userSource!.locator, 7);
      final mixed = await lib.counts(SearchOptions('alpha'));
      expect(mixed.total, 10); // the deleted document is gone
    } finally {
      await lib.close();
    }
  });
}
