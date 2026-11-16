import 'package:seforim_search/seforim_db.dart' show LexicalDictionary;

import 'seforim_db.dart' show SqliteReadConnection;

/// Otzaria's lexical.db ([LexicalDictionary]) through package:sqlite3.
class LexicalDb extends LexicalDictionary {
  LexicalDb._(super.connection);

  /// Opens [path] read-only; throws when it is not a lexical.db (Otzaria's
  /// `MagicDictionary::open`: every statement must run).
  static LexicalDb open(String path) {
    final c = SqliteReadConnection.open(path);
    try {
      c.execute('PRAGMA query_only = ON;');
    } catch (_) {
      c.close();
      rethrow;
    }
    return LexicalDb._(c);
  }

  /// [open], or null when the file is missing or not a lexical.db (Otzaria's
  /// `set_magic_dictionary_path` returning false: no dictionary, no forms).
  static LexicalDb? tryOpen(String path) {
    try {
      return open(path);
    } catch (_) {
      return null;
    }
  }
}
