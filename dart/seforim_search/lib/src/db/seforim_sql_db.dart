// seforim.db's reads over the HOST's own SQLite connection: schema probe,
// catalog order, the line feed, notes books, snippet texts. The host plugs in
// its database layer as a [SeforimSqlConnection] (its sqlite library, its
// line-content decoder); package:seforim_search_sqlite is the ready-made one.
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'data_source.dart';
import 'note_ids.dart';

/// A read-only SQL connection the host owns (its own sqlite library).
abstract interface class SqlReadConnection {
  /// The rows of a read statement, each its column values in select order.
  List<List<Object?>> select(String sql, [List<Object?> params = const []]);

  /// Runs a statement without results (`BEGIN`, `COMMIT`, pragmas).
  void execute(String sql);

  /// Releases the connection (a connection the host shares: a no-op).
  void close();
}

/// A connection to seforim.db plus the host's line-content decoder.
abstract interface class SeforimSqlConnection implements SqlReadConnection {
  /// One zstd frame of line content decompressed with the database's
  /// `zstd_dict` dictionaries (the frame names its dictionary id). Throws a
  /// [FormatException] when it cannot be decoded.
  Uint8List decodeFrame(Uint8List frame);
}

/// Opens a [SeforimSqlConnection] to the database at a path.
typedef SeforimSqlOpener = SeforimSqlConnection Function(String path);

