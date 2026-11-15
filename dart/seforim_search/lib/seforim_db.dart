/// Reading Otzaria / Zayit `seforim.db` for hosts of the engine: the schema
/// probe, zstd line decoding, line texts, snippet texts with short-row
/// context, line locations, sections, the catalog order, footnote ids, and
/// the index build / per-book update from the DB with progress.
///
/// Separate from `seforim_search.dart`: the engine itself knows no SQLite
/// (the index is fed by the host); this library is the host side, shared.
library;

export 'src/db/seforim_db.dart';
export 'src/db/zstd.dart';
export 'src/db/note_ids.dart';
export 'src/db/snippet_texts.dart';
export 'src/db/book_updates.dart';
export 'src/db/index_builder.dart';
export 'src/db/library.dart';
