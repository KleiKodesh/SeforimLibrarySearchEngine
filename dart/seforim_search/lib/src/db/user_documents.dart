// Host helpers that turn plain-text files and extracted PDF text into user
// documents for the user index (issue #44). Generic: the package parses no
// PDF; the app extracts each page's text and these helpers shape it the way
// Otzaria's engine does (otzaria_search_engine `api/search_engine.rs`
// add_text_book / add_pdf_book, `hebrew_query.rs`
// normalize_pdf_text_for_indexing / is_probably_garbage_pdf_text).
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import '../../seforim_search.dart';

/// One user document: the host's key (e.g. a file path), its stamp (the
/// host's change marker) and its lines.
class UserDocument {
  const UserDocument(this.key, this.stamp, this.lines);
  final String key, stamp;
  final List<UserDocumentLine> lines;

  /// No usable text (e.g. a scanned PDF without a sidecar). Otzaria then
  /// writes an "empty book" marker so the file is not retried on every run:
  /// that bookkeeping is the host's (it may still [SeforimLibrary.updateUserDocuments]
  /// the empty document: it is listed by [SeforimIndex.documents] with 0 lines).
  bool get isEmpty => lines.isEmpty;
}

/// One page of extracted PDF text; [pageIndex] as the app numbers pages
/// (Otzaria: 0-based).
class PdfPageText {
  const PdfPageText(this.pageIndex, this.text);
  final int pageIndex;
  final String text;
}

/// Otzaria's FNV-1a 64 content fingerprint (`compute_content_fingerprint`,
/// the `textHash` stamp of a text book): FNV-1a over the UTF-8 bytes of the
/// RAW text; never 0 (0 means "none"). As an unsigned 64-bit value in a Dart
/// int (two's complement; use [fingerprintHex] for a stamp string).
int contentFingerprint(String text) => contentFingerprintBytes(utf8.encode(text));

/// [contentFingerprint] over UTF-8 bytes (Otzaria
/// `compute_content_fingerprint_bytes`; invalid UTF-8 is first replaced
/// lossily, as Otzaria's `from_utf8_lossy`).
int contentFingerprintBytes(List<int> bytes) {
  final b = _lossyUtf8(bytes);
  var h = _fnvOffset;
  for (final x in b) {
    h = (h ^ x) * _fnvPrime; // wraps mod 2^64 on the VM
  }
  return h == 0 ? 1 : h;
}

/// Otzaria's `compute_book_metadata_fingerprint` (the `metadataHash` stamp): FNV-1a 64
/// over the title, the category path and the extra facets (sorted by their UTF-8 bytes
/// and deduplicated, as the index keeps them), each as an 8-byte little-endian length
/// then its UTF-8 bytes; never 0.
int bookMetadataFingerprint(String title, String topics, [List<String>? extraFacets]) {
  final f = _Fnv();
  _feedMetadata(f, title, topics, extraFacets ?? const []);
  return f.finish();
}

/// Otzaria's `compute_book_fingerprint` (the `contentHash` stamp): the text as a field,
/// then the [bookMetadataFingerprint] fields; never 0.
int bookFingerprint(String text, String title, String topics, [List<String>? extraFacets]) {
  final f = _Fnv()..field(utf8.encode(text));
  _feedMetadata(f, title, topics, extraFacets ?? const []);
  return f.finish();
}

void _feedMetadata(_Fnv f, String title, String topics, List<String> extraFacets) {
  final facets = [for (final x in extraFacets) utf8.encode(x)]..sort(_compareBytes);
  f
    ..field(utf8.encode(title))
    ..field(utf8.encode(topics));
  List<int>? previous;
  for (final x in facets) {
    if (previous != null && _compareBytes(previous, x) == 0) continue; // Vec::dedup
    f.field(x);
    previous = x;
  }
}

int _compareBytes(List<int> a, List<int> b) {
  final n = a.length < b.length ? a.length : b.length;
  for (var i = 0; i < n; i++) {
    if (a[i] != b[i]) return a[i] - b[i];
  }
  return a.length - b.length;
}

final class _Fnv {
  var h = _fnvOffset;
  void feed(List<int> bytes) {
    for (final x in bytes) {
      h = (h ^ x) * _fnvPrime; // wraps mod 2^64 on the VM
    }
  }

