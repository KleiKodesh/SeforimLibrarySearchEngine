import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:sqlite3/sqlite3.dart';

import 'note_ids.dart';
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
///
/// Kept OPEN for the life of the object: one connection, its schema probe,
/// zstd dictionary and prepared statements (by SQL text) serve every search
/// and batch, the snippet-text reads included. When the database changes
/// (size or write time of the main file, or `PRAGMA data_version`: a commit
/// through the WAL, e.g. a new zstd dictionary) the next query opens a fresh
/// connection, probe and codec FIRST and only then closes the old ones (a
/// failed reopen leaves the old ones working). No cursor stays open between
/// calls ([bookLines] reads in chunks), so a reopen never closes the DB under
/// a live cursor. [close] is idempotent. Single-isolate use: sqlite3 handles
/// do not cross isolates.
/// A snippet's TEXT (KitveiHakodesh Core's FtsSnippetText): the matched line
/// and, for a short window only, the plain context before and after it.
class SnippetText {
  SnippetText(this.lineId, this.text, this.before, this.after);
  final int lineId;
  final String text, before, after;
}

/// How reading one line's text went (issue #13): found, no text row, or a row
/// that cannot be decoded ([error] names the line id, issue #8).
enum LineStatus { found, missing, unreadable }

class LineRead {
  const LineRead.found(String this.text)
      : status = LineStatus.found,
        error = null;
  const LineRead.missing()
      : status = LineStatus.missing,
        text = null,
        error = null;
  const LineRead.unreadable(Object this.error)
      : status = LineStatus.unreadable,
        text = null;
  final LineStatus status;
  final String? text;
  final Object? error;
}

class SeforimDb {
  SeforimDb._(this.path, this._zstdLib, _Open o)
      : _db = o.db,
        splitContent = o.split,
        compressed = o.compressed,
        _zstd = o.zstd,
        _stamp = o.stamp,
        _dataVersion = o.dataVersion,
        hasNotes = o.notes;

  final String path;
  final String? _zstdLib;
  Database _db;

  /// `line_content` exists and `line` has no `content` column (schema 6+).
  bool splitContent;

  /// `zstd_dict` exists.
  bool compressed;
  ZstdDictDecoder? _zstd;
  String _stamp;
  int _dataVersion;
  bool _closed = false;

  /// Schema 7: the DB has `line_note`; its notes are indexed as virtual notes
  /// books (issue #13, ids in note_ids.dart).
  bool hasNotes;

  /// Line ids whose row could not be decoded on a lenient (search-path) read:
  /// read as empty, recorded here so 0 hits is never silently wrong (issue #13).
  /// Accumulates until [clearUnreadable].
  final Set<int> unreadableLineIds = {};

  /// Empties [unreadableLineIds] (call before a search to count only its rows).
  void clearUnreadable() => unreadableLineIds.clear();

  /// Prepared statements kept with the connection, by SQL text.
  final _statements = <String, PreparedStatement>{};

  static SeforimDb open(String path, {String? zstdLib}) => SeforimDb._(path, zstdLib, _open(path, zstdLib));

  static String _fileStamp(String path) {
    String one(String p) {
      final s = File(p).statSync();
      return s.type == FileSystemEntityType.notFound ? '-' : '${s.size}@${s.modified.microsecondsSinceEpoch}';
    }

    return one(path); // not the -wal: a reader alone can create or touch it
  }

  static int _readDataVersion(Database db) => db.select('PRAGMA data_version').first.columnAt(0) as int;

  static List<Uint8List> _readDictionaries(Database db) => [
        for (final r in db.select('SELECT dict FROM zstd_dict'))
          if (r['dict'] != null) r['dict'] as Uint8List,
      ];

