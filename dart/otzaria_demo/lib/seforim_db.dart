import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:sqlite3/sqlite3.dart';

import 'zstd.dart';

/// Default DB path resolution (see repo CLAUDE.md): Otzaria, then Zayit.
/// (The KitveiHakodesh registry key is Windows/C#-only and skipped here.)
String? defaultDbPath() {
  final appData = Platform.environment['APPDATA'] ??
      (Platform.environment['HOME'] != null ? '${Platform.environment['HOME']}/.local/share' : null);
  if (appData == null) return null;
  for (final p in [
    '$appData/otzaria/books/seforim.db',
    '$appData/io.github.kdroidfilter.seforimapp/databases/seforim.db',
  ]) {
    if (File(p).existsSync()) return p;
  }
  return null;
}

/// One line read from the DB.
class DbLine {
  DbLine(this.id, this.bookId, this.lineIndex, this.tocEntryId, this.text);
  final int id, bookId, lineIndex, tocEntryId;
  final String text;
}

/// Read-only access to seforim.db: schema probe, content codec, catalog
/// order and the line feed.
class SeforimDb {
  SeforimDb._(this.path, this._db, this.splitContent, this.compressed, this._zstd);

  final String path;
  final Database _db;

  /// `line_content` exists and `line` has no `content` column (schema 6+).
  final bool splitContent;

  /// `zstd_dict` exists.
  final bool compressed;
  final ZstdDictDecoder? _zstd;

  static SeforimDb open(String path, {String? zstdLib}) {
    final db = sqlite3.open(path, mode: OpenMode.readOnly);
    db.execute('PRAGMA cache_size=-65536; PRAGMA mmap_size=268435456; PRAGMA temp_store=MEMORY;');
    bool table(String t) =>
        db.select("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", [t]).isNotEmpty;
    final lineHasContent = db.select('PRAGMA table_info(line)').any((r) => r['name'] == 'content');
    final split = table('line_content') && !lineHasContent;
    final compressed = table('zstd_dict');
    ZstdDictDecoder? dec;
    if (compressed) {
      final dicts = [
        for (final r in db.select('SELECT dict FROM zstd_dict'))
          if (r['dict'] != null) r['dict'] as Uint8List,
      ];
      if (dicts.isEmpty) throw StateError('zstd_dict table is empty: compressed content cannot be decoded');
      dec = ZstdDictDecoder(Zstd.load(zstdLib), dicts);
    }
    return SeforimDb._(path, db, split, compressed, dec);
  }

  String get _contentCol => splitContent ? 'lc.content' : 'l.content';
  String get _contentJoin => splitContent ? ' LEFT JOIN line_content lc ON lc.id = l.id' : '';

  /// Decide by runtime value type, never via SQL (SQLite coerces blobs).
  String decode(Object? v) {
    if (v == null) return '';
    if (v is String) {
      if (_zstd != null && v.length >= 8 && v.startsWith('KLUv')) {
        try {
          return utf8.decode(_zstd.decode(base64.decode(v)), allowMalformed: true);
        } catch (_) {/* genuinely text */}
      }
      return v;
    }
    if (v is Uint8List) {
      final z = _zstd ?? (throw StateError('blob content but no zstd_dict'));
      return utf8.decode(z.decode(v), allowMalformed: true);
    }
    return v.toString();
  }

  /// Book ids in catalog order: categories by (orderIndex<0, orderIndex),
  /// in each folder its base books first, then subfolders, then the rest
  /// (books by orderIndex). Books outside the tree go last.
  List<int> catalogBookOrder() {
    final hasCatOrder = db.select('PRAGMA table_info(category)').any((r) => r['name'] == 'orderIndex');
    final hasBase = db.select('PRAGMA table_info(book)').any((r) => r['name'] == 'isBaseBook');
    final cats = db.select('SELECT id, parentId FROM category ORDER BY level'
        '${hasCatOrder ? ', CASE WHEN orderIndex < 0 THEN 1 ELSE 0 END, orderIndex' : ''}');
    final known = {for (final r in cats) r['id'] as int};
    final children = <int, List<int>>{};
    final roots = <int>[];
    for (final r in cats) {
      final p = r['parentId'] as int?;
      if (p != null && known.contains(p)) {
        (children[p] ??= []).add(r['id'] as int);
      } else {
        roots.add(r['id'] as int);
      }
    }
    final books = <int, List<(int, bool)>>{};
    final orphans = <int>[];
    for (final r in db.select('SELECT id, categoryId, ${hasBase ? 'isBaseBook' : '0'} AS b FROM book ORDER BY orderIndex, id')) {
      final c = r['categoryId'] as int?;
      final e = (r['id'] as int, (r['b'] as int? ?? 0) != 0);
      if (c != null && known.contains(c)) {
        (books[c] ??= []).add(e);
      } else {
        orphans.add(e.$1);
      }
    }
    final order = <int>[];
    void walk(int c) {
      final own = books[c] ?? const [];
      order.addAll([for (final b in own) if (b.$2) b.$1]);
      for (final ch in children[c] ?? const <int>[]) {
        walk(ch);
      }
      order.addAll([for (final b in own) if (!b.$2) b.$1]);
    }

    roots.forEach(walk);
    order.addAll(orphans);
    return order;
  }