  void field(List<int> bytes) {
    var n = bytes.length;
    final le = List<int>.filled(8, 0);
    for (var i = 0; i < 8; i++) {
      le[i] = n & 0xff;
      n >>= 8;
    }
    feed(le);
    feed(bytes);
  }

  int finish() => h == 0 ? 1 : h;
}

/// A fingerprint as 16 lowercase hex digits (unsigned).
String fingerprintHex(int fingerprint) =>
    (BigInt.from(fingerprint) & ((BigInt.one << 64) - BigInt.one)).toRadixString(16).padLeft(16, '0');

const _fnvOffset = 0xcbf29ce484222325;
const _fnvPrime = 0x100000001b3;

List<int> _lossyUtf8(List<int> bytes) {
  try {
    utf8.decode(bytes);
    return bytes;
  } on FormatException {
    return utf8.encode(utf8.decode(bytes, allowMalformed: true));
  }
}

/// Builders of [UserDocument]s from text files and PDF text.
abstract final class UserDocuments {
  /// A stamp of a file's identity: FNV-1a 64 of `path|size|mtime(us, UTC)`,
  /// as hex. Cheap (no read); changes when the file is rewritten.
  static String fileStamp(String path) {
    final s = File(path).statSync();
    return fingerprintHex(contentFingerprint('$path|${s.size}|${s.modified.toUtc().microsecondsSinceEpoch}'));
  }

  /// A text book's lines as Otzaria's add_text_book reads them: split on
  /// `\n` (a final `\r` is dropped: CRLF files; Otzaria leaves it to its
  /// whitespace normalisation), every line kept with its text as is (the
  /// engine tokenizes it; HTML allowed). A line starting with `<h` is a
  /// heading: it opens the next section (sections count from 1; lines before
  /// the first heading have none) and takes part in no cross-line phrase.
  /// [UserDocumentLine.locator] = the 0-based line number.
  static List<UserDocumentLine> textLines(String text) {
    if (text.isEmpty) return const [];
    final out = <UserDocumentLine>[];
    var section = 0;
    var n = 0;
    for (var line in text.split('\n')) {
      if (line.endsWith('\r')) line = line.substring(0, line.length - 1);
      final heading = line.startsWith('<h');
      if (heading) section++;
      out.add(UserDocumentLine(line, locator: n++, sectionId: section == 0 ? null : section, noCross: heading));
    }
    return out;
  }

  /// [textLines] as a document; [stamp] defaults to the [contentFingerprint]
  /// of [text] (Otzaria's textHash) as hex.
  static UserDocument fromText(String key, String text, {String? stamp}) =>
      UserDocument(key, stamp ?? fingerprintHex(contentFingerprint(text)), textLines(text));

  /// [fromText] over UTF-8 bytes (invalid UTF-8 replaced, as Otzaria).
  static UserDocument fromBytes(String key, Uint8List bytes, {String? stamp}) => UserDocument(
      key,
      stamp ?? fingerprintHex(contentFingerprintBytes(bytes)),
      textLines(utf8.decode(bytes, allowMalformed: true)));

  /// Reads a text file: key = [key] or its path, stamp = [fileStamp].
  static Future<UserDocument> fromTextFile(String path, {String? key}) async {
    final bytes = await File(path).readAsBytes();
    return fromBytes(key ?? path, bytes, stamp: fileStamp(path));
  }

  /// A PDF as one document (Otzaria add_pdf_book): each page's text is split
  /// on `\n`, every line normalised ([normalizePdfTextForIndexing]) and
  /// dropped when [isProbablyGarbagePdfText]; a kept line's locator is its
  /// page's index and its section the page (sectionId = pageIndex + 1, as
  /// sections start at 1). A dropped line keeps its slot as an empty line (no
  /// words, never a hit): no phrase continues across it, while each neighbour
  /// still joins its other side (Otzaria blocks only the side facing a dropped
  /// line). No kept line at all: the PDF has no usable text (scanned): try
  /// [pdfSidecarPages], else the host records the empty book.
  static UserDocument fromPdfPages(String key, String stamp, List<PdfPageText> pages) {
    final out = <UserDocumentLine>[];
    var kept = false;
    for (final p in pages) {
      for (final raw in p.text.split('\n')) {
        final t = normalizePdfTextForIndexing(raw);
        final garbage = isProbablyGarbagePdfText(t);
        kept |= !garbage;
        out.add(UserDocumentLine(garbage ? '' : t, locator: p.pageIndex, sectionId: p.pageIndex + 1));
      }
    }
    return UserDocument(key, stamp, kept ? out : const []);
  }