  static _Open _open(String path, String? zstdLib) {
    final stamp = _fileStamp(path);
    final db = sqlite3.open(path, mode: OpenMode.readOnly);
    try {
      db.execute('PRAGMA cache_size=-65536; PRAGMA mmap_size=268435456; PRAGMA temp_store=MEMORY;');
      bool table(String t) =>
          db.select("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", [t]).isNotEmpty;
      final lineHasContent = db.select('PRAGMA table_info(line)').any((r) => r['name'] == 'content');
      final split = table('line_content') && !lineHasContent;
      final compressed = table('zstd_dict');
      final dataVersion = _readDataVersion(db);
      ZstdDictDecoder? dec;
      if (compressed) {
        final dicts = _readDictionaries(db);
        if (dicts.isEmpty) throw StateError('zstd_dict table is empty: compressed content cannot be decoded');
        dec = ZstdDictDecoder(Zstd.load(zstdLib), dicts, reload: () => _readDictionaries(db));
      }
      return _Open(db, split, compressed, dec, stamp, dataVersion, table('line_note'));
    } catch (_) {
      db.close();
      rethrow;
    }
  }

  /// Reopens when the database changed since it was opened: the main file's
  /// stamp, or `PRAGMA data_version` (another connection committed, a WAL
  /// write included). The new connection, probe and codec are opened FIRST;
  /// the old ones are closed only once that succeeded.
  void _ensureCurrent() {
    if (_closed) throw StateError('SeforimDb is closed');
    if (_fileStamp(path) == _stamp && _readDataVersion(_db) == _dataVersion) return;
    final o = _open(path, _zstdLib); // throws: the old handles stay open and usable
    _closeHandles();
    _db = o.db;
    splitContent = o.split;
    compressed = o.compressed;
    _zstd = o.zstd;
    _stamp = o.stamp;
    _dataVersion = o.dataVersion;
    hasNotes = o.notes;
  }

  bool _inReadTx = false;

  /// Runs [body] inside ONE read transaction, so every chunked query in it
  /// sees the same snapshot (a writer committing between chunks is not seen).
  T _readSnapshot<T>(T Function() body) {
    _ensureCurrent();
    _db.execute('BEGIN');
    _inReadTx = true;
    try {
      return body();
    } finally {
      _inReadTx = false;
      _db.execute('COMMIT');
    }
  }

  /// [ids] in chunks of at most 900 (under SQLite's bound-parameter limit).
  static Iterable<List<int>> _chunks(List<int> ids) sync* {
    for (var i = 0; i < ids.length; i += 900) {
      yield ids.sublist(i, i + 900 > ids.length ? ids.length : i + 900);
    }
  }

  /// Runs [sql] through its kept prepared statement.
  ResultSet _select(String sql, [List<Object?> params = const []]) {
    if (!_inReadTx) _ensureCurrent();
    if (_statements.length >= 64 && !_statements.containsKey(sql)) {
      // IN-lists of every remainder size: bound the cache
      for (final stmt in _statements.values) {
        stmt.close();
      }
      _statements.clear();
    }
    final stmt = _statements[sql] ??= _db.prepare(sql);
    return stmt.select(params);
  }

  String get _contentCol => splitContent ? 'lc.content' : 'l.content';
  String get _contentJoin => splitContent ? ' LEFT JOIN line_content lc ON lc.id = l.id' : '';

  /// Decide by runtime value type, never via SQL (SQLite coerces blobs).
  ///
  /// A string starting "KLUv" is decoded only when it is valid base64 AND
  /// carries a valid zstd frame header; otherwise it is the text it claims to
  /// be. A frame that fails (missing dictionary, bad data) and malformed UTF-8
  /// raise a [FormatException] naming [lineId] (issue #8).
  String decode(Object? v, [int? lineId]) {
    final s = _decodeRaw(v, lineId);
    return s.startsWith('\uFEFF') ? s.substring(1) : s; // one leading BOM (issue #13)
  }

  String _decodeRaw(Object? v, int? lineId) {
    if (v == null) return '';
    if (v is String) {
      if (_zstd == null || v.length < 8 || !v.startsWith('KLUv')) return v;
      final Uint8List frame;
      try {
        frame = base64.decode(v);
        ZstdDictDecoder.readFrameDictionaryId(frame);
      } on FormatException {
        return v; // not base64, or no frame header: genuinely text
      }
      return _decodeFrame(frame, lineId);
    }
    if (v is Uint8List) {
      if (_zstd == null) throw StateError('line ${lineId ?? '?'}: blob content but no zstd_dict');
      return _decodeFrame(v, lineId);
    }
    return v.toString();
  }

