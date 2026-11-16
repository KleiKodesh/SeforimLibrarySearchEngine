/// The host side of the engine over Otzaria / Zayit `seforim.db`: the
/// ready-made [SeforimLibrary], the index build / per-book update with
/// progress ([SeforimIndexBuilder]), the snippet-text cache, grouping,
/// footnote ids and user-document helpers.
///
/// No SQLite here: every database read goes through [SeforimDataSource],
/// which the host implements over ITS OWN database layer (or takes the
/// ready-made sqlite one from package:seforim_search_sqlite). Separate from
/// `seforim_search.dart`: the engine itself knows no database.
library;

export 'src/db/data_source.dart';
export 'src/db/note_ids.dart';
export 'src/db/snippet_texts.dart';
export 'src/db/book_updates.dart';
export 'src/db/index_builder.dart';
export 'src/db/library.dart';
export 'src/db/grouping.dart';
export 'src/db/user_documents.dart';
