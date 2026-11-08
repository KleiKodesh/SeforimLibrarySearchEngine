/// Category (facet) counts, Otzaria's search navigation tree, one-snapshot
/// result pages and the count status label: host work over what the engine
/// already returns ([SearchSession.counts], [SearchSession.hits],
/// [SearchInfo.truncated]) and the category / book tables of seforim.db.
///
/// Ported from Otzaria (`lib/search/utils/facet_helper.dart`,
/// `search_catalogue_order_helper.dart`, `view/search_navigation_tree.dart`,
/// `library/models/library.dart` normalizeBookTitle). Deviations are named
/// where they occur.
library;

import 'package:seforim_search/seforim_search.dart';

import 'grouping.dart';
import 'seforim_db.dart';

/// Otzaria `SearchCatalogueOrderHelper._orderedTopCategories`: the fixed
/// order of the top categories (titles with gershayim / geresh folded to
/// ASCII `"` / `'`). Others follow, by their own order.
const orderedTopCategories = [
  '\u05ea\u05e0"\u05da',
  '\u05de\u05d3\u05e8\u05e9',
  '\u05de\u05e9\u05e0\u05d4',
  '\u05ea\u05dc\u05de\u05d5\u05d3 \u05d1\u05d1\u05dc\u05d9',
  '\u05ea\u05dc\u05de\u05d5\u05d3 \u05d9\u05e8\u05d5\u05e9\u05dc\u05de\u05d9',
  '\u05ea\u05d5\u05e1\u05e4\u05ea\u05d0',
  '\u05d4\u05dc\u05db\u05d4',
  '\u05e9\u05d5"\u05ea',
  '\u05e7\u05d1\u05dc\u05d4',
  '\u05e1\u05d3\u05e8 \u05d4\u05ea\u05e4\u05d9\u05dc\u05d4',
  '\u05de\u05d7\u05e9\u05d1\u05ea \u05d9\u05e9\u05e8\u05d0\u05dc',
  '\u05d7\u05e1\u05d9\u05d3\u05d5\u05ea',
  '\u05e1\u05e4\u05e8\u05d9 \u05de\u05d5\u05e1\u05e8',
  '\u05de\u05d9\u05dc\u05d5\u05e0\u05d9\u05dd \u05d5\u05e1\u05e4\u05e8\u05d9 \u05d9\u05e2\u05e5',
  '\u05dc\u05d9\u05de\u05d5\u05d3 \u05d9\u05d5\u05de\u05d9',
  '\u05e1\u05e4\u05e8\u05d5\u05ea \u05e2\u05d6\u05e8',
  '\u05d1\u05d9\u05ea \u05e9\u05e0\u05d9',
];

/// A category of the catalog tree (seforim.db `category`).
class CatalogCategory {
  CatalogCategory(this.id, this.title, this.order, this.parent);
  final int id;
  final String title;

  /// `orderIndex` (0 when the DB has none).
  final int order;

  /// Null for a top category (the library root is not a row).
  final CatalogCategory? parent;
  final List<CatalogCategory> subCategories = [];
  final List<CatalogBook> books = [];

  /// Otzaria `Category.path`: `/` + the titles from the top category down,
  /// joined with `/` (titles are not escaped, as in Otzaria).
  late final String path = parent == null ? '/$title' : '${parent!.path}/$title';
}

/// A book of the catalog (seforim.db `book`).
class CatalogBook {
  CatalogBook(this.id, this.title, this.order, this.category);
  final int id;
  final String title;
  final int order;

  /// Null when the book has no (known) category.
  final CatalogCategory? category;

  /// Otzaria `FacetHelper.buildBookFacet`: the category path + `/` + the
  /// book key. Deviation: the key is the book id (Otzaria's BookIdentity key
  /// mixes title, source and file path; seforim.db ids are already unique).
  late final String facet = category == null ? '/$id' : '${category!.path}/$id';
}

/// The library: the category tree and its books, read once from seforim.db.
class Catalog {
  Catalog(this.topCategories, this.books);