  String _decodeFrame(Uint8List frame, int? lineId) {
    final Uint8List bytes;
    try {
      bytes = _zstd!.decode(frame);
    } on FormatException catch (e) {
      throw FormatException('line ${lineId ?? '?'}: compressed content cannot be decoded: ${e.message}');
    }
    try {
      return utf8.decode(bytes);
    } on FormatException catch (e) {
      throw FormatException('line ${lineId ?? '?'}: content is not valid UTF-8: ${e.message}');
    }
  }

  /// [raw] (a row of line [lineId], or the note body of virtual line
  /// [lineId]) decoded. Throws like [decode], naming the line id.
  String lineText(int lineId, Object? raw) => decode(raw, lineId);

  /// The raw body of virtual line [id]; [missing] when there is no such note.
  Object? _noteBody(int id, {required void Function() missing}) {
    final src = noteSourceOf(id)!;
    final rs = !hasNotes
        ? null
        : _select('SELECT n.body FROM line_note n JOIN line l ON l.id = n.lineId WHERE n.lineId = ? AND n.noteIndex = ?', [src.sourceLineId, src.noteIndex]);
    if (rs == null || rs.isEmpty) {
      missing();
      return null;
    }
    return rs.first.columnAt(0);
  }

  /// The note text of virtual line ids (search path: unreadable recorded, or
  /// thrown with [strict]); absent when missing.
  /// One statement per chunk of distinct source lines (IN-list, as C#
  /// `FetchNotes`), never one query per note.
  Map<int, String> _noteTexts(Iterable<int> ids, bool strict) {
    final out = <int, String>{};
    if (!hasNotes) return out;
    final wanted = <int>{};
    final sourceLines = <int>{};
    for (final id in ids) {
      final src = noteSourceOf(id);
      if (src == null || !wanted.add(id)) continue;
      sourceLines.add(src.sourceLineId);
    }
    for (final chunk in _chunks(sourceLines.toList())) {
      final rs = _select(
          'SELECT n.lineId, n.noteIndex, n.body FROM line_note n JOIN line l ON l.id = n.lineId'
          ' WHERE n.lineId IN (${List.filled(chunk.length, '?').join(',')})',
          chunk);
      for (final r in rs) {
        final lineId = r.columnAt(0) as int, noteIndex = r.columnAt(1) as int;
        if (lineId < 0 || lineId > maxNoteSourceLineId || noteIndex < 0 || noteIndex > maxNoteIndex) continue;
        final id = noteLineId(lineId, noteIndex);
        if (!wanted.contains(id)) continue;
        final t = strict ? lineText(id, r.columnAt(2)) : _lenientText(id, r.columnAt(2));
        if (t != null) out[id] = t;
      }
    }
    return out;
  }

  /// [lineText] that records an unreadable row in [unreadableLineIds] and
  /// returns null instead of throwing (search path).
  String? _lenientText(int lineId, Object? raw) {
    try {
      return lineText(lineId, raw);
    } catch (_) {
      // any decode failure (base64, zstd, UTF-8, missing dictionary) is an unreadable row
      unreadableLineIds.add(lineId);
    }
    return null;
  }

  /// One line's text (a virtual id: its note body), tri-state: found,
  /// missing (no text row) or unreadable (the error names the line id).
  LineRead readLine(int lineId) {
    Object? raw;
    if (isVirtualId(lineId)) {
      var gone = false;
      raw = _noteBody(lineId, missing: () => gone = true);
      if (gone) return const LineRead.missing();
    } else {
      final rs = _select('SELECT content FROM ${splitContent ? 'line_content' : 'line'} WHERE id = ?', [lineId]);
      if (rs.isEmpty) return const LineRead.missing();
      raw = rs.first.columnAt(0);
    }
    try {
      return LineRead.found(lineText(lineId, raw));
    } on FormatException catch (e) {
      return LineRead.unreadable(e);
    } on StateError catch (e) {
      return LineRead.unreadable(e);
    }
  }

