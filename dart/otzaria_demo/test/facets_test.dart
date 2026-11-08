// Issue #11: category (facet) counts, Otzaria's navigation tree, one-snapshot
// result pages, page grouping by book then section, count status label.
import 'dart:io';

import 'package:otzaria_demo/facets.dart';
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

/// Top category "A" (order 5) > "A1" (order 0) and "A0" (order -1, last);
/// top "B" (order 1); a top category titled with the first entry of
/// [orderedTopCategories] written with gershayim (sorted first).
/// Books: 1 in A1, 2 in A, 4 in A0, 5 in B, 6 in the fixed top category,
/// 3 off-tree (no category).
const _cats = [
  (10, null, 'A', 5),
  (11, 10, 'A1', 0),
  (12, 10, 'A0', -1),
  (20, null, 'B', 1),
  (30, null, 'תנ״ך', 9),
];
const _books = [
  (1, 11, 'Book One', 0),
  (2, 10, 'Book Two', 0),
  (3, null, 'Loose Book', 0),
  (4, 12, 'Book Four', 0),
  (5, 20, 'Book Five', 0),
  (6, 30, 'Book ש״ס Six', 0),
];

void main() {
  final catalog = Catalog.fromRows(_cats, _books);

  test('catalog: paths and book facets', () {
    expect(catalog.books[1]!.facet, '/A/A1/1');
    expect(catalog.books[2]!.facet, '/A/2');
    expect(catalog.books[3]!.facet, '/3');
    expect(catalog.offTreeBooks.map((b) => b.id), [3]);
  });

  test('catalog: a parent cycle is cut, an unknown parent makes a top category', () {
    final c = Catalog.fromRows([(1, 2, 'x', 0), (2, 1, 'y', 0), (3, 99, 'z', 0)], []);
    expect(c.topCategories.map((t) => t.id).toSet(), {2, 3});
    expect(c.topCategories.firstWhere((t) => t.id == 2).subCategories.single.path, '/y/x');
  });

  test('facetCounts: per-book counts summed up the tree (Otzaria buildFacetCountsFromBookCounts)', () {
    final f = facetCounts({1: 2, 2: 3, 3: 1, 4: 0, 99: 5}, catalog);
    expect(f, {
      '/': 6,
      '/A': 5,
      '/A/A1': 2,
      '/A/A1/1': 2,
      '/A/2': 3,
      '/3': 1,
    });
  });

  test('navigation tree: hidden zero rows, order, expansion, selection', () {
    final f = facetCounts({1: 2, 2: 3, 4: 1, 5: 1, 6: 1, 3: 1}, catalog);
    var rows = navigationRows(catalog, f);
    // top: fixed-order category first, then B (1), then A (5); A collapsed
    expect(rows.map((r) => r.facet), ['/', '/תנ״ך', '/B', '/A', '/3']);
    expect(rows.first.count, 9);
    expect(rows[3].hasChildren, isTrue);
    expect(rows[3].isExpanded, isFalse);
    expect(rows[1].isGroupStart, isTrue);
    expect(rows.last.isGroupEnd, isTrue);

    rows = navigationRows(catalog, f, expansion: {'/A': true});
    // nested order: A1 (0) before A0 (-1 -> last), then A's own books
    expect(rows.map((r) => r.facet).toList().sublist(3), ['/A', '/A/A1', '/A/A0', '/A/2', '/3']);
    expect(rows[4].level, 2);

    // a selection below a collapsed category expands the path to it
    rows = navigationRows(catalog, f, selectedFacets: {'/A/A1/1'});
    expect(rows.map((r) => r.facet), contains('/A/A1/1'));

    // a zero-count book shows only when selected
    final f2 = facetCounts({1: 2}, catalog);
    expect(navigationRows(catalog, f2, expansion: {'/A': true}).map((r) => r.facet), isNot(contains('/A/2')));
    expect(navigationRows(catalog, f2, expansion: {'/A': true}, selectedFacets: {'/A/2'}).map((r) => r.facet),
        contains('/A/2'));
  });

  test('navigation tree: filter mode normalizes titles and query (Otzaria normalizeBookTitle)', () {
    final f = facetCounts({1: 2, 2: 3, 6: 1}, catalog);
    // query typed with ASCII quote, title written with gershayim
    var rows = navigationRows(catalog, f, filterQuery: 'ש"ס');
    expect(rows.map((r) => r.book!.id), [6]);
    expect(rows.single.level, 0);
    rows = navigationRows(catalog, f, filterQuery: '  BOOK   ');
    expect(rows.map((r) => r.book!.id), [6, 2, 1]); // catalog order, a category's own books before its subcategories'
    expect(navigationRows(catalog, f, filterQuery: 'b').first.kind, NavRowKind.root); // < 2 chars: tree
  });

  test('normalizeBookTitle: clean titles unchanged, marks and odd spaces removed', () {
    expect(normalizeBookTitle('Book One'), 'Book One');
    expect(normalizeBookTitle(' a  b<c>\'d" ׳״ '), 'a bcd '); // as Otzaria: trimmed before the marks are dropped
  });

  test('countStatus', () {
    expect(countStatus(const HitCounts(3, {}, 3, true), truncated: false), CountStatus.exact);
    expect(countStatus(const HitCounts(1, {}, 1, false), truncated: false), CountStatus.partial);
    expect(countStatus(const HitCounts(3, {}, 3, true), truncated: true), CountStatus.truncated);
  });

  test('groupPage: by book, then section, hit order', () {
    const page = [
      PlacedHit(0, 100, 1),
      PlacedHit(1, 101, 1),
      PlacedHit(2, 200, 2),
      PlacedHit(3, 102, 1),
      PlacedHit(4, 103, 1),
    ];
    final g = groupPage(page, {100: 7, 101: 8, 102: 7, 200: 7});
    expect(g.map((b) => b.bookId), [1, 2]);
    expect([for (final s in g[0].sections) s.members.map((h) => h.lineId).toList()], [
      [100, 102],
      [101],
      [103],
    ]);
    expect(g[1].sections.single.members.single.lineId, 200);
  });

  group('SearchPage on a synthetic seforim.db', () {
    late Directory dir;
    late SeforimDb db;
    late SeforimIndex index;
    // line id, book, line index, section, text
    const lines = [
      (1, 1, 0, 70, 'alpha beta gamma'),
      (2, 1, 1, 70, 'alpha delta beta'),
      (3, 1, 2, 71, 'alpha x x x x x x x x beta'),
      (4, 2, 0, 80, 'beta alpha'),
      (5, 3, 0, 0, 'alpha beta'),
      (6, 5, 0, 90, 'alphabet soup'),
    ];

    setUpAll(() {
      SeforimNative.load(_nativePath());
      dir = Directory.systemTemp.createTempSync('facets_test');
      final w = sqlite3.open('${dir.path}/seforim.db');
      w.execute('CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER, '
          'orderIndex INTEGER)');
      w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER)');
      w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content, '
          'heRef TEXT, tocEntryId INTEGER)');
      w.execute('CREATE TABLE line_toc (lineId INTEGER PRIMARY KEY, tocEntryId INTEGER)');
      for (final c in _cats) {
        w.execute('INSERT INTO category VALUES (?, ?, ?, 0, ?)', [c.$1, c.$2, c.$3, c.$4]);
      }
      for (final b in _books) {
        w.execute('INSERT INTO book VALUES (?, ?, ?, ?)', [b.$1, b.$2, b.$3, b.$4]);
      }
      for (final l in lines) {
        w.execute('INSERT INTO line VALUES (?, ?, ?, ?, NULL, NULL)', [l.$1, l.$2, l.$3, l.$5]);
        if (l.$4 != 0) w.execute('INSERT INTO line_toc VALUES (?, ?)', [l.$1, l.$4]);
      }
      w.close();
      db = SeforimDb.open('${dir.path}/seforim.db');
      index = SeforimIndex.create('${dir.path}/idx');
      final iw = index.openWriter(heapBytes: 30000000, threads: 1);
      var pos = 0;
      for (final l in lines) {
        iw.add(LineDocument(lineId: l.$1, position: pos++, bookId: l.$2, text: l.$5));
      }
      iw.commit('done');
      iw.close();
    });

    tearDownAll(() {
      db.close();
      try {
        dir.deleteSync(recursive: true);
      } catch (_) {}
    });

    test('no text needed: exact at once, category counts, page grouped', () {
      final s = index.search(SearchOptions('alpha beta'));
      final cat = Catalog.read(db);
      final p = SearchPage.take(db, s, cat, count: 3);
      expect(p.status, CountStatus.exact);
      expect(p.totalCount, 5);
      expect(p.categoryCounts['/'], 5);
      expect(p.categoryCounts['/A'], 4);
      expect(p.categoryCounts['/A/A1/1'], 3);
      expect(p.categoryCounts['/3'], 1);
      expect(p.hits.map((h) => h.lineId), [1, 2, 3]);
      expect(p.groups.single.bookId, 1);
      expect([for (final g in p.groups.single.sections) g.members.length], [2, 1]);
    });

    test('distance needs texts: partial without counting, exact after', () {
      final cat = Catalog.read(db);
      final s = index.search(SearchOptions('alpha beta', maxWordDistance: 2));
      expect(s.countNeedsText, isTrue);
      final partial = SearchPage.take(db, s, cat, exactCounts: false);
      expect(partial.status, CountStatus.partial);
      expect(partial.counts.validated, 0);
      final exact = SearchPage.take(db, s, cat);
      expect(exact.status, CountStatus.exact);
      expect(exact.totalCount, 4); // line 3 is too far apart
      expect(exact.counts.byBook, {1: 2, 2: 1, 3: 1});
      expect(exact.categoryCounts['/A/A1'], 2);
      expect(exact.hitCount, 5); // the page reads hits before the filters
    });

    test('expansion cap hit: truncated', () {
      final s = index.search(SearchOptions('alpha*', maxExpandedTerms: 1));
      final p = SearchPage.take(db, s, Catalog.read(db));
      expect(p.truncated, isTrue);
      expect(p.status, CountStatus.truncated);
    });
  });
}