  /// Otzaria's OCR sidecar of a scanned PDF: `<pdf>.txt` or the PDF's path
  /// with `.txt` for its extension, whichever exists first; its text split on
  /// form feed (`\f`) into pages 0, 1, ... (no form feed: one page). Empty
  /// when there is none.
  static Future<List<PdfPageText>> pdfSidecarPages(String pdfPath) async {
    final dot = pdfPath.lastIndexOf('.');
    final sep = pdfPath.lastIndexOf(RegExp(r'[\\/]'));
    final swapped = dot > sep + 1 ? '${pdfPath.substring(0, dot)}.txt' : '$pdfPath.txt';
    for (final c in {'$pdfPath.txt', swapped}) {
      final f = File(c);
      if (await f.exists()) {
        final text = await f.readAsString();
        final pages = text.contains('\f') ? text.split('\f') : [text];
        return [for (var i = 0; i < pages.length; i++) PdfPageText(i, pages[i])];
      }
    }
    return const [];
  }
}

// -- Otzaria hebrew_query.rs ports --

/// Otzaria `normalize_pdf_text_for_indexing`: HTML stripped (block tags and
/// sup/sub become a space, except a sup/sub around one letter inside a word;
/// whitespace entities a space; other entities and tags dropped), bidi /
/// zero-width invisibles dropped, Hebrew presentation forms decomposed,
/// attached nikud / te'amim stripped, whitespace collapsed and trimmed.
/// Punctuation is kept.
String normalizePdfTextForIndexing(String input) {
  final out = StringBuffer();
  var pending = false;
  void push(int c) {
    if (_isWhitespace(c)) {
      if (out.isNotEmpty) pending = true;
    } else {
      if (pending) {
        out.writeCharCode(0x20);
        pending = false;
      }
      out.writeCharCode(c);
    }
  }

  for (final c in _stripHtml(input.runes.toList())) {
    if (_isInvisible(c)) continue;
    final f = _presentationForms[c];
    for (final x in f ?? [c]) {
      if (!_isAttachedMark(x)) push(x);
    }
  }
  return out.toString();
}

/// Otzaria `is_probably_garbage_pdf_text` over a [normalizePdfTextForIndexing]
/// result: after its query sanitizer (paired readings reduced, quotes
/// folded, separators to spaces, `*[]^$\+.~` and invisibles removed) and
/// without whitespace, true when empty, without Hebrew / ASCII letters and
/// digits, or with too few of them (50+ chars under 10%; 20+ chars under 20%
/// with more non-letters than letters). Needs the native library (the
/// paired-readings step is the engine's, [SeforimIndex.reducePairedReadings]).
bool isProbablyGarbagePdfText(String normalizedText) {
  final compact = [
    for (final c in _sanitizeQuery(normalizedText).runes)
      if (!_isWhitespace(c)) c
  ];
  final total = compact.length;
  if (total == 0) return true;
  bool letterOrDigit(int c) =>
      (c >= 0x05D0 && c <= 0x05EA) || (c >= 0x30 && c <= 0x39) || (c >= 0x41 && c <= 0x5A) || (c >= 0x61 && c <= 0x7A);
  final letters = compact.where(letterOrDigit).length;
  if (letters == 0) return true;
  final nonLetters = total - letters;
  final ratio = letters / total;
  if (total >= 50 && ratio < 0.10) return true;
  if (total >= 20 && ratio < 0.20 && nonLetters > letters) return true;
  return false;
}