/// The dictionary id in a zstd frame header; throws a [FormatException] when
/// [f] is not a zstd frame with a dictionary id.
int zstdFrameDictionaryId(Uint8List f) {
  if (f.length < 5 || ByteData.sublistView(f).getUint32(0, Endian.little) != 0xFD2FB528) {
    throw const FormatException('not a zstd frame');
  }
  final fhd = f[4];
  var width = fhd & 3;
  if (width == 3) width = 4;
  if (width == 0) throw const FormatException('zstd frame carries no dictionary id');
  final offset = (fhd & 0x20) != 0 ? 5 : 6;
  if (f.length < offset + width) throw const FormatException('truncated zstd frame header');
  var id = 0;
  for (var i = 0; i < width; i++) {
    id |= f[offset + i] << (8 * i);
  }
  return id;
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

/// Read-only access to seforim.db over a host connection: schema probe,
/// content decoding, catalog order and the line feed.
///
/// Kept OPEN for the life of the object: one connection and its schema probe
/// serve every search and batch, the snippet-text reads included. When the
/// database changes (size or write time of the main file, or `PRAGMA
/// data_version`: a commit through the WAL, e.g. a new zstd dictionary) the
/// next query opens a fresh connection and probe FIRST (through the host's
/// [SeforimSqlOpener]) and only then closes the old one (a failed reopen
/// leaves the old one working). No cursor stays open between calls
/// ([bookLines] reads in chunks), so a reopen never closes the DB under a
/// live cursor. [close] is idempotent. Single-isolate use.
class SeforimSqlDb implements SeforimDataSource {
  SeforimSqlDb(String path, SeforimSqlOpener opener) : this._(path, opener, _open(path, opener));

  SeforimSqlDb._(this.path, this._opener, _Open o)
      : _db = o.db,
        splitContent = o.split,
        compressed = o.compressed,
        _stamp = o.stamp,
        _dataVersion = o.dataVersion,
        hasNotes = o.notes;

  final String path;
  final SeforimSqlOpener _opener;
  SeforimSqlConnection _db;

  /// `line_content` exists and `line` has no `content` column (schema 6+).
  bool splitContent;

  /// `zstd_dict` exists.
  bool compressed;
  String _stamp;
  int _dataVersion;
  bool _closed = false;

  /// Schema 7: the DB has `line_note`; its notes are indexed as virtual notes
  /// books (issue #13, ids in note_ids.dart).
  @override
  bool hasNotes;

  /// Line ids whose row could not be decoded on a lenient (search-path) read:
  /// read as empty, recorded here so 0 hits is never silently wrong (issue #13).
  /// Accumulates until [clearUnreadable].
  final Set<int> unreadableLineIds = {};

  /// Empties [unreadableLineIds] (call before a search to count only its rows).
  void clearUnreadable() => unreadableLineIds.clear();

  static String _fileStamp(String path) {
    final s = File(path).statSync();
    // not the -wal: a reader alone can create or touch it
    return s.type == FileSystemEntityType.notFound ? '-' : '${s.size}@${s.modified.microsecondsSinceEpoch}';
  }

  static int _readDataVersion(SqlReadConnection db) => db.select('PRAGMA data_version').first[0] as int;

  static _Open _open(String path, SeforimSqlOpener opener) {
    final stamp = _fileStamp(path);
    final db = opener(path);
    try {
      bool table(String t) =>
          db.select("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", [t]).isNotEmpty;
      final lineHasContent = _hasColumn(db, 'line', 'content');
      final split = table('line_content') && !lineHasContent;
      final compressed = table('zstd_dict');
      final dataVersion = _readDataVersion(db);
      if (compressed && db.select('SELECT 1 FROM zstd_dict WHERE dict IS NOT NULL LIMIT 1').isEmpty) {
        throw StateError('zstd_dict table is empty: compressed content cannot be decoded');
      }
      return _Open(db, split, compressed, stamp, dataVersion, table('line_note'));
    } catch (_) {
      db.close();
      rethrow;
    }
  }

  /// [table] has a column [column] (`PRAGMA table_info`: name is column 1).
  static bool _hasColumn(SqlReadConnection db, String table, String column) =>
      db.select('PRAGMA table_info($table)').any((r) => r[1] == column);

  /// Reopens when the database changed since it was opened: the main file's
  /// stamp, or `PRAGMA data_version` (another connection committed, a WAL
  /// write included). The new connection and probe are opened FIRST; the old
  /// one is closed only once that succeeded.
  void _ensureCurrent() {
    if (_closed) throw StateError('SeforimSqlDb is closed');
    if (_fileStamp(path) == _stamp && _readDataVersion(_db) == _dataVersion) return;
    final o = _open(path, _opener); // throws: the old connection stays open and usable
    final old = _db;
    _db = o.db;
    if (!identical(old, o.db)) old.close();
    splitContent = o.split;
    compressed = o.compressed;
    _stamp = o.stamp;
    _dataVersion = o.dataVersion;
    hasNotes = o.notes;
  }

  /// The current connection (reopened first if the database changed).
  SeforimSqlConnection get connection {
    _ensureCurrent();
    return _db;
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

  /// Runs [sql] on the current connection (reopened first if the DB changed).
  List<List<Object?>> _select(String sql, [List<Object?> params = const []]) {
    if (!_inReadTx) _ensureCurrent();
    return _db.select(sql, params);
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
      if (!compressed || v.length < 8 || !v.startsWith('KLUv')) return v;
      final Uint8List frame;
      try {
        frame = base64.decode(v);
        zstdFrameDictionaryId(frame);
      } on FormatException {
        return v; // not base64, or no frame header: genuinely text
      }
      return _decodeFrame(frame, lineId);
    }
    if (v is Uint8List) {
      if (!compressed) throw StateError('line ${lineId ?? '?'}: blob content but no zstd_dict');
      return _decodeFrame(v, lineId);
    }
    return v.toString();
  }

  /// A frame's decompressed bytes, unchanged (no UTF-8 round trip).
  Uint8List _frameBytes(Uint8List frame, int lineId) {
    try {
      return _db.decodeFrame(frame);
    } on FormatException catch (e) {
      throw FormatException('line $lineId: compressed content cannot be decoded: ${e.message}');
    }
  }

  String _decodeFrame(Uint8List frame, int? lineId) {
    final Uint8List bytes;
    try {
      bytes = _db.decodeFrame(frame);
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
    return rs.first[0];
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
        final lineId = r[0] as int, noteIndex = r[1] as int;
        if (lineId < 0 || lineId > maxNoteSourceLineId || noteIndex < 0 || noteIndex > maxNoteIndex) continue;
        final id = noteLineId(lineId, noteIndex);
        if (!wanted.contains(id)) continue;
        final t = strict ? lineText(id, r[2]) : _lenientText(id, r[2]);
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
      raw = rs.first[0];
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
    if (!_hasColumn(connection, 'book', 'isBaseBook')) return {};
    return {for (final r in _select('SELECT id FROM book WHERE isBaseBook = 1')) r[0] as int};
  }

  /// Book ids in catalog order: categories by (orderIndex<0, orderIndex),
  /// in each folder its base books first, then subfolders, then the rest
  /// (books by orderIndex). Books outside the tree go last.
  @override
  List<int> catalogBookOrder() {
    final hasCatOrder = _hasColumn(connection, 'category', 'orderIndex');
    final hasBase = _hasColumn(connection, 'book', 'isBaseBook');
    final cats = _select('SELECT id, parentId FROM category ORDER BY level'
        '${hasCatOrder ? ', CASE WHEN orderIndex < 0 THEN 1 ELSE 0 END, orderIndex' : ''}');
    final known = {for (final r in cats) r[0] as int};
    final children = <int, List<int>>{};
    final roots = <int>[];
    for (final r in cats) {
      final p = r[1] as int?;
      if (p != null && known.contains(p)) {
        (children[p] ??= []).add(r[0] as int);
      } else {
        roots.add(r[0] as int);
      }
    }
    final books = <int, List<(int, bool)>>{};
    final orphans = <int>[];
    for (final r in _select('SELECT id, categoryId, ${hasBase ? 'isBaseBook' : '0'} AS b FROM book ORDER BY orderIndex, id')) {
      final c = r[1] as int?;
      final e = (r[0] as int, (r[2] as int? ?? 0) != 0);
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
            r[0] as int
        };

  /// The index build's loud guard (issue #13): throws, naming the id, when a
  /// real line / book id or a note does not fit the virtual id scheme.
  @override
  void checkNoteIds() {
    if (!hasNotes) return;
    final l = _select('SELECT max(id) FROM line').first[0] as int?;
    final b = _select('SELECT max(id) FROM book').first[0] as int?;
    final n = _select('SELECT max(lineId), min(noteIndex), max(noteIndex) FROM line_note').first;
    checkNoteIdLimits(
        maxLineId: l,
        maxBookId: b,
        maxNoteLineId: n[0] as int?,
        minNoteIndex: n[1] as int?,
        maxNoteIdx: n[2] as int?);
  }

  /// How many lines [bookId] feeds (a notes book: its notes).
  @override
  int bookLineCount(int bookId) {
    final src = sourceBookOf(bookId);
    if (src == null) return _select('SELECT COUNT(*) FROM line WHERE bookId = ?', [bookId]).first[0] as int;
    if (!hasNotes) return 0;
    return _select('SELECT COUNT(*) FROM line_note n JOIN line l ON l.id = n.lineId WHERE l.bookId = ?', [src])
        .first
        [0] as int;
  }

  /// The source of a hit on a notes line (line id >= 2^52): its source line,
  /// note index and source book; null for a real line.
  ({int sourceLineId, int noteIndex, int sourceBookId})? noteSource(int lineId, int bookId) {
    final s = noteSourceOf(lineId);
    if (s == null) return null;
    return (sourceLineId: s.sourceLineId, noteIndex: s.noteIndex, sourceBookId: sourceBookOf(bookId) ?? -1);
  }

  /// Lines per [bookLines] chunk.
  static const bookLinesChunk = 2000;

  /// Text bytes (UTF-16, 2 per char) per [bookLines] chunk (issue #20): a
  /// chunk stops at this many decoded bytes even before [bookLinesChunk]
  /// lines (at least one line), so a book of huge lines cannot balloon the
  /// feed; the next query's LIMIT shrinks to what fit.
  static const bookLinesChunkBytes = 32000000;

  /// A real book's rows as stored, in `lineIndex, id` order: (line id, raw
  /// content) with zstd frames decoded but nothing else touched (a leading BOM
  /// kept, unlike [bookLines]): what Otzaria's vector text check rebuilds the
  /// book's lines from (`chunker::book_lines` strips only the first row's BOM).
  Iterable<(int, Uint8List)> bookRawRows(int bookId) sync* {
    // CAST AS BLOB: a TEXT row's own bytes (a string read would lose a leading BOM)
    final rows = _select(
        'SELECT l.id, typeof($_contentCol), CAST($_contentCol AS BLOB) FROM line l$_contentJoin'
        ' WHERE l.bookId = ? ORDER BY l.lineIndex, l.id',
        [bookId]);
    for (final r in rows) {
      final id = r[0] as int;
      final raw = r[2] as Uint8List?;
      if (raw == null) {
        yield (id, Uint8List(0));
      } else if (r[1] == 'blob') {
        // the frame's own bytes: utf8.decode would drop a leading BOM
        yield (id, _frameBytes(raw, id));
      } else if (compressed && raw.length >= 8 && raw[0] == 0x4B && raw[1] == 0x4C && raw[2] == 0x55 && raw[3] == 0x76) {
        // "KLUv": base64 text of a frame (or text that only looks like one)
        final Uint8List frame;
        try {
          frame = base64.decode(ascii.decode(raw));
          zstdFrameDictionaryId(frame);
        } on FormatException {
          yield (id, raw);
          continue;
        }
        yield (id, _frameBytes(frame, id));
      } else {
        yield (id, raw);
      }
    }
  }

  /// One book's lines in reading order.
  ///
  /// Read in chunks (keyset on `lineIndex, id`), each one fully read before
  /// it is yielded: no statement or cursor stays open between items, so an
  /// early stop (`take`, `break`, an exception in the consumer) leaks nothing
  /// and a reopen between chunks never closes the DB under a live cursor.
  @override
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
        final line = DbLine(r[0] as int, bookId, r[1] as int, r[2] as int,
            lineText(r[0] as int, r[3]));
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
        a = r[0] as int;
        b = r[1] as int;
        c = r[2] as int;
        final id = noteLineId(b, c);
        chunk.add(DbLine(id, bookId, a, 0, lineText(id, r[3])));
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
  @override
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
        final id = r[0] as int;
        final text = strict ? lineText(id, r[1]) : _lenientText(id, r[1]);
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
  @override
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
        final id = r[0] as int;
        final delta = r[1] as int;
        final text = _lenientText(r[2] as int, r[3]) ?? '';
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
  @override
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
            out[r[0] as int] = r[1] as int;
          }
        }
        return out;
      });

  /// Where each line sits: its book and its `lineIndex` (the host opens the
  /// book at that line). A notes line (issue #13) sits in its virtual notes
  /// book at its note index. Unknown ids are absent.
  @override
  Map<int, LineLocation> lineLocations(Iterable<int> ids) {
    final list = ids.toList();
    if (list.isEmpty) return {};
    final real = <int>{for (final id in list) isVirtualId(id) ? noteSourceOf(id)!.sourceLineId : id}.toList();
    final rows = _readSnapshot(() => {
          for (final chunk in _chunks(real))
            for (final r in _select(
                'SELECT id, bookId, lineIndex FROM line WHERE id IN (${List.filled(chunk.length, '?').join(',')})',
                chunk))
              r[0] as int: (bookId: r[1] as int, lineIndex: r[2] as int),
        });
    final out = <int, LineLocation>{};
    for (final id in list) {
      if (isVirtualId(id)) {
        final src = noteSourceOf(id)!;
        final r = rows[src.sourceLineId];
        if (r != null) out[id] = (bookId: notesBookId(r.bookId), lineIndex: src.noteIndex);
      } else if (rows[id] case final r?) {
        out[id] = r;
      }
    }
    return out;
  }

  /// heRef of line ids, read in chunks of at most 900 ids inside one read
  /// transaction (one snapshot for every chunk).
  @override
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
              r[0] as int: (r[1] as String?) ?? '',
        });
    return {
      for (final id in list)
        if (refs[src[id] ?? id] case final r?) id: r,
    };
  }

  /// Book titles; a notes book is titled with the Hebrew word for notes + its
  /// book's title (issue #13).
  @override
  Map<int, String> bookTitles() {
    final out = {for (final r in _select('SELECT id, title FROM book')) r[0] as int: r[1] as String};
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
    return {for (final r in rs) r[0] as int};
  }

  @override
  Map<int, int>? bookContentHashes() => _readSnapshot(() {
        final has = _hasColumn(_db, 'book', 'contentHash');
        if (!has) return null;
        return {
          for (final r in _select('SELECT id, contentHash FROM book')) r[0] as int: (r[1] as int?) ?? 0
        };
      });

  @override
  Map<int, int> bookLineCounts() => _readSnapshot(() {
        final counts = {
          for (final r in _select('SELECT bookId, COUNT(*) AS n FROM line GROUP BY bookId'))
            r[0] as int: r[1] as int
        };
        if (hasNotes) {
          for (final r in _select(
              'SELECT l.bookId AS b, COUNT(*) AS n FROM line_note n JOIN line l ON l.id = n.lineId GROUP BY l.bookId')) {
            counts[notesBookId(r[0] as int)] = r[1] as int;
          }
        }
        return counts;
      });

  int countLines() => _select('SELECT COUNT(*) FROM line').first[0] as int;

  /// File identity, stored as the index payload to detect a replaced DB.
  @override
  String fingerprint() {
    final s = File(path).statSync();
    return '${s.size}|${s.modified.toUtc().toIso8601String()}';
  }

  /// Closes the connection, statements and codec; idempotent.
  @override
  void close() {
    if (_closed) return;
    _closed = true;
    _closeHandles();
  }

  void _closeHandles() => _db.close();
}

/// One opening of the database: connection, schema probe, decoder, file stamp.
class _Open {
  _Open(this.db, this.split, this.compressed, this.stamp, this.dataVersion, this.notes);
  final SeforimSqlConnection db;
  final bool split, compressed;
  final String stamp;
  final int dataVersion;
  final bool notes;
}