  /// The library root's sub-categories (Otzaria's Library children).
  final List<CatalogCategory> topCategories;

  /// Every book by id.
  final Map<int, CatalogBook> books;

  /// Books with no known category (Otzaria `offTreeBooks`): listed under the
  /// root, counted in the root only.
  Iterable<CatalogBook> get offTreeBooks => books.values.where((b) => b.category == null);

  /// Reads `category` (id, parentId, title, orderIndex) and `book` (id,
  /// categoryId, title, orderIndex); a missing `orderIndex` reads as 0.
  static Catalog read(SeforimDb db) {
    final d = db.db;
    bool has(String table, String col) => d.select('PRAGMA table_info($table)').any((r) => r['name'] == col);
    final catOrder = has('category', 'orderIndex') ? 'orderIndex' : '0';
    final bookOrder = has('book', 'orderIndex') ? 'orderIndex' : '0';
    return fromRows(
      [
        for (final r in d.select('SELECT id, parentId, title, $catOrder AS o FROM category'))
          (r['id'] as int, r['parentId'] as int?, (r['title'] as String?) ?? '', (r['o'] as int?) ?? 0)
      ],
      [
        for (final r in d.select('SELECT id, categoryId, title, $bookOrder AS o FROM book'))
          (r['id'] as int, r['categoryId'] as int?, (r['title'] as String?) ?? '', (r['o'] as int?) ?? 0)
      ],
    );
  }

  /// Builds the tree from rows: categories (id, parentId, title, order) and
  /// books (id, categoryId, title, order). A parent that is not a known
  /// category makes a top category; a cycle is cut where it closes.
  static Catalog fromRows(List<(int, int?, String, int)> categories, List<(int, int?, String, int)> bookRows) {
    final rowById = {for (final c in categories) c.$1: c};
    final built = <int, CatalogCategory>{};
    final top = <CatalogCategory>[];
    CatalogCategory build(int id, Set<int> visiting) {
      final done = built[id];
      if (done != null) return done;
      final r = rowById[id]!;
      visiting.add(id);
      final p = r.$2;
      final parent =
          p != null && rowById.containsKey(p) && !visiting.contains(p) ? build(p, visiting) : null;
      final c = CatalogCategory(id, r.$3, r.$4, parent);
      built[id] = c;
      (parent == null ? top : parent.subCategories).add(c);
      return c;
    }

    for (final c in categories) {
      build(c.$1, <int>{});
    }
    final books = <int, CatalogBook>{};
    for (final r in bookRows) {
      final b = CatalogBook(r.$1, r.$3, r.$4, r.$2 == null ? null : built[r.$2]);
      books[b.id] = b;
      b.category?.books.add(b);
    }
    return Catalog(top, books);
  }
}

// -- Facet counts (Otzaria FacetHelper) --

/// Otzaria `FacetHelper.incrementFacet`.
void _increment(Map<String, int> counts, String facet, int delta) => counts[facet] = (counts[facet] ?? 0) + delta;

/// Otzaria `FacetHelper.incrementFacetWithAncestors`: the root `/` and every
/// prefix of [categoryPath].
void _incrementWithAncestors(Map<String, int> counts, String categoryPath, int delta) {
  if (categoryPath.isEmpty) return;
  final normalized = categoryPath.startsWith('/') ? categoryPath : '/$categoryPath';
  _increment(counts, '/', delta);
  var current = '';
  for (final part in normalized.split('/').where((p) => p.isNotEmpty)) {
    current = '$current/$part';
    _increment(counts, current, delta);
  }
}

