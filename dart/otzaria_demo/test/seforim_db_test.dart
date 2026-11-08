// Issue #8: idempotent close, safe reopen, chunked book reads, new zstd
// dictionaries through the WAL, strict base64 / UTF-8 decoding.
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:otzaria_demo/note_ids.dart';
import 'package:otzaria_demo/seforim_db.dart';
import 'package:otzaria_demo/zstd.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:test/test.dart';

/// Test-only compression side of libzstd (the library binds decompression only).
class _ZstdWriter {
  _ZstdWriter(DynamicLibrary l)
      : _train = l.lookupFunction<Size Function(Pointer<Uint8>, Size, Pointer<Uint8>, Pointer<Size>, Uint32),
            int Function(Pointer<Uint8>, int, Pointer<Uint8>, Pointer<Size>, int)>('ZDICT_trainFromBuffer'),
        _isError = l.lookupFunction<Uint32 Function(Size), int Function(int)>('ZDICT_isError'),
        _createCCtx = l.lookupFunction<Pointer<Void> Function(), Pointer<Void> Function()>('ZSTD_createCCtx'),
        _freeCCtx = l.lookupFunction<Size Function(Pointer<Void>), int Function(Pointer<Void>)>('ZSTD_freeCCtx'),
        _compress = l.lookupFunction<
            Size Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Int32),
            int Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Uint8>, int, Pointer<Uint8>, int,
                int)>('ZSTD_compress_usingDict');

  static _ZstdWriter load() {
    final path = Platform.environment['ZSTD_LIB'];
    for (final c in [
      if (path != null) path,
      if (Platform.isWindows) ...['libzstd.dll', r'C:\Program Files\Git\mingw64\bin\libzstd.dll'],
      if (Platform.isMacOS) ...['libzstd.dylib', '/opt/homebrew/lib/libzstd.dylib'],
      if (Platform.isLinux) ...['libzstd.so.1', 'libzstd.so'],
    ]) {
      try {
        return _ZstdWriter(DynamicLibrary.open(c));
      } catch (_) {}
    }
    throw StateError('libzstd not found');
  }

  final int Function(Pointer<Uint8>, int, Pointer<Uint8>, Pointer<Size>, int) _train;
  final int Function(int) _isError;
  final Pointer<Void> Function() _createCCtx;
  final int Function(Pointer<Void>) _freeCCtx;
  final int Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Uint8>, int, Pointer<Uint8>, int, int) _compress;

  Uint8List train(int seed) {
    final samples = [
      for (var i = 0; i < 2000; i++)
        utf8.encode('line $seed $i alpha beta gamma delta ${(i * 7919 + seed) % 1000} epsilon zeta ${i % 100} eta'),
    ];
    final total = samples.fold<int>(0, (a, s) => a + s.length);
    final buf = malloc<Uint8>(total), sizes = malloc<Size>(samples.length), dict = malloc<Uint8>(4096);
    try {
      var o = 0;
      for (var i = 0; i < samples.length; i++) {
        buf.asTypedList(total).setAll(o, samples[i]);
        sizes[i] = samples[i].length;
        o += samples[i].length;
      }
      final n = _train(dict, 4096, buf, sizes, samples.length);
      if (_isError(n) != 0) throw StateError('ZDICT_trainFromBuffer failed');
      return Uint8List.fromList(dict.asTypedList(n));
    } finally {
      malloc.free(buf);
      malloc.free(sizes);
      malloc.free(dict);
    }
  }

  Uint8List compress(Uint8List dictionary, List<int> data) {
    final cctx = _createCCtx();
    final src = malloc<Uint8>(data.length + 1), dst = malloc<Uint8>(data.length + 1024);
    final d = malloc<Uint8>(dictionary.length);
    try {
      src.asTypedList(data.length).setAll(0, data);
      d.asTypedList(dictionary.length).setAll(0, dictionary);
      final n = _compress(cctx, dst, data.length + 1024, src, data.length, d, dictionary.length, 3);
      if (n < 0 || n > data.length + 1024) throw StateError('compress failed');
      return Uint8List.fromList(dst.asTypedList(n));
    } finally {
      _freeCCtx(cctx);
      malloc.free(src);
      malloc.free(dst);
      malloc.free(d);
    }
  }
}