  /// The ids of the books flagged `book.isBaseBook` (the relevance order
  /// puts them first within a tier, issue #6). Empty without the column.
  Set<int> baseBooks() {
    final hasBase = db.select('PRAGMA table_info(book)').any((r) => r['name'] == 'isBaseBook');
    if (!hasBase) return {};
    return {for (final r in db.select('SELECT id FROM book WHERE isBaseBook = 1')) r['id'] as int};
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
    if (!hasNotes) return order;
    // issue #13: each book's notes book right after it
    final withNotes = booksWithNotes();
    return [
      for (final b in order) ...[b, if (withNotes.contains(b)) notesBookId(b)]
    ];
  }

  /// Real book ids that have `line_note` rows (empty without the table).
  Set<int> booksWithNotes() => !hasNotes
      ? {}
      : {
          for (final r in _select('SELECT DISTINCT l.bookId FROM line_note n JOIN line l ON l.id = n.lineId'))
            r.columnAt(0) as int
        };

  /// The index build's loud guard (issue #13): throws, naming the id, when a
  /// real line / book id or a note does not fit the virtual id scheme.
  void checkNoteIds() {
    if (!hasNotes) return;
    final l = _select('SELECT max(id) FROM line').first.columnAt(0) as int?;
    final b = _select('SELECT max(id) FROM book').first.columnAt(0) as int?;
    final n = _select('SELECT max(lineId), min(noteIndex), max(noteIndex) FROM line_note').first;
    checkNoteIdLimits(
        maxLineId: l,
        maxBookId: b,
        maxNoteLineId: n.columnAt(0) as int?,
        minNoteIndex: n.columnAt(1) as int?,
        maxNoteIdx: n.columnAt(2) as int?);
  }

  /// How many lines [bookId] feeds (a notes book: its notes).
  int bookLineCount(int bookId) {
    final src = sourceBookOf(bookId);
    if (src == null) return _select('SELECT COUNT(*) FROM line WHERE bookId = ?', [bookId]).first.columnAt(0) as int;
    if (!hasNotes) return 0;
    return _select('SELECT COUNT(*) FROM line_note n JOIN line l ON l.id = n.lineId WHERE l.bookId = ?', [src])
        .first
        .columnAt(0) as int;
  }

  /// The source of a hit on a notes line (line id >= 2^52): its source line,
  /// note index and source book; null for a real line.
  ({int sourceLineId, int noteIndex, int sourceBookId})? noteSource(int lineId, int bookId) {
    final s = noteSourceOf(lineId);
    if (s == null) return null;
    return (sourceLineId: s.sourceLineId, noteIndex: s.noteIndex, sourceBookId: sourceBookOf(bookId) ?? -1);
  }

  /// The open connection (reopened first if the file changed).
  Database get db {
    _ensureCurrent();
    return _db;
  }

  /// Lines per [bookLines] chunk.
  static const bookLinesChunk = 2000;

  /// Text bytes (UTF-16, 2 per char) per [bookLines] chunk (issue #20): a
  /// chunk stops at this many decoded bytes even before [bookLinesChunk]
  /// lines (at least one line), so a book of huge lines cannot balloon the
  /// feed; the next query's LIMIT shrinks to what fit.
  static const bookLinesChunkBytes = 32000000;

