// User-document helpers (issue #44): text files, PDF text normalisation and
// garbage check (ports of Otzaria's hebrew_query.rs), per-page lines, the
// OCR sidecar, the FNV-1a text fingerprint. Test data is ASCII / escaped.
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_db.dart';
import 'package:test/test.dart';

void main() {
  test('content fingerprint = FNV-1a 64 over UTF-8, never 0', () {
    expect(fingerprintHex(contentFingerprint('')), 'cbf29ce484222325');
    expect(fingerprintHex(contentFingerprint('a')), 'af63dc4c8601ec8c');
    expect(contentFingerprintBytes(utf8.encode('\u05d0b')), contentFingerprint('\u05d0b'));
    // invalid UTF-8 is replaced first (lossy), as Otzaria
    expect(contentFingerprintBytes([0x61, 0xff]), contentFingerprint('a\ufffd'));
  });

  test('text lines: split on newline, headings open sections and do not cross', () {
    final d = UserDocuments.fromText('k', 'intro\r\n<h1>A</h1>\nbody\n\nend');
    expect(d.stamp, fingerprintHex(contentFingerprint('intro\r\n<h1>A</h1>\nbody\n\nend')));
    expect([for (final l in d.lines) l.text], ['intro', '<h1>A</h1>', 'body', '', 'end']);
    expect([for (final l in d.lines) l.locator], [0, 1, 2, 3, 4]);
    expect([for (final l in d.lines) l.sectionId], [null, 1, 1, 1, 1]);
    expect([for (final l in d.lines) l.noCross], [false, true, false, false, false]);
    expect(UserDocuments.fromText('k', '').isEmpty, isTrue);
    final b = UserDocuments.fromBytes('k', Uint8List.fromList(utf8.encode('x\ny')));
    expect(b.lines.length, 2);
  });

  test('normalizePdfTextForIndexing (Otzaria port)', () {
    expect(normalizePdfTextForIndexing('  a \u200f b\t\tc  '), 'a b c');
    // marks stripped, presentation forms folded (shin + shin dot -> shin)
    expect(normalizePdfTextForIndexing('\u05d0\u05b8\u05d1 \ufb2a'), '\u05d0\u05d1 \u05e9');
    // block tags and entities
    expect(normalizePdfTextForIndexing('a<p>b</p>c&nbsp;d&amp;e<b>f</b>g'), 'a b c defg');
    expect(normalizePdfTextForIndexing('a < b'), 'a < b'); // unterminated: kept
    // sup/sub split words, except one letter inside a Hebrew word
    expect(normalizePdfTextForIndexing('word<sup>30</sup>'), 'word 30');
    expect(normalizePdfTextForIndexing('\u05d0<sup>\u05d1</sup>\u05d2'), '\u05d0\u05d1\u05d2');
    expect(normalizePdfTextForIndexing('\u05d0<sup class="footnote-marker">\u05d1</sup>\u05d2'),
        '\u05d0 \u05d1 \u05d2');
  });

  test('isProbablyGarbagePdfText (Otzaria port)', () {
    expect(isProbablyGarbagePdfText(''), isTrue);
    expect(isProbablyGarbagePdfText('... ---'), isTrue);
    expect(isProbablyGarbagePdfText('a real line of text'), isFalse);
    expect(isProbablyGarbagePdfText('ab ${'#' * 60}'), isTrue); // 50+ chars, under 10% letters
    expect(isProbablyGarbagePdfText('abcd ${'#' * 18}'), isTrue); // 20+ chars, under 20%, more non-letters
    expect(isProbablyGarbagePdfText('abcde ${'#' * 15}'), isFalse);
  });

  test('PDF pages: lines per page, garbage dropped, neighbours do not cross', () {
    final d = UserDocuments.fromPdfPages('c:/b.pdf', 'st', const [
      PdfPageText(0, 'first line\n####################\nthird line'),
      PdfPageText(1, 'page two'),
    ]);
    expect([for (final l in d.lines) l.text], ['first line', 'third line', 'page two']);
    expect([for (final l in d.lines) l.locator], [0, 0, 1]);
    expect([for (final l in d.lines) l.sectionId], [1, 1, 2]);
    expect([for (final l in d.lines) l.noCross], [true, true, false]);
    expect(UserDocuments.fromPdfPages('k', 's', const [PdfPageText(0, '....')]).isEmpty, isTrue);
  });

  test('PDF OCR sidecar pages and file stamp', () async {
    final tmp = Directory.systemTemp.createTempSync('se_userdocs_');
    try {
      final pdf = '${tmp.path}/book.pdf';
      expect(await UserDocuments.pdfSidecarPages(pdf), isEmpty);
      File('${tmp.path}/book.txt').writeAsStringSync('one\ftwo');
      final pages = await UserDocuments.pdfSidecarPages(pdf);
      expect([for (final p in pages) (p.pageIndex, p.text)], [(0, 'one'), (1, 'two')]);
      File('$pdf.txt').writeAsStringSync('only');
      expect((await UserDocuments.pdfSidecarPages(pdf)).single.text, 'only'); // <pdf>.txt first
      final f = File('${tmp.path}/n.txt')..writeAsStringSync('a\nb');
      final d = await UserDocuments.fromTextFile(f.path);
      expect(d.key, f.path);
      expect(d.stamp, UserDocuments.fileStamp(f.path));
      expect(d.lines.length, 2);
    } finally {
      tmp.deleteSync(recursive: true);
    }
  });
}
