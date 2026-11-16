// The host's seforim.db access, as the package's host helpers read it
// (SeforimLibrary, SeforimIndexBuilder, updateChangedBooks, the snippet-text
// cache). The package holds no SQLite code: a host plugs in ITS OWN database
// layer (one connection, its own sqlite library) by implementing
// [SeforimDataSource]; package:seforim_search_sqlite is the ready-made one.
import 'dart:async';

/// A line's book and line index ([SeforimDataSource.lineLocations]).
typedef LineLocation = ({int bookId, int lineIndex});

/// One line fed to an index build ([SeforimDataSource.bookLines]).
class DbLine {
  DbLine(this.id, this.bookId, this.lineIndex, this.tocEntryId, this.text);
  final int id, bookId, lineIndex, tocEntryId;
  final String text;
}

/// A snippet's TEXT (KitveiHakodesh Core's FtsSnippetText): the matched line
/// and, for a short window only, the plain context before and after it.
class SnippetText {
  SnippetText(this.lineId, this.text, this.before, this.after);
  final int lineId;
  final String text, before, after;
}

/// Every seforim.db read the package's host helpers make. Ids follow the
/// package's scheme: real line / book ids below 2^51; footnotes (schema 7
/// `line_note`) as virtual notes lines / books (note_ids.dart, issue #13)
/// when [hasNotes].
///
/// Display reads ([lineTexts] .. [sectionsOf]) may answer synchronously or
/// with a Future; they run on the caller's isolate. Build reads
/// ([catalogBookOrder] .. [bookLines]) are synchronous: an index build runs
/// them on its own background isolate, where [SeforimIndexBuilder] calls the
/// host's [SeforimDataSourceOpener] to get an instance.
abstract class SeforimDataSource {
  // -- display reads (the search's host side) --

  /// The texts of these lines (a notes line: its note body). A line with no
  /// text row (or one that cannot be read) is absent.
  FutureOr<Map<int, String>> lineTexts(Iterable<int> ids);

  /// Core's `FetchSnippetTexts`: the texts of [lineIds]; [shortLineIds] also
  /// get their non-empty same-book neighbours within 2 lines on each side,
  /// joined by single spaces in document order (a notes line: no context).
  /// A line with no text row is absent.
  FutureOr<Map<int, SnippetText>> snippetTexts(Iterable<int> lineIds, Iterable<int> shortLineIds);

  /// Each line's book and line index (a notes line: its notes book and note
  /// index). Unknown ids are absent.
  FutureOr<Map<int, LineLocation>> lineLocations(Iterable<int> ids);

  /// Each line's reference text (heRef; a notes line: its source line's).
  FutureOr<Map<int, String>> heRefs(Iterable<int> ids);

  /// Book titles by id, notes books included.
  FutureOr<Map<int, String>> bookTitles();

  /// Each line's section (TOC entry id), for the sameSection grouping. Lines
  /// without one are absent.
  FutureOr<Map<int, int>> sectionsOf(Iterable<int> ids);

  // -- build / update reads (synchronous, on the build isolate) --

  /// Book ids in catalog order (each book's notes book right after it).
  List<int> catalogBookOrder();

  /// How many lines [bookId] feeds (a notes book: its notes).
  int bookLineCount(int bookId);

  /// Line counts per book id as the build feeds them, notes books included
  /// (one read for every book; the per-book update's position plan).
  Map<int, int> bookLineCounts();

  /// The lines of [bookId] in lineIndex order (a notes book: its notes).
  Iterable<DbLine> bookLines(int bookId);

  /// The DB holds footnotes (indexed as virtual notes books).
  bool get hasNotes;

  /// Throws, naming the id, when a line / book id or a note does not fit the
  /// virtual id scheme (note_ids.dart `checkNoteIdLimits`).
  void checkNoteIds();

  /// A stamp of the database the index is built from (recorded in the index
  /// payload; a changed stamp tells the per-book update to look).
  String fingerprint();

  /// Each book's `book.contentHash` (NULL reads as 0); null when the DB has
  /// no such column (it can then only be rebuilt whole).
  Map<int, int>? bookContentHashes();

  /// Releases the source (a library or builder that opened it calls this).
  void close();
}

/// Opens a [SeforimDataSource] on the isolate that calls it (an index build
/// runs on its own isolate). Must be sendable to that isolate: a top-level
/// or static function, or a closure over plain values such as a path.
typedef SeforimDataSourceOpener = SeforimDataSource Function();