void main() {
  late Directory dir;
  late _ZstdWriter zw;
  late Uint8List d1, d2, d3;

  setUpAll(() {
    zw = _ZstdWriter.load();
    d1 = zw.train(1);
    d2 = zw.train(2);
    d3 = zw.train(3);
  });
  setUp(() => dir = Directory.systemTemp.createTempSync('seforim_db_test'));
  tearDown(() {
    try {
      dir.deleteSync(recursive: true);
    } catch (_) {}
  });

  /// A Zayit-shaped DB (line.content), optionally compressed with [dicts].
  Database create(String path, {List<Uint8List>? dicts, bool wal = false}) {
    final w = sqlite3.open(path);
    if (wal) w.execute('PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;');
    w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content, '
        'heRef TEXT, tocEntryId INTEGER)');
    if (dicts != null) {
      w.execute('CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB)');
      for (final d in dicts) {
        w.execute('INSERT INTO zstd_dict (dict) VALUES (?)', [d]);
      }
    }
    return w;
  }

  test('decoder: double close; a bad dictionary fails the constructor cleanly', () {
    final z = Zstd.load();
    final dec = ZstdDictDecoder(z, [d1]);
    expect(utf8.decode(dec.decode(zw.compress(d1, utf8.encode('alpha')))), 'alpha');
    dec.close();
    dec.close();
    expect(() => dec.decode(zw.compress(d1, utf8.encode('alpha'))), throwsStateError);
    expect(() => ZstdDictDecoder(z, [d1, Uint8List.fromList(List.filled(16, 7))]), throwsFormatException);
  });

  test('decoder: a missing dictionary id reloads once and retries', () {
    var reloads = 0;
    final dec = ZstdDictDecoder(Zstd.load(), [d1], reload: () {
      reloads++;
      return [d1, d2];
    });
    expect(utf8.decode(dec.decode(zw.compress(d2, utf8.encode('beta')))), 'beta');
    expect(reloads, 1);
    expect(() => dec.decode(zw.compress(d3, utf8.encode('x'))), throwsFormatException);
    expect(reloads, 2);
    dec.close();
  });

  test('db: double close; calls after close throw', () {
    final path = '${dir.path}/a.db';
    create(path).close();
    final db = SeforimDb.open(path);
    db.close();
    db.close();
    expect(() => db.countLines(), throwsStateError);
  });

  test('db: a failed reopen keeps the old handles; close afterwards is clean', () {
    final path = '${dir.path}/b.db';
    final w = create(path, wal: true);
    w.execute("INSERT INTO line VALUES (1, 1, 0, 'alpha', '', 0)");
    final db = SeforimDb.open(path);
    expect(db.countLines(), 1);
    w.execute('CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB)'); // empty: open fails
    expect(() => db.countLines(), throwsStateError);
    db.close();
    db.close();
    w.close();
  });

  test('db: early stop of bookLines releases everything (file can be renamed after close)', () {
    final path = '${dir.path}/c.db';
    final w = create(path);
    const n = SeforimDb.bookLinesChunk * 2 + 17;
    w.execute('BEGIN');
    final ins = w.prepare("INSERT INTO line VALUES (?, 1, ?, 'w', '', 0)");
    for (var i = 0; i < n; i++) {
      ins.execute([n - i, i ~/ 2]); // duplicate lineIndex: the keyset must use the id too
    }
    ins.close();
    w.execute('COMMIT');
    w.close();
    final db = SeforimDb.open(path);
    final all = db.bookLines(1).toList();
    expect(all.length, n);
    for (var i = 1; i < n; i++) {
      final a = all[i - 1], b = all[i];
      expect(a.lineIndex < b.lineIndex || (a.lineIndex == b.lineIndex && a.id < b.id), isTrue);
    }
    expect(db.bookLines(1).take(3).length, 3);
    for (final _ in db.bookLines(1)) {
      break;
    }
    db.close();
    File(path).renameSync('${dir.path}/c2.db');
  });

  test('db: a dictionary appended through the WAL is picked up', () {
    final path = '${dir.path}/d.db';
    final w = create(path, dicts: [d1], wal: true);
    w.execute('INSERT INTO line VALUES (1, 1, 0, ?, \'\', 0)', [zw.compress(d1, utf8.encode('one'))]);
    final stamp = File(path).lastModifiedSync();
    final db = SeforimDb.open(path);
    expect(db.lineTexts([1]), {1: 'one'});
    w.execute('INSERT INTO zstd_dict (dict) VALUES (?)', [d2]);
    w.execute('INSERT INTO line VALUES (2, 1, 1, ?, \'\', 0)', [zw.compress(d2, utf8.encode('two'))]);
    File(path).setLastModifiedSync(stamp); // the WAL commit leaves the main file alone
    expect(db.lineTexts([2]), {2: 'two'});
    db.close();
    w.close();
  });

  test('db: KLUv text, base64 frames, missing dictionary and bad UTF-8', () {
    final path = '${dir.path}/e.db';
    final w = create(path, dicts: [d1]);
    final rows = <int, Object>{
      1: 'KLUv is how it starts',
      2: 'KLUvAAAAAAAA', // valid base64, no frame header
      3: base64.encode(zw.compress(d1, utf8.encode('gamma'))),
      4: base64.encode(zw.compress(d3, utf8.encode('x'))), // dictionary not in the DB
      5: zw.compress(d3, utf8.encode('x')),
      6: zw.compress(d1, [0x61, 0xff, 0xfe]), // malformed UTF-8
    };
    rows.forEach((id, v) => w.execute("INSERT INTO line VALUES (?, 1, ?, ?, '', 0)", [id, id, v]));
    w.close();
    final db = SeforimDb.open(path);
    expect(db.lineTexts([1, 2, 3]), {1: 'KLUv is how it starts', 2: 'KLUvAAAAAAAA', 3: 'gamma'});
    for (final id in [4, 5, 6]) {
      expect(() => db.lineTexts([id], strict: true),
          throwsA(isA<FormatException>().having((e) => e.message, 'message', contains('line $id'))));
    }
    db.close();
  });

  test('db: heRefs and sectionsOf over 33000+ ids read in chunks (issue #9 CB08)', () {
    final path = '${dir.path}/big.db';
    final w = create(path);
    w.execute('CREATE TABLE line_toc (lineId INTEGER PRIMARY KEY, tocEntryId INTEGER)');
    w.execute('BEGIN');
    const n = 34000;
    for (var i = 1; i <= n; i++) {
      w.execute('INSERT INTO line (id, bookId, lineIndex, content, heRef) VALUES (?, 1, ?, ?, ?)',
          [i, i, 'x', 'r$i']);
      w.execute('INSERT INTO line_toc VALUES (?, ?)', [i, i % 7]);
    }
    w.execute('COMMIT');
    w.close();
    final db = SeforimDb.open(path);
    final ids = [for (var i = 1; i <= n + 5; i++) i];
    final refs = db.heRefs(ids);
    expect(refs.length, n);
    expect(refs[1], 'r1');
    expect(refs[n], 'r$n');
    final secs = db.sectionsOf(ids);
    expect(secs.length, n);
    expect(secs[n], n % 7);
    expect(db.countLines(), n); // the read transaction was closed
    db.close();
  });
  test('note ids: boundaries, roundtrip and the guard (issue #13)', () {
    expect(noteLineId(maxNoteSourceLineId, 63), (1 << 31) - 1);
    expect(noteLineId(0, 0), noteIdBase);
    expect(noteSourceOf(noteLineId(12345, 7)), (sourceLineId: 12345, noteIndex: 7));
    expect(noteSourceOf(noteIdBase - 1), isNull);
    expect(sourceBookOf(notesBookId(8440)), 8440);
    expect(() => noteLineId(1 << 24, 0), throwsStateError);
    expect(() => noteLineId(1, 64), throwsStateError);
    expect(() => notesBookId(noteIdBase), throwsStateError);
    checkNoteIdLimits(maxLineId: noteIdBase - 1, maxBookId: noteIdBase - 1, maxNoteLineId: maxNoteSourceLineId,
        minNoteIndex: 0, maxNoteIdx: 63);
    expect(() => checkNoteIdLimits(maxLineId: noteIdBase),
        throwsA(isA<StateError>().having((e) => e.message, 'message', contains('$noteIdBase'))));
    expect(() => checkNoteIdLimits(maxBookId: noteIdBase), throwsStateError);
    expect(() => checkNoteIdLimits(maxNoteLineId: 1 << 24), throwsStateError);
    expect(() => checkNoteIdLimits(maxNoteIdx: 64), throwsStateError);
    expect(() => checkNoteIdLimits(minNoteIndex: -1), throwsStateError);
  });

  test('db: schema 7 notes books: order, feed, texts, snippets, source mapping, BOM (issue #13)', () {
    final path = '${dir.path}/notes.db';
    final w = sqlite3.open(path);
    w.execute('CREATE TABLE category (id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER)');
    w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER)');
    w.execute("INSERT INTO category VALUES (1, NULL, 'c', 0)");
    w.execute("INSERT INTO book VALUES (1, 1, 'one', 0), (2, 1, 'two', 1), (3, 1, 'three', 2)");
    w.execute('CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, tocEntryId INTEGER)');
    w.execute('CREATE TABLE line_content (id INTEGER PRIMARY KEY, content)');
    w.execute('CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB)');
    w.execute('INSERT INTO zstd_dict (dict) VALUES (?)', [d1]);
    w.execute('CREATE TABLE line_note (lineId INTEGER NOT NULL, noteIndex INTEGER NOT NULL, charPos INTEGER, '
        'hasMarker INTEGER, body, PRIMARY KEY (lineId, noteIndex)) WITHOUT ROWID');
    // book 1: lines 11 (index 1) and 10 (index 0); book 3: line 30; book 2: none
    for (final (id, book, idx, text) in [(10, 1, 0, 'a'), (11, 1, 1, 'b'), (20, 2, 0, 'c'), (30, 3, 0, '﻿d')]) {
      w.execute("INSERT INTO line VALUES (?, ?, ?, 'ref$id', 0)", [id, book, idx]);
      w.execute('INSERT INTO line_content VALUES (?, ?)', [id, text]);
    }
    w.execute('INSERT INTO line_note VALUES (11, 0, 0, 1, ?)', ['zeta note']);
    w.execute('INSERT INTO line_note VALUES (10, 1, 0, 1, ?)', [zw.compress(d1, utf8.encode('﻿packed note'))]);
    w.execute('INSERT INTO line_note VALUES (10, 0, 0, 1, ?)', [base64.encode(zw.compress(d1, utf8.encode('b64 note')))]);
    w.execute('INSERT INTO line_note VALUES (30, 63, 0, 0, ?)', ['last note']);
    w.close();
    final db = SeforimDb.open(path);
    expect(db.hasNotes, isTrue);
    db.checkNoteIds();
    final n1 = notesBookId(1), n3 = notesBookId(3);
    expect(db.catalogBookOrder(), [1, n1, 2, 3, n3]);
    final feed = [for (final b in db.catalogBookOrder()) ...db.bookLines(b)];
    expect([for (final l in feed) l.id],
        [10, 11, noteLineId(10, 0), noteLineId(10, 1), noteLineId(11, 0), 20, 30, noteLineId(30, 63)]);
    expect([for (final l in feed) l.text], ['a', 'b', 'b64 note', 'packed note', 'zeta note', 'c', 'd', 'last note']);
    expect(feed[2].bookId, n1);
    expect(feed.last.bookId, n3);
    expect(feed.last.tocEntryId, 0);
    expect(db.bookLineCount(n1), 3);
    expect(db.bookLineCount(1), 2);
    final v = noteLineId(10, 1);
    expect(db.lineTexts([10, v, noteLineId(20, 0)]), {10: 'a', v: 'packed note'});
    expect(db.readLine(v).text, 'packed note');
    expect(db.readLine(noteLineId(20, 0)).status, LineStatus.missing);
    final sn = db.snippetTexts([noteLineId(30, 63)], [v, 11]);
    expect((sn[v]!.text, sn[v]!.before, sn[v]!.after), ('packed note', '', '')); // the note alone
    expect(sn[noteLineId(30, 63)]!.text, 'last note');
    expect(sn[11]!.before, 'a'); // a real line keeps its real neighbours only
    expect(db.noteSource(v, n1), (sourceLineId: 10, noteIndex: 1, sourceBookId: 1));
    expect(db.noteSource(10, 1), isNull);
    expect(db.heRefs([v, 11]), {v: 'ref10', 11: 'ref11'});
    final titles = db.bookTitles();
    expect(titles[n1], '${SeforimDb.notesTitlePrefix} one');
    expect(titles.containsKey(notesBookId(2)), isFalse);
    expect(db.unreadableLineIds, isEmpty);
    db.close();
  });

  test('db: no line_note table, nothing changes (issue #13)', () {
    final path = '${dir.path}/plain.db';
    final w = create(path);
    w.execute("INSERT INTO line VALUES (1, 1, 0, '﻿alpha', '', 0)");
    w.close();
    final db = SeforimDb.open(path);
    expect(db.hasNotes, isFalse);
    db.checkNoteIds();
    expect(db.lineTexts([1]), {1: 'alpha'}); // BOM stripped
    db.close();
  });

  test('db: the build guard names a real id in the reserved range (issue #13)', () {
    final path = '${dir.path}/guard.db';
    final w = create(path);
    w.execute('CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT)');
    w.execute('CREATE TABLE line_note (lineId INTEGER, noteIndex INTEGER, charPos INTEGER, hasMarker INTEGER, body)');
    w.execute("INSERT INTO line VALUES (?, 1, 0, 'x', '', 0)", [noteIdBase]);
    w.close();
    final db = SeforimDb.open(path);
    expect(db.checkNoteIds, throwsA(isA<StateError>().having((e) => e.message, 'message', contains('$noteIdBase'))));
    db.close();
  });

  test('db: an unreadable row is counted, not thrown, on the search path (issue #13)', () {
    final path = '${dir.path}/bad.db';
    final w = create(path, dicts: [d1]);
    w.execute("INSERT INTO line VALUES (1, 1, 0, 'good', '', 0)");
    w.execute("INSERT INTO line VALUES (2, 1, 1, ?, '', 0)", [zw.compress(d3, utf8.encode('x'))]);
    w.execute("INSERT INTO line VALUES (3, 1, 2, 'next', '', 0)");
    w.close();
    final db = SeforimDb.open(path);
    expect(db.lineTexts([1, 2, 3]), {1: 'good', 3: 'next'});
    expect(db.unreadableLineIds, {2});
    final sn = db.snippetTexts([2], [1]);
    expect(sn[2]!.text, ''); // read as empty
    expect(sn[1]!.after, 'next'); // the bad neighbour is skipped as empty
    final r = db.readLine(2);
    expect(r.status, LineStatus.unreadable);
    expect(r.error.toString(), contains('line 2'));
    expect(db.readLine(1).text, 'good');
    db.clearUnreadable();
    expect(db.unreadableLineIds, isEmpty);
    db.close();
  });
}