  Database get db => _db;

  /// One book's lines in reading order.
  Iterable<DbLine> bookLines(int bookId) sync* {
    final stmt = _db.prepare('SELECT l.id, l.lineIndex, COALESCE(l.tocEntryId, 0), $_contentCol FROM line l'
        '$_contentJoin WHERE l.bookId = ? ORDER BY l.lineIndex, l.id');
    try {
      final cur = stmt.selectCursor([bookId]);
      while (cur.moveNext()) {
        final r = cur.current;
        yield DbLine(r.columnAt(0) as int, bookId, r.columnAt(1) as int, r.columnAt(2) as int, decode(r.columnAt(3)));
      }
    } finally {
      stmt.close();
    }
  }

  /// Texts of the given line ids.
  Map<int, String> lineTexts(Iterable<int> ids) {
    final out = <int, String>{};
    final list = ids.toList();
    for (var i = 0; i < list.length; i += 500) {
      final chunk = list.sublist(i, i + 500 > list.length ? list.length : i + 500);
      final rs = _db.select(
          'SELECT l.id, $_contentCol FROM line l$_contentJoin WHERE l.id IN (${List.filled(chunk.length, '?').join(',')})',
          chunk);
      for (final r in rs) {
        out[r.columnAt(0) as int] = decode(r.columnAt(1));
      }
    }
    return out;
  }

  /// Lines of [bookId] with lineIndex in [from, to], keyed by lineIndex.
  Map<int, String> linesAround(int bookId, int from, int to) {
    final rs = _db.select(
        'SELECT l.lineIndex, $_contentCol FROM line l$_contentJoin WHERE l.bookId = ? AND l.lineIndex BETWEEN ? AND ?',
        [bookId, from, to]);
    return {for (final r in rs) r.columnAt(0) as int: decode(r.columnAt(1))};
  }

  /// Each line's lineIndex (its place in its book), from the DB: the engine
  /// returns line ids only.
  Map<int, int> lineIndexesOf(Iterable<int> ids) {
    final out = <int, int>{};
    final list = ids.toList();
    for (var i = 0; i < list.length; i += 900) {
      final chunk = list.sublist(i, i + 900 > list.length ? list.length : i + 900);
      final rs = _db.select(
          'SELECT id, lineIndex FROM line WHERE id IN (${List.filled(chunk.length, '?').join(',')})', chunk);
      for (final r in rs) {
        out[r.columnAt(0) as int] = r.columnAt(1) as int;
      }
    }
    return out;
  }

  /// Each line's section (TOC entry) for grouping, straight from the DB: the
  /// engine only returns line ids. `line_toc` is the authoritative map
  /// (`line.tocEntryId` is NULL for most lines). Lines without one are absent.
  Map<int, int> sectionsOf(Iterable<int> ids) {
    final hasLineToc =
        _db.select("SELECT 1 FROM sqlite_master WHERE type='table' AND name='line_toc'").isNotEmpty;
    final out = <int, int>{};
    final list = ids.toList();
    for (var i = 0; i < list.length; i += 900) {
      final chunk = list.sublist(i, i + 900 > list.length ? list.length : i + 900);
      final marks = List.filled(chunk.length, '?').join(',');
      final rs = _db.select(
          hasLineToc
              ? 'SELECT lineId, tocEntryId FROM line_toc WHERE lineId IN ($marks)'
              : 'SELECT id, tocEntryId FROM line WHERE tocEntryId IS NOT NULL AND id IN ($marks)',
          chunk);
      for (final r in rs) {
        out[r.columnAt(0) as int] = r.columnAt(1) as int;
      }
    }
    return out;
  }

  /// heRef of line ids.
  Map<int, String> heRefs(Iterable<int> ids) {
    final list = ids.toList();
    if (list.isEmpty) return {};
    final rs = _db.select(
        'SELECT id, heRef FROM line WHERE id IN (${List.filled(list.length, '?').join(',')})', list);
    return {for (final r in rs) r['id'] as int: (r['heRef'] as String?) ?? ''};
  }

  Map<int, String> bookTitles() =>
      {for (final r in _db.select('SELECT id, title FROM book')) r['id'] as int: r['title'] as String};

  /// Book ids whose title, or an ancestor category's title, contains [needle].
  Set<int> booksMatching(String needle) {
    final rs = _db.select('''
      SELECT b.id FROM book b WHERE b.title LIKE ?1
      UNION SELECT b.id FROM book b
        JOIN category_closure cc ON cc.descendantId = b.categoryId
        JOIN category c ON c.id = cc.ancestorId WHERE c.title LIKE ?1''', ['%$needle%']);
    return {for (final r in rs) r.columnAt(0) as int};
  }

  int countLines() => _db.select('SELECT COUNT(*) FROM line').first.columnAt(0) as int;

  /// File identity, stored as the index payload to detect a replaced DB.
  String fingerprint() {
    final s = File(path).statSync();
    return '${s.size}|${s.modified.toUtc().toIso8601String()}';
  }

  void close() {
    _zstd?.close();
    _db.close();
  }
}