  /// One book's lines in reading order.
  ///
  /// Read in chunks (keyset on `lineIndex, id`), each one fully read before
  /// it is yielded: no statement or cursor stays open between items, so an
  /// early stop (`take`, `break`, an exception in the consumer) leaks nothing
  /// and a reopen between chunks never closes the DB under a live cursor.
  Iterable<DbLine> bookLines(int bookId) sync* {
    final src = sourceBookOf(bookId);
    if (src != null) {
      yield* _noteLines(bookId, src);
      return;
    }
    var lastIndex = -(1 << 62), lastId = -(1 << 62);
    var limit = bookLinesChunk;
    while (true) {
      // the section: `line_toc` (the authoritative map) when the DB has it
      final hasLineToc =
          _select("SELECT 1 FROM sqlite_master WHERE type='table' AND name='line_toc'").isNotEmpty;
      final toc = hasLineToc ? 'COALESCE(lt.tocEntryId, 0)' : 'COALESCE(l.tocEntryId, 0)';
      final tocJoin = hasLineToc ? ' LEFT JOIN line_toc lt ON lt.lineId = l.id' : '';
      final rows = _select(
          'SELECT l.id, l.lineIndex, $toc, $_contentCol FROM line l$_contentJoin$tocJoin'
          ' WHERE l.bookId = ? AND (l.lineIndex > ? OR (l.lineIndex = ? AND l.id > ?))'
          ' ORDER BY l.lineIndex, l.id LIMIT $limit',
          [bookId, lastIndex, lastIndex, lastId]);
      final chunk = <DbLine>[];
      var bytes = 0;
      for (final r in rows) {
        final line = DbLine(r.columnAt(0) as int, bookId, r.columnAt(1) as int, r.columnAt(2) as int,
            lineText(r.columnAt(0) as int, r.columnAt(3)));
        chunk.add(line);
        bytes += 2 * line.text.length + 64;
        if (bytes >= bookLinesChunkBytes) break; // the rest is read again by the next query
      }
      final full = chunk.length < rows.length; // stopped by the byte budget
      yield* chunk;
      if (!full && rows.length < limit) return;
      limit = full ? chunk.length : bookLinesChunk;
      lastIndex = chunk.last.lineIndex;
      lastId = chunk.last.id;
    }
  }

  /// A notes book's lines: every note of [src]'s lines in source order
  /// (lineIndex, line id, noteIndex), as virtual lines (no section).
  Iterable<DbLine> _noteLines(int bookId, int src) sync* {
    if (!hasNotes) return;
    var a = -(1 << 62), b = -(1 << 62), c = -(1 << 62);
    var limit = bookLinesChunk;
    while (true) {
      final rows = _select(
          'SELECT l.lineIndex, l.id, n.noteIndex, n.body FROM line_note n JOIN line l ON l.id = n.lineId'
          ' WHERE l.bookId = ? AND (l.lineIndex, l.id, n.noteIndex) > (?, ?, ?)'
          ' ORDER BY l.lineIndex, l.id, n.noteIndex LIMIT $limit',
          [src, a, b, c]);
      final chunk = <DbLine>[];
      var bytes = 0;
      for (final r in rows) {
        a = r.columnAt(0) as int;
        b = r.columnAt(1) as int;
        c = r.columnAt(2) as int;
        final id = noteLineId(b, c);
        chunk.add(DbLine(id, bookId, a, 0, lineText(id, r.columnAt(3))));
        bytes += 2 * chunk.last.text.length + 64;
        if (bytes >= bookLinesChunkBytes) break; // byte budget (issue #20), as bookLines
      }
      final full = chunk.length < rows.length;
      yield* chunk;
      if (!full && rows.length < limit) return;
      limit = full ? chunk.length : bookLinesChunk;
    }
  }

  /// Texts of the given line ids: id + content only, by primary key from the table that
  /// holds the text (`line_content` on a split database, no join), what verification reads.
  /// A line with no text row is absent (callers read it as empty). A virtual
  /// notes line reads its note body. An unreadable row is absent too and recorded in
  /// [unreadableLineIds]; with [strict] it throws instead (naming the line id).
  Map<int, String> lineTexts(Iterable<int> ids, {bool strict = false}) {
    final all = ids.toList();
    final out = _noteTexts(all.where(isVirtualId), strict);
    final list = [for (final id in all) if (!isVirtualId(id)) id];
    final table = splitContent ? 'line_content' : 'line';
    for (var i = 0; i < list.length; i += 500) {
      final chunk = list.sublist(i, i + 500 > list.length ? list.length : i + 500);
      final rs = _select(
          'SELECT id, content FROM $table WHERE id IN (${List.filled(chunk.length, '?').join(',')})',
          chunk);
      for (final r in rs) {
        final id = r.columnAt(0) as int;
        final text = strict ? lineText(id, r.columnAt(1)) : _lenientText(id, r.columnAt(1));
        if (text != null) out[id] = text;
      }
    }
    return out;
  }

