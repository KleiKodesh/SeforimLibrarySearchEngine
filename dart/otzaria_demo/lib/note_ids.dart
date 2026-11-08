/// Issue #13: schema 7 footnotes (`line_note`) are indexed as virtual "notes
/// books". One place for the id scheme (the C# host has `SeforimDbNoteIds`).
///
///   R = 2^30; every real line / book id must be below it.
///   virtual line id = R + (lineId << 6) + noteIndex   (lineId < 2^24, noteIndex 0..63)
///   virtual book id = R + bookId
/// The largest virtual line id is 2^31 - 1: fits int32 everywhere.
library;

const int noteIdBase = 1 << 30;
const int maxNoteSourceLineId = (1 << 24) - 1;
const int maxNoteIndex = 63;

/// Where a notes-book hit comes from: note [noteIndex] of line [sourceLineId].
typedef NoteSource = ({int sourceLineId, int noteIndex});

bool isVirtualId(int id) => id >= noteIdBase;

int noteLineId(int lineId, int noteIndex) {
  if (lineId < 0 || lineId > maxNoteSourceLineId) {
    throw StateError('line_note: line id $lineId does not fit the notes id range (must be < 2^24)');
  }
  if (noteIndex < 0 || noteIndex > maxNoteIndex) {
    throw StateError('line_note: noteIndex $noteIndex of line $lineId is outside 0..$maxNoteIndex');
  }
  return noteIdBase + (lineId << 6) + noteIndex;
}

int notesBookId(int bookId) {
  if (bookId < 0 || bookId >= noteIdBase) {
    throw StateError('book id $bookId is in the reserved notes range (must be < 2^30)');
  }
  return noteIdBase + bookId;
}

/// The source of a virtual line id; null for a real one.
NoteSource? noteSourceOf(int lineId) {
  if (!isVirtualId(lineId)) return null;
  final v = lineId - noteIdBase;
  return (sourceLineId: v >> 6, noteIndex: v & 63);
}

/// The source book of a virtual book id; null for a real one.
int? sourceBookOf(int bookId) => isVirtualId(bookId) ? bookId - noteIdBase : null;

/// The build's loud guard: throws, naming the id, when the database does not
/// fit the scheme (null = table/column absent).
void checkNoteIdLimits({int? maxLineId, int? maxBookId, int? maxNoteLineId, int? minNoteIndex, int? maxNoteIdx}) {
  if (maxLineId != null && maxLineId >= noteIdBase) {
    throw StateError('line id $maxLineId is in the reserved notes range (must be < 2^30)');
  }
  if (maxBookId != null && maxBookId >= noteIdBase) {
    throw StateError('book id $maxBookId is in the reserved notes range (must be < 2^30)');
  }
  if (maxNoteLineId != null && maxNoteLineId > maxNoteSourceLineId) {
    throw StateError('line_note: line id $maxNoteLineId does not fit the notes id range (must be < 2^24)');
  }
  if (minNoteIndex != null && minNoteIndex < 0) {
    throw StateError('line_note: noteIndex $minNoteIndex is outside 0..$maxNoteIndex');
  }
  if (maxNoteIdx != null && maxNoteIdx > maxNoteIndex) {
    throw StateError('line_note: noteIndex $maxNoteIdx is outside 0..$maxNoteIndex');
  }
}