/// Otzaria `FacetHelper.buildFacetCountsFromBookCounts`: the engine's
/// per-book counts summed up the category tree. Keys: `/` (all), each
/// category path, each book facet ([CatalogBook.facet]). Books unknown to
/// the catalog are skipped, as in Otzaria (they still count in
/// [HitCounts.total]).
Map<String, int> facetCounts(Map<int, int> byBook, Catalog catalog) {
  final counts = <String, int>{};
  final byCategoryPath = <String, int>{};
  for (final e in byBook.entries) {
    if (e.value <= 0) continue;
    final book = catalog.books[e.key];
    if (book == null) continue;
    _increment(counts, book.facet, e.value);
    _increment(byCategoryPath, book.category?.path ?? '', e.value);
  }
  for (final e in byCategoryPath.entries) {
    if (e.key.isNotEmpty) {
      _incrementWithAncestors(counts, e.key, e.value);
    } else {
      _increment(counts, '/', e.value);
    }
  }
  return counts;
}

// -- Navigation tree (Otzaria SearchNavigationTree) --

/// Otzaria `normalizeBookTitle` (library.dart): trim, collapse whitespace,
/// drop `"`, `'`, gershayim U+05F4, geresh U+05F3, `<` and `>`. A title
/// already clean (no edge / double space, no control char, none of those
/// marks, no U+0085 / U+00A0 / U+1680 / U+2000-U+3000 / U+FEFF) is returned
/// as is.
String normalizeBookTitle(String title) {
  if (_isNormalizedBookTitle(title)) return title;
  return title
      .trim()
      .replaceAll(RegExp(r'\s+'), ' ')
      .replaceAll('"', '')
      .replaceAll("'", '')
      .replaceAll('״', '')
      .replaceAll('׳', '')
      .replaceAll('<', '')
      .replaceAll('>', '');
}

bool _isNormalizedBookTitle(String title) {
  final last = title.length - 1;
  for (var i = 0; i <= last; i++) {
    final c = title.codeUnitAt(i);
    if (c == 0x20) {
      if (i == 0 || i == last || title.codeUnitAt(i - 1) == 0x20) return false;
    } else if (c < 0x20 ||
        c == 0x22 ||
        c == 0x27 ||
        c == 0x3C ||
        c == 0x3E ||
        c == 0x05F3 ||
        c == 0x05F4 ||
        c == 0x85 ||
        c == 0xA0 ||
        c == 0x1680 ||
        (c >= 0x2000 && c <= 0x3000) ||
        c == 0xFEFF) {
      return false;
    }
  }
  return true;
}

/// Otzaria `SearchCatalogueOrderHelper.normalizeOrder`: negative orders last.
int normalizeOrder(int order) => order >= 0 ? order : 1000 + order.abs();

/// Otzaria `SearchCatalogueOrderHelper.topCategoryOrder`.
int topCategoryOrder(CatalogCategory c) {
  final idx = orderedTopCategories.indexOf(c.title.replaceAll('״', '"').replaceAll('׳', "'"));
  return idx >= 0 ? idx : orderedTopCategories.length + c.order;
}

/// Kind of a [NavRow].
enum NavRowKind { root, category, book }

/// One row of the flattened tree (Otzaria `_FlatRow`).
class NavRow {
  NavRow(this.kind, this.level, this.count,
      {this.category, this.book, this.isExpanded = false, this.hasChildren = false});
  final NavRowKind kind;

  /// 0 for the root header, 1 for a top category, one more per level.
  final int level;
  final int count;
  final CatalogCategory? category;
  final CatalogBook? book;
  final bool isExpanded;
  final bool hasChildren;

  /// Otzaria `_markGroupBoundaries`: the first / last row below the root.
  bool isGroupStart = false, isGroupEnd = false;

  /// The facet this row selects: `/`, a category path or a book facet.
  String get facet => switch (kind) {
        NavRowKind.root => '/',
        NavRowKind.category => category!.path,
        NavRowKind.book => book!.facet,
      };

  String get title => switch (kind) {
        NavRowKind.root => '',
        NavRowKind.category => category!.title,
        NavRowKind.book => book!.title,
      };
}