/// Otzaria `sanitize_query`.
String _sanitizeQuery(String query) {
  final q = query.contains('(') ? SeforimIndex.reducePairedReadings(query) : query;
  const strip = {0x2A, 0x5B, 0x5D, 0x5E, 0x24, 0x5C, 0x2B, 0x2E, 0x7E, 0x60};
  const space = {0x05BE, 0x2D, 0x7C, 0x2C, 0x3B, 0x3A, 0x21, 0x3F, 0x28, 0x29, 0x7B, 0x7D};
  final b = StringBuffer();
  for (final c in q.runes) {
    if (c == 0x05F4 || c == 0x201C || c == 0x201D) {
      b.write('"');
    } else if (c == 0x05F3 || c == 0x2018 || c == 0x2019) {
      b.write("'");
    } else if (space.contains(c)) {
      b.write(' ');
    } else if (strip.contains(c) || _isInvisible(c)) {
      continue;
    } else {
      b.writeCharCode(c);
    }
  }
  // collapse + trim (only the non-whitespace count is read)
  return b.toString();
}

/// Rust `char::is_whitespace` (Unicode White_Space).
bool _isWhitespace(int c) =>
    (c >= 0x09 && c <= 0x0D) ||
    c == 0x20 ||
    c == 0x85 ||
    c == 0xA0 ||
    c == 0x1680 ||
    (c >= 0x2000 && c <= 0x200A) ||
    c == 0x2028 ||
    c == 0x2029 ||
    c == 0x202F ||
    c == 0x205F ||
    c == 0x3000;

bool _isAttachedMark(int c) => c >= 0x0591 && c <= 0x05C7 && c != 0x05BE && c != 0x05C0 && c != 0x05C3 && c != 0x05C6;

bool _isWordMark(int c) => _isAttachedMark(c) || (c >= 0x0300 && c <= 0x036F) || c == 0xFB1E;

bool _isInvisible(int c) =>
    (c >= 0x200B && c <= 0x200F) || (c >= 0x202A && c <= 0x202E) || (c >= 0x2066 && c <= 0x2069) || c == 0xFEFF;

bool _isScriptInvisible(int c) => _isInvisible(c) || c == 0x034F || c == 0xFB1E;

bool _isHebrewLetter(int c) => c >= 0x05D0 && c <= 0x05EA;

bool _isScriptLetter(int c) {
  final f = _presentationForms[c];
  if (f == null) return _isHebrewLetter(c);
  return f.isNotEmpty && _isHebrewLetter(f.first) && f.skip(1).every(_isAttachedMark);
}

const _breakingTags = {
  'address', 'article', 'aside', 'blockquote', 'br', 'caption', 'center', 'dd', 'div', 'dl', 'dt', //
  'figcaption', 'figure', 'footer', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'header', 'hr', 'li', 'main', //
  'nav', 'ol', 'p', 'pre', 'section', 'table', 'tbody', 'td', 'tfoot', 'th', 'thead', 'tr', 'ul',
};
const _scriptTags = {'sub', 'sup'};

bool _isAsciiAlnum(int c) => (c >= 0x30 && c <= 0x39) || (c >= 0x41 && c <= 0x5A) || (c >= 0x61 && c <= 0x7A);

/// Otzaria `tag_name` of the body r[from, to): (closing, lowercase name).
(bool, String)? _tagName(List<int> r, int from, int to) {
  var i = from;
  final closing = i < to && r[i] == 0x2F;
  if (closing) i++;
  final start = i;
  while (i < to && _isAsciiAlnum(r[i])) {
    i++;
  }
  final len = i - start;
  if (len < 1 || len > 10) return null;
  return (closing, String.fromCharCodes(r.sublist(start, i)).toLowerCase());
}

int _indexOf(List<int> r, int c, int from, int to) {
  for (var i = from; i < to; i++) {
    if (r[i] == c) return i;
  }
  return -1;
}

int _lastIndexOf(List<int> r, int c, int from, int to) {
  for (var i = to - 1; i >= from; i--) {
    if (r[i] == c) return i;
  }
  return -1;
}

bool _isInlineTag(List<int> r, int from, int to) {
  final t = _tagName(r, from, to);
  return t == null || !(_breakingTags.contains(t.$2) || _scriptTags.contains(t.$2));
}

/// Otzaria `trim_script_formatting_start` of r[i, end): the new start.
int _trimFormattingStart(List<int> r, int i, int end) {
  while (true) {
    while (i < end && _isScriptInvisible(r[i])) {
      i++;
    }
    if (i >= end || r[i] != 0x3C) break;
    final gt = _indexOf(r, 0x3E, i + 1, end);
    if (gt < 0 || !_isInlineTag(r, i + 1, gt)) break;
    i = gt + 1;
  }
  return i;
}

