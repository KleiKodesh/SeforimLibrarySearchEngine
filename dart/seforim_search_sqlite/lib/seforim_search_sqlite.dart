/// The ready-made sqlite implementation of package:seforim_search's
/// [SeforimDataSource] over Otzaria / Zayit `seforim.db` ([SeforimDb]: schema
/// probe, zstd line decoding, notes books), the default DB path, and
/// Otzaria's lexical.db reader ([LexicalDb]). A host with its own database
/// layer implements [SeforimDataSource] instead and does not need this
/// package (nor a second sqlite library).
library;

import 'package:seforim_search/seforim_db.dart';

import 'src/seforim_db.dart';

export 'package:seforim_search/seforim_db.dart';
export 'src/seforim_db.dart';
export 'src/zstd.dart';
export 'src/lexical_db.dart' show LexicalDb, isBlacklisted, indexToken;

/// [SeforimLibrary.open] over seforim.db at [dbPath] (default: the usual
/// install locations, [defaultDbPath]); the library closes the DB.
Future<SeforimLibrary> openSeforimLibrary(String indexDir,
    {String? dbPath, String? userIndexDir, bool createUserIndex = false, String? nativePath}) async {
  final path = dbPath ?? defaultDbPath();
  if (path == null) throw StateError('seforim.db not found');
  final db = SeforimDb.open(path);
  try {
    return await SeforimLibrary.open(indexDir,
        source: db, closeSource: true, userIndexDir: userIndexDir, createUserIndex: createUserIndex, nativePath: nativePath);
  } catch (_) {
    db.close();
    rethrow;
  }
}
