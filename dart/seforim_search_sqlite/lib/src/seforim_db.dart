import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_db.dart';
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

/// A package:sqlite3 [SqlReadConnection]: prepared statements kept by SQL
/// text (bounded), closed with the connection.
class SqliteReadConnection implements SqlReadConnection {
  SqliteReadConnection(this.db);

  /// Opens [path] read-only.
  SqliteReadConnection.open(String path) : db = sqlite3.open(path, mode: OpenMode.readOnly);

  final Database db;
  final _statements = <String, PreparedStatement>{};
  bool _closed = false;

  @override
  List<List<Object?>> select(String sql, [List<Object?> params = const []]) {
    if (_statements.length >= 64 && !_statements.containsKey(sql)) {
      // IN-lists of every remainder size: bound the cache
      for (final stmt in _statements.values) {
        stmt.close();
      }
      _statements.clear();
    }
    final stmt = _statements[sql] ??= db.prepare(sql);
    return stmt.select(params).rows;
  }

  @override
  void execute(String sql) => db.execute(sql);

  @override
  void close() {
    if (_closed) return;
    _closed = true;
    for (final stmt in _statements.values) {
      stmt.close();
    }
    _statements.clear();
    db.close();
  }
}

/// The ready-made [SeforimSqlConnection]: package:sqlite3, read-only, and
/// the zstd decoder of [zstd.dart] (libzstd), loaded on the first frame.
class SqliteSeforimConnection extends SqliteReadConnection implements SeforimSqlConnection {
  SqliteSeforimConnection.open(super.path, {this.zstdLib}) : super.open() {
    try {
      db.execute('PRAGMA cache_size=-65536; PRAGMA mmap_size=268435456; PRAGMA temp_store=MEMORY;');
      // the decoder at open, as before: a missing libzstd or a bad dictionary fails the open
      if (db.select("SELECT 1 FROM sqlite_master WHERE type='table' AND name='zstd_dict'").isNotEmpty) {
        final dicts = _dictionaries();
        if (dicts.isNotEmpty) _zstd = ZstdDictDecoder(Zstd.load(zstdLib), dicts, reload: _dictionaries);
      }
    } catch (_) {
      db.close();
      rethrow;
    }
  }

  final String? zstdLib;
  ZstdDictDecoder? _zstd;

  List<Uint8List> _dictionaries() => [
        for (final r in db.select('SELECT dict FROM zstd_dict'))
          if (r['dict'] != null) r['dict'] as Uint8List,
      ];

  @override
  Uint8List decodeFrame(Uint8List frame) {
    final z = _zstd;
    if (z == null) throw StateError('blob content but no zstd_dict');
    return z.decode(frame);
  }

  @override
  void close() {
    _zstd?.close();
    _zstd = null;
    super.close();
  }
}

/// seforim.db through package:sqlite3 ([SeforimSqlDb] over
/// [SqliteSeforimConnection]).
class SeforimDb extends SeforimSqlDb {
  SeforimDb._(String path, String? zstdLib)
      : super(path, (p) => SqliteSeforimConnection.open(p, zstdLib: zstdLib));

  static SeforimDb open(String path, {String? zstdLib}) => SeforimDb._(path, zstdLib);

  /// A [SeforimDataSourceOpener] for [path] (an index build opens its own
  /// connection on its isolate).
  static SeforimDataSourceOpener opener(String path, {String? zstdLib}) => () => SeforimDb.open(path, zstdLib: zstdLib);

  /// The open package:sqlite3 connection (reopened first if the file changed).
  Database get db => (connection as SqliteSeforimConnection).db;
}