/// Otzaria `trim_script_formatting_end` of r[start, e): the new end.
int _trimFormattingEnd(List<int> r, int start, int e) {
  while (true) {
    while (e > start && _isScriptInvisible(r[e - 1])) {
      e--;
    }
    if (e <= start || r[e - 1] != 0x3E) break;
    final body = e - 1;
    final lt = _lastIndexOf(r, 0x3C, start, body);
    if (lt < 0) break;
    if (_indexOf(r, 0x3E, lt + 1, body) >= 0 || !_isInlineTag(r, lt + 1, body)) break;
    e = lt;
  }
  return e;
}

int _trimContextEnd(List<int> r, int start, int e) {
  while (true) {
    var t = _trimFormattingEnd(r, start, e);
    while (t > start && _isWordMark(r[t - 1])) {
      t--;
    }
    if (t == e) return e;
    e = t;
  }
}

int _trimContextStart(List<int> r, int i, int end) {
  while (true) {
    var t = _trimFormattingStart(r, i, end);
    while (t < end && _isWordMark(r[t])) {
      t++;
    }
    if (t == i) return i;
    i = t;
  }
}

bool _marksFootnote(List<int> r, int from, int to) {
  final lower = String.fromCharCodes(r.sublist(from, to)).toLowerCase();
  bool word(int i) => i >= 0 && i < lower.length && (_isAsciiAlnum(lower.codeUnitAt(i)) || lower.codeUnitAt(i) == 0x5F);
  const m = 'footnote-marker';
  for (var i = lower.indexOf(m); i >= 0; i = lower.indexOf(m, i + 1)) {
    if (!word(i - 1) && !word(i + m.length)) return true;
  }
  return false;
}

/// Otzaria `is_word_internal_script`: the open tag r[lt..=gt] wraps one
/// letter (with marks) between letters.
bool _isWordInternalScript(List<int> r, int lt, int gt) {
  final t = _tagName(r, lt + 1, gt);
  if (t == null || t.$1) return false;
  if (_marksFootnote(r, lt + 1, gt)) return false;
  final be = _trimContextEnd(r, 0, lt);
  if (be == 0 || !_isScriptLetter(r[be - 1])) return false;
  final n = r.length;
  final c = _trimFormattingStart(r, gt + 1, n);
  if (c >= n || !_isScriptLetter(r[c])) return false;
  final rest = _trimContextStart(r, c + 1, n);
  if (rest >= n || r[rest] != 0x3C) return false;
  final closeGt = _indexOf(r, 0x3E, rest + 1, n);
  if (closeGt < 0) return false;
  final ct = _tagName(r, rest + 1, closeGt);
  if (ct == null || !ct.$1 || ct.$2 != t.$2) return false;
  final after = _trimContextStart(r, closeGt + 1, n);
  return after < n && _isScriptLetter(r[after]);
}

/// Otzaria `script_open_before`: the tag ending right before the lone letter
/// (and its marks) preceding [lt].
(int, int)? _scriptOpenBefore(List<int> r, int lt) {
  final be = _trimContextEnd(r, 0, lt);
  if (be == 0 || !_isScriptLetter(r[be - 1])) return null;
  final gt = _trimFormattingEnd(r, 0, be - 1) - 1;
  if (gt < 0 || r[gt] != 0x3E) return null;
  final open = _lastIndexOf(r, 0x3C, 0, gt);
  if (open < 0 || _indexOf(r, 0x3E, open + 1, gt) >= 0) return null;
  return (open, gt);
}

/// Otzaria `tag_breaks_word` for the tag r[lt..=gt].
bool _tagBreaksWord(List<int> r, int lt, int gt) {
  final t = _tagName(r, lt + 1, gt);
  if (t == null) return false;
  if (_breakingTags.contains(t.$2)) return true;
  if (!_scriptTags.contains(t.$2)) return false;
  final open = t.$1 ? _scriptOpenBefore(r, lt) : (lt, gt);
  return !(open != null && _isWordInternalScript(r, open.$1, open.$2));
}