  /// How many lines per side a SHORT snippet's context reaches (same book
  /// only); KitveiHakodesh Core's `SnippetNeighbourLineRadius`.
  static const snippetNeighbourLineRadius = 2;

  /// Core's `FetchSnippetTexts`: the snippet texts of the given lines in ONE
  /// statement per chunk. Plain ids ([lineIds]) are read by primary key from
  /// the table that holds the text; [shortLineIds] also get their non-empty
  /// same-book neighbours within [snippetNeighbourLineRadius], each side
  /// joined by single spaces in document order (plain context, never located).
  /// A line with no text row is absent. A virtual notes line is its note
  /// alone. An unreadable row (each neighbour checked by its OWN line id) is
  /// read as empty and recorded in [unreadableLineIds].
  Map<int, SnippetText> snippetTexts(Iterable<int> lineIds, Iterable<int> shortLineIds) {
    // a notes line is its note alone: no neighbour context
    final notes = _noteTexts({...lineIds, ...shortLineIds}.where(isVirtualId), false);
    final shortSet = shortLineIds.where((id) => !isVirtualId(id)).toSet();
    final plain = [for (final id in lineIds) if (!shortSet.contains(id) && !isVirtualId(id)) id];
    final shorts = shortSet.toList();
    final texts = <int, String>{};
    final before = <int, Map<int, String>>{};
    final after = <int, Map<int, String>>{};
    const perStatement = 997; // under SQLite's 999 variables: the radius is bound twice
    var p = 0, q = 0;
    while (p < plain.length || q < shorts.length) {
      final plainCount = perStatement < plain.length - p ? perStatement : plain.length - p;
      final rest = perStatement - plainCount;
      final shortCount = rest < shorts.length - q ? rest : shorts.length - q;
      final parts = <String>[];
      final params = <Object?>[];
      if (plainCount > 0) {
        parts.add('SELECT id, 0, id, content FROM ${splitContent ? 'line_content' : 'line'}'
            ' WHERE id IN (${List.filled(plainCount, '?').join(',')})');
        params.addAll(plain.sublist(p, p + plainCount));
      }
      if (shortCount > 0) {
        // the content join must come AFTER this JOIN's own ON clause
        parts.add('SELECT m.id, n.lineIndex - m.lineIndex, n.id, ${splitContent ? 'lc.content' : 'n.content'}'
            ' FROM line m JOIN line n ON n.bookId = m.bookId'
            ' AND n.lineIndex BETWEEN m.lineIndex - ? AND m.lineIndex + ?'
            ' AND (n.lineIndex <> m.lineIndex OR n.id = m.id)'
            '${splitContent ? ' LEFT JOIN line_content lc ON lc.id = n.id' : ''}'
            ' WHERE m.id IN (${List.filled(shortCount, '?').join(',')})');
        params
          ..add(snippetNeighbourLineRadius)
          ..add(snippetNeighbourLineRadius)
          ..addAll(shorts.sublist(q, q + shortCount));
      }
      p += plainCount;
      q += shortCount;
      for (final r in _select(parts.join(' UNION ALL '), params)) {
        final id = r.columnAt(0) as int;
        final delta = r.columnAt(1) as int;
        final text = _lenientText(r.columnAt(2) as int, r.columnAt(3)) ?? '';
        if (delta == 0) {
          texts[id] = text;
          continue;
        }
        if (text.isEmpty) continue; // empty neighbours are skipped, not joined
        (delta < 0 ? before : after).putIfAbsent(id, () => {})[delta] = text;
      }
    }
    String side(Map<int, String>? m) {
      if (m == null) return '';
      final keys = m.keys.toList()..sort();
      return [for (final k in keys) m[k]!].join(' ');
    }

    return {
      for (final e in texts.entries) e.key: SnippetText(e.key, e.value, side(before[e.key]), side(after[e.key])),
      for (final e in notes.entries) e.key: SnippetText(e.key, e.value, '', ''),
    };
  }

