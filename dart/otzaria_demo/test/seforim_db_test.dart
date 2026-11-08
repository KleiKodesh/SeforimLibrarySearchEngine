// Issue #8: idempotent close, safe reopen, chunked book reads, new zstd
// dictionaries through the WAL, strict base64 / UTF-8 decoding.
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
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
      expect(() => db.lineTexts([id]),
          throwsA(isA<FormatException>().having((e) => e.message, 'message', contains('line $id'))));
    }
    db.close();
  });
}