const _spaceEntities = ['&nbsp;', '&thinsp;', '&ensp;', '&emsp;'];

int? _spaceEntityLen(List<int> r, int i) {
  for (final e in _spaceEntities) {
    if (i + e.length <= r.length) {
      var ok = true;
      for (var k = 0; k < e.length; k++) {
        if (r[i + k] != e.codeUnitAt(k)) {
          ok = false;
          break;
        }
      }
      if (ok) return e.length;
    }
  }
  return null;
}

/// Otzaria `strip_html_for_indexing` (`for_each_html_stripped_piece_at`).
List<int> _stripHtml(List<int> r) {
  if (!r.contains(0x3C) && !r.contains(0x26)) return r;
  final out = <int>[];
  var i = 0;
  while (i < r.length) {
    final c = r[i];
    if (c == 0x3C) {
      final close = _indexOf(r, 0x3E, i + 1, r.length);
      if (close >= 0) {
        if (_tagBreaksWord(r, i, close)) out.add(0x20);
        i = close + 1;
      } else {
        out.add(c);
        i++;
      }
    } else if (c == 0x26) {
      final len = _spaceEntityLen(r, i);
      if (len != null) {
        out.add(0x20);
        i += len;
        continue;
      }
      var j = i + 1;
      var semi = -1;
      while (j < r.length) {
        if (r[j] == 0x3B) {
          semi = j;
          break;
        }
        if (r[j] == 0x26) {
          j += _spaceEntityLen(r, j) ?? 1;
        } else {
          j++;
        }
      }
      if (semi > i + 1) {
        i = semi + 1;
      } else {
        out.add(c);
        i++;
      }
    } else {
      out.add(c);
      i++;
    }
  }
  return out;
}

/// Otzaria `fold_presentation_form` (U+FB1D..U+FB4F, NFKD).
final Map<int, List<int>> _presentationForms = {
  0xFB1D: [0x05D9, 0x05B4], 0xFB1E: [], 0xFB1F: [0x05F2, 0x05B7], 0xFB20: [0x05E2], 0xFB21: [0x05D0], //
  0xFB22: [0x05D3], 0xFB23: [0x05D4], 0xFB24: [0x05DB], 0xFB25: [0x05DC], 0xFB26: [0x05DD], 0xFB27: [0x05E8],
  0xFB28: [0x05EA], 0xFB2A: [0x05E9, 0x05C1], 0xFB2B: [0x05E9, 0x05C2], 0xFB2C: [0x05E9, 0x05BC, 0x05C1],
  0xFB2D: [0x05E9, 0x05BC, 0x05C2], 0xFB2E: [0x05D0, 0x05B7], 0xFB2F: [0x05D0, 0x05B8], 0xFB30: [0x05D0, 0x05BC],
  0xFB31: [0x05D1, 0x05BC], 0xFB32: [0x05D2, 0x05BC], 0xFB33: [0x05D3, 0x05BC], 0xFB34: [0x05D4, 0x05BC],
  0xFB35: [0x05D5, 0x05BC], 0xFB36: [0x05D6, 0x05BC], 0xFB38: [0x05D8, 0x05BC], 0xFB39: [0x05D9, 0x05BC],
  0xFB3A: [0x05DA, 0x05BC], 0xFB3B: [0x05DB, 0x05BC], 0xFB3C: [0x05DC, 0x05BC], 0xFB3E: [0x05DE, 0x05BC],
  0xFB40: [0x05E0, 0x05BC], 0xFB41: [0x05E1, 0x05BC], 0xFB43: [0x05E3, 0x05BC], 0xFB44: [0x05E4, 0x05BC],
  0xFB46: [0x05E6, 0x05BC], 0xFB47: [0x05E7, 0x05BC], 0xFB48: [0x05E8, 0x05BC], 0xFB49: [0x05E9, 0x05BC],
  0xFB4A: [0x05EA, 0x05BC], 0xFB4B: [0x05D5, 0x05B9], 0xFB4C: [0x05D1, 0x05BF], 0xFB4D: [0x05DB, 0x05BF],
  0xFB4E: [0x05E4, 0x05BF], 0xFB4F: [0x05D0, 0x05DC],
};