  /// Each line's section (TOC entry) for grouping, straight from the DB: the
  /// engine only returns line ids. `line_toc` is the authoritative map
  /// (`line.tocEntryId` is NULL for most lines). Lines without one are absent.
  Map<int, int> sectionsOf(Iterable<int> ids) => _readSnapshot(() {
        final hasLineToc =
            _select("SELECT 1 FROM sqlite_master WHERE type='table' AND name='line_toc'").isNotEmpty;
        final out = <int, int>{};
        for (final chunk in _chunks(ids.toList())) {
          final marks = List.filled(chunk.length, '?').join(',');
          final rs = _select(
              hasLineToc
                  ? 'SELECT lineId, tocEntryId FROM line_toc WHERE lineId IN ($marks)'
                  : 'SELECT id, tocEntryId FROM line WHERE tocEntryId IS NOT NULL AND id IN ($marks)',
              chunk);
          for (final r in rs) {
            out[r.columnAt(0) as int] = r.columnAt(1) as int;
          }
        }
        return out;
      });

  /// heRef of line ids, read in chunks of at most 900 ids inside one read
  /// transaction (one snapshot for every chunk).
  Map<int, String> heRefs(Iterable<int> ids) {
    final list = ids.toList();
    if (list.isEmpty) return {};
    // a notes line shows its source line's ref
    final src = {for (final id in list) if (isVirtualId(id)) id: noteSourceOf(id)!.sourceLineId};
    final real = {for (final id in list) isVirtualId(id) ? src[id]! : id}.toList();
    final refs = _readSnapshot(() => {
          for (final chunk in _chunks(real))
            for (final r in _select(
                'SELECT id, heRef FROM line WHERE id IN (${List.filled(chunk.length, '?').join(',')})', chunk))
              r['id'] as int: (r['heRef'] as String?) ?? '',
        });
    return {
      for (final id in list)
        if (refs[src[id] ?? id] case final r?) id: r,
    };
  }

  /// Book titles; a notes book is titled with the Hebrew word for notes + its
  /// book's title (issue #13).
  Map<int, String> bookTitles() {
    final out = {for (final r in _select('SELECT id, title FROM book')) r['id'] as int: r['title'] as String};
    for (final b in booksWithNotes()) {
      out[notesBookId(b)] = '$notesTitlePrefix ${out[b] ?? b}';
    }
    return out;
  }

  /// The notes book title prefix (the Hebrew word for notes).
  static const notesTitlePrefix = '\u05D4\u05E2\u05E8\u05D5\u05EA';

  /// Book ids whose title, or an ancestor category's title, contains [needle].
  Set<int> booksMatching(String needle) {
    final rs = _select('''
      SELECT b.id FROM book b WHERE b.title LIKE ?1
      UNION SELECT b.id FROM book b
        JOIN category_closure cc ON cc.descendantId = b.categoryId
        JOIN category c ON c.id = cc.ancestorId WHERE c.title LIKE ?1''', ['%$needle%']);
    return {for (final r in rs) r.columnAt(0) as int};
  }

  int countLines() => _select('SELECT COUNT(*) FROM line').first.columnAt(0) as int;

  /// File identity, stored as the index payload to detect a replaced DB.
  String fingerprint() {
    final s = File(path).statSync();
    return '${s.size}|${s.modified.toUtc().toIso8601String()}';
  }

  /// Closes the connection, statements and codec; idempotent.
  void close() {
    if (_closed) return;
    _closed = true;
    _closeHandles();
  }

  void _closeHandles() {
    for (final stmt in _statements.values) {
      stmt.close();
    }
    _statements.clear();
    _zstd?.close();
    _zstd = null;
    _db.close();
  }
}

/// One opening of the database: connection, schema probe, decoder, file stamp.
class _Open {
  _Open(this.db, this.split, this.compressed, this.zstd, this.stamp, this.dataVersion, this.notes);
  final Database db;
  final bool split, compressed;
  final ZstdDictDecoder? zstd;
  final String stamp;
  final int dataVersion;
  final bool notes;
}