/// Otzaria's search navigation tree, flattened to rows, over [counts]
/// ([facetCounts]).
///
/// As Otzaria: a category or book with no hits is hidden unless it is
/// selected or (category) leads to a selection; a category expands when
/// [expansion] says so, else when it leads to a selection, and only when it
/// has visible children; top categories go by [topCategoryOrder], nested
/// ones by [normalizeOrder], books by order. With a [filterQuery] of two
/// chars or more the tree turns into a flat list of the books with hits
/// whose [normalizeBookTitle] (lower-cased) contains the normalized query,
/// in catalog order, all at level 0.
///
/// Deviations: no dimension facets (`/base`, `/era/`, `/author/`) and no
/// extra root categories (external libraries); sorting ties break by id
/// (Dart's sort is not stable); books are deduplicated by id only (ids are
/// unique); the off-tree books stand for the Library root's own books.
List<NavRow> navigationRows(
  Catalog catalog,
  Map<String, int> counts, {
  Set<String> selectedFacets = const {},
  Map<String, bool> expansion = const {},
  String filterQuery = '',
}) {
  bool isSelected(String f) => selectedFacets.contains(f);
  bool leadsToSelection(String path) => selectedFacets.any((f) => f.startsWith('$path/'));
  int countOf(String f) => counts[f] ?? 0;

  List<CatalogCategory> sortedSubs(CatalogCategory? parent) {
    int key(CatalogCategory c) => parent == null ? topCategoryOrder(c) : normalizeOrder(c.order);
    return (parent == null ? catalog.topCategories : parent.subCategories).toList()
      ..sort((a, b) {
        final k = key(a).compareTo(key(b));
        return k != 0 ? k : a.id.compareTo(b.id);
      });
  }

  List<CatalogBook> sortedBooks(CatalogCategory? c) => (c == null ? catalog.offTreeBooks : c.books).toList()
    ..sort((a, b) {
      final k = a.order.compareTo(b.order);
      return k != 0 ? k : a.id.compareTo(b.id);
    });

  if (filterQuery.length >= 2) {
    final query = normalizeBookTitle(filterQuery).toLowerCase();
    final all = <CatalogBook>[];
    void collect(CatalogCategory? c) {
      all.addAll(sortedBooks(c));
      for (final s in sortedSubs(c)) {
        collect(s);
      }
    }

    collect(null);
    return [
      for (final b in all)
        if (normalizeBookTitle(b.title).toLowerCase().contains(query) && countOf(b.facet) != 0)
          NavRow(NavRowKind.book, 0, countOf(b.facet), book: b)
    ];
  }

  bool hasVisibleChildren(CatalogCategory c) {
    for (final s in c.subCategories) {
      if (countOf(s.path) > 0 || leadsToSelection(s.path) || isSelected(s.path)) return true;
    }
    for (final b in c.books) {
      if (countOf(b.facet) > 0 || isSelected(b.facet)) return true;
    }
    return false;
  }

  final rows = <NavRow>[NavRow(NavRowKind.root, 0, countOf('/'))];
  void flatten(CatalogCategory? c, int level) {
    for (final s in sortedSubs(c)) {
      final count = countOf(s.path);
      final leads = leadsToSelection(s.path);
      if (count == 0 && !leads && !isSelected(s.path)) continue;
      final hasChildren = hasVisibleChildren(s);
      final expanded = hasChildren && (expansion[s.path] ?? leads);
      rows.add(NavRow(NavRowKind.category, level + 1, count,
          category: s, isExpanded: expanded, hasChildren: hasChildren));
      if (expanded) flatten(s, level + 1);
    }
    for (final b in sortedBooks(c)) {
      final count = countOf(b.facet);
      if (count == 0 && !isSelected(b.facet)) continue;
      rows.add(NavRow(NavRowKind.book, level + 1, count, book: b));
    }
  }

  flatten(null, 0);
  final below = [for (final r in rows) if (r.level >= 1) r];
  if (below.isNotEmpty) {
    below.first.isGroupStart = true;
    below.last.isGroupEnd = true;
  }
  return rows;
}

// -- Count status --

