//! Otzaria's `textHash` of a library book (otzaria_search_engine 7.2.0
//! `library_indexing::library_book_fingerprint`), byte for byte, so an index stamped by the
//! old engine stays valid. Pure: the host reads the rows and notes from SQLite (this crate
//! holds no database code) and passes them in. The plain FNV stamps (`textHash` of a text,
//! `contentHash`, `metadataHash`) are pure Dart in the package (`user_documents.dart`).

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One `line_note` row of a book (`lineIndex, charPos, hasMarker, body`), as the old
/// engine's `next_book_note` reads it: rows with a NULL body or a non-integer
/// `lineIndex` / `noteIndex` are left out by the host, `charPos` is -1 when not an integer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookNoteRow {
    pub line_index: i64,
    pub char_pos: i64,
    pub has_marker: bool,
    pub body: String,
}

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const MIN_DATA_URI_PAYLOAD: usize = 64;

fn contains_data_scheme(bytes: &[u8]) -> bool {
    bytes.windows(5).any(|w| w == b"data:")
}

fn is_data_uri_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b';' | b',' | b'=' | b'.' | b'-')
}

/// `strip_data_uris_for_index` (the app's `IndexingRepository.stripDataUrisForIndex`).
fn strip_data_uris_for_index(text: &str) -> String {
    const SCHEME: &str = "data:";
    let bytes = text.as_bytes();
    let Some(mut match_start) = text.find(SCHEME) else {
        return text.to_string();
    };
    let mut out = String::new();
    let mut copied_up_to = 0;
    loop {
        let mut end = match_start + SCHEME.len();
        while end < bytes.len() && is_data_uri_byte(bytes[end]) {
            end += 1;
        }
        if end - match_start - SCHEME.len() >= MIN_DATA_URI_PAYLOAD {
            out.push_str(&text[copied_up_to..match_start]);
            copied_up_to = end;
        }
        match text[end..].find(SCHEME) {
            Some(next) => match_start = end + next,
            None => break,
        }
    }
    if copied_up_to == 0 {
        return text.to_string();
    }
    out.push_str(&text[copied_up_to..]);
    out
}

/// `prepare_row(bytes, false)`.
fn prepare_row(bytes: &[u8]) -> String {
    strip_data_uris_for_index(&String::from_utf8_lossy(bytes))
}

/// The old engine's `TextBuffer::fingerprinting`: FNV over everything pushed, and a
/// second one that skips the first 3 bytes (a BOM skipped at the end).
struct FingerprintBuffer {
    hashes: [u64; 2],
    len: u64,
    skip: u64,
}

impl FingerprintBuffer {
    fn push(&mut self, text: &[u8]) {
        for (offset, &byte) in text.iter().enumerate() {
            self.hashes[0] = (self.hashes[0] ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
            if self.len + offset as u64 >= 3 {
                self.hashes[1] = (self.hashes[1] ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
            }
        }
        self.len += text.len() as u64;
    }
}

/// `library_book_content_fingerprint` after the read: the `textHash` the old engine's
/// `add_library_books` stamps on a book whose stored rows (`lineIndex, id` order; zstd
/// frames decoded, TEXT as its bytes, NULL empty) are `rows` and whose notes
/// (`lineIndex, noteIndex` order; only for the library DB with `line_note`) are `notes`
/// (`read_book_for_indexing_buffered` with `fingerprint_only`). `None` when the book has
/// no rows or its text is empty (the old engine's `Missing`).
pub fn book_content_fingerprint<'a>(rows: impl IntoIterator<Item = &'a [u8]>, notes: &[BookNoteRow]) -> Option<u64> {
    use super::inline_notes::{rehydrate_row, BookNote, BookNotes};
    let mut notes = notes.iter();
    let mut next_note = notes.next();
    let has_notes = next_note.is_some();
    let mut text = FingerprintBuffer { hashes: [FNV_OFFSET; 2], len: 0, skip: 0 };
    let mut book_rows = 0u64;
    let mut first_row_bom = false;
    let mut has_data_uri = false;
    let mut text_line = 0i64;
    for row in rows {
        let mut bytes = std::borrow::Cow::Borrowed(row);
        if book_rows > 0 {
            text.push(b"\n");
        }
        if has_notes {
            let raw = String::from_utf8_lossy(&bytes);
            let row_text = if book_rows == 0 { raw.strip_prefix('\u{FEFF}').unwrap_or(&raw) } else { &raw };
            let next_line = text_line + 1 + row_text.matches('\n').count() as i64;
            let mut row_notes = BookNotes::new();
            while let Some(note) = next_note.filter(|n| n.line_index < next_line) {
                if note.line_index >= text_line {
                    row_notes.entry(note.line_index).or_default().push(BookNote {
                        char_pos: note.char_pos,
                        has_marker: note.has_marker,
                        body: note.body.clone(),
                    });
                }
                next_note = notes.next();
            }
            let rehydrated = rehydrate_row(row_text, text_line, &row_notes);
            text_line = next_line;
            let replaced = match rehydrated {
                Some(rehydrated) => Some(rehydrated.into_bytes()),
                None if row_text.len() != raw.len() => Some(row_text.as_bytes().to_vec()),
                None => None,
            };
            if let Some(replaced) = replaced {
                bytes = std::borrow::Cow::Owned(replaced);
            }
        }
        if contains_data_scheme(&bytes) {
            has_data_uri = true;
        }
        first_row_bom |= book_rows == 0 && bytes.starts_with(BOM);
        text.push(prepare_row(&bytes).as_bytes());
        book_rows += 1;
    }
    if book_rows == 0 {
        return None;
    }
    if first_row_bom && has_data_uri {
        text.skip = (BOM.len() as u64).min(text.len);
    }
    if text.len - text.skip == 0 {
        return None;
    }
    let hash = match text.skip {
        0 => text.hashes[0],
        _ => text.hashes[1],
    };
    Some(hash.max(1))
}