/// How final a page's numbers are (Dart host label).
///
/// - [exact]: every hit counted ([HitCounts.exact]) and no expansion cap hit.
/// - [partial]: counting not finished ([HitCounts.validated] below the hit
///   count): the totals cover the hits validated so far.
/// - [truncated]: an expansion cap was hit ([SearchInfo.truncated]): the hit
///   set itself is incomplete, whatever the count state.
enum CountStatus { exact, partial, truncated }

CountStatus countStatus(HitCounts counts, {required bool truncated}) =>
    truncated ? CountStatus.truncated : (counts.exact ? CountStatus.exact : CountStatus.partial);

// -- Page grouping: book, then section --

/// The hits of one book on a page, grouped by section (`line_toc`), both in
/// hit order (a group's first member is its best one).
class BookHits {
  BookHits(this.bookId, this.sections);
  final int bookId;

  /// Section groups; keys as [groupKey] with [Grouping.sameSection].
  final List<HitGroup<PlacedHit>> sections;
}

/// Groups a page by book, then by section ([sections]: line id -> TOC entry,
/// from [SeforimDb.sectionsOf]; a line without one is its own group).
List<BookHits> groupPage(List<PlacedHit> page, Map<int, int> sections) => [
      for (final g in groupInOrder<PlacedHit>(page, (h) => (0, h.bookId)))
        BookHits(
            g.key.$2,
            groupInOrder<PlacedHit>(g.members,
                (h) => groupKey(Grouping.sameSection, tocEntryId: sections[h.lineId] ?? 0, lineId: h.lineId))),
    ];

// -- One snapshot --

/// Counts every hit when the count needs texts (FtsLib distance / order,
/// text-level negative, ...): the hits not validated yet are fed in hit
/// order, with their next lines (cross-line), until [HitCounts.exact].
/// No-op when the engine counts without texts.
void completeCounts(SeforimDb db, SearchSession session, {int batch = 4096}) {
  if (!session.countNeedsText) return;
  for (var o = session.counts().validated; o < session.hitCount; o += batch) {
    final hits = session.hits(o, batch);
    final texts =
        db.lineTexts([...hits.map((h) => h.lineId), for (final h in hits) if (h.nextLineId != null) h.nextLineId!]);
    session.countBatchPairs([for (final h in hits) texts[h.lineId] ?? ''],
        [for (final h in hits) h.nextLineId == null ? null : texts[h.nextLineId] ?? '']);
  }
}

/// One search result snapshot (Otzaria's `searchAndCount*` /
/// SearchPageResult): the totals, the category counts and one page of hits,
/// grouped by book then section, taken together.
class SearchPage {
  SearchPage._(this.counts, this.truncated, this.hitCount, this.categoryCounts, this.offset, this.hits, this.groups);

  /// Engine totals (total, byBook, validated, exact).
  final HitCounts counts;

  /// An expansion cap was hit ([SearchInfo.truncated]).
  final bool truncated;

  /// Matching lines before the snippet filters ([SearchSession.hitCount]).
  final int hitCount;

  /// The category counts ([facetCounts] of [counts]' byBook).
  final Map<String, int> categoryCounts;
  final int offset;
  final List<PlacedHit> hits;
  final List<BookHits> groups;

  int get totalCount => counts.total;
  CountStatus get status => countStatus(counts, truncated: truncated);

  /// Takes the snapshot. With [exactCounts] the count is completed first
  /// ([completeCounts]), as Otzaria's searchAndCount; without it the totals
  /// are what the engine has counted so far ([status] says which).
  static SearchPage take(SeforimDb db, SearchSession session, Catalog catalog,
      {int offset = 0, int count = 50, bool exactCounts = true}) {
    if (exactCounts) completeCounts(db, session);
    final c = session.counts();
    final page = session.hits(offset, count);
    return SearchPage._(c, session.info().truncated, session.hitCount, facetCounts(c.byBook, catalog), offset, page,
        groupPage(page, db.sectionsOf(page.map((h) => h.lineId))));
  }
}
