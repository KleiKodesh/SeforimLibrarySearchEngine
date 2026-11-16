import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

/// Minimal dart:ffi binding to the system libzstd (dictionary decompression
/// only). No pub.dev zstd package exposes dictionaries portably, and libzstd
/// is on every Linux/macOS box and ships with Git for Windows, so we bind it.
///
/// Resolution: explicit path, else `ZSTD_LIB` env var, else the platform
/// names (`libzstd.dll`, `zstd.dll`, `libzstd.so.1`, `libzstd.so`,
/// `libzstd.1.dylib`, `libzstd.dylib`).
final class Zstd {
  Zstd._(DynamicLibrary l)
      : _createDCtx = l.lookupFunction<Pointer<Void> Function(), Pointer<Void> Function()>('ZSTD_createDCtx'),
        _freeDCtx = l.lookupFunction<Size Function(Pointer<Void>), int Function(Pointer<Void>)>('ZSTD_freeDCtx'),
        _createDDict = l.lookupFunction<Pointer<Void> Function(Pointer<Uint8>, Size),
            Pointer<Void> Function(Pointer<Uint8>, int)>('ZSTD_createDDict'),
        _freeDDict = l.lookupFunction<Size Function(Pointer<Void>), int Function(Pointer<Void>)>('ZSTD_freeDDict'),
        _decompress = l.lookupFunction<
            Size Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Pointer<Void>),
            int Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Uint8>, int,
                Pointer<Void>)>('ZSTD_decompress_usingDDict'),
        _contentSize = l.lookupFunction<Uint64 Function(Pointer<Uint8>, Size), int Function(Pointer<Uint8>, int)>(
            'ZSTD_getFrameContentSize'),
        _isError = l.lookupFunction<Uint32 Function(Size), int Function(int)>('ZSTD_isError'),
        _errorName = l.lookupFunction<Pointer<Utf8> Function(Size), Pointer<Utf8> Function(int)>('ZSTD_getErrorName');

  final Pointer<Void> Function() _createDCtx;
  final int Function(Pointer<Void>) _freeDCtx;
  final Pointer<Void> Function(Pointer<Uint8>, int) _createDDict;
  final int Function(Pointer<Void>) _freeDDict;
  final int Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Uint8>, int, Pointer<Void>) _decompress;
  final int Function(Pointer<Uint8>, int) _contentSize;
  final int Function(int) _isError;
  final Pointer<Utf8> Function(int) _errorName;

  static Zstd load([String? path]) {
    path ??= Platform.environment['ZSTD_LIB'];
    final candidates = path != null
        ? [path]
        : Platform.isWindows
            ? ['libzstd.dll', 'zstd.dll', r'C:\Program Files\Git\mingw64\bin\libzstd.dll']
            : Platform.isMacOS
                ? ['libzstd.1.dylib', 'libzstd.dylib', '/opt/homebrew/lib/libzstd.dylib', '/usr/local/lib/libzstd.dylib']
                : ['libzstd.so.1', 'libzstd.so'];
    Object? last;
    for (final c in candidates) {
      try {
        return Zstd._(DynamicLibrary.open(c));
      } catch (e) {
        last = e;
      }
    }
    throw StateError('libzstd not found (tried ${candidates.join(', ')}); set ZSTD_LIB. $last');
  }
}

/// Decodes zstd frames with the dictionaries of one database. One per
/// isolate/connection (a DCtx is not thread-safe). Call [close] when done;
/// [close] is idempotent.
///
/// [reload] re-reads the database's dictionaries: a frame naming an id this
/// decoder lacks (a writer appended a dictionary through the WAL) reloads
/// them once and retries, as Otzaria does (issue #8).
final class ZstdDictDecoder {
  ZstdDictDecoder(this._z, List<Uint8List> dictionaries, {List<Uint8List> Function()? reload})
      : _dctx = _z._createDCtx(),
        _reload = reload {
    try {
      if (_dctx == nullptr) throw StateError('ZSTD_createDCtx failed');
      _addDictionaries(dictionaries);
    } catch (_) {
      close(); // frees the dictionaries loaded so far and the DCtx
      rethrow;
    }
  }

  static const int maxLineBytes = 16 * 1024 * 1024;
  final Zstd _z;
  Pointer<Void> _dctx;
  final List<Uint8List> Function()? _reload;
  final Map<int, Pointer<Void>> _ddicts = {};
  Pointer<Uint8> _src = nullptr, _dst = nullptr;
  int _srcCap = 0, _dstCap = 0;
  bool _closed = false;

  /// Loads the dictionaries whose id is not loaded yet; true when one was added.
  bool _addDictionaries(List<Uint8List> dictionaries) {
    var added = false;
    for (final d in dictionaries) {
      final id = readDictionaryId(d);
      if (_ddicts.containsKey(id)) continue;
      final p = malloc<Uint8>(d.length);
      p.asTypedList(d.length).setAll(0, d);
      final dd = _z._createDDict(p, d.length); // copies the dictionary
      malloc.free(p);
      if (dd == nullptr) throw FormatException('zstd dictionary $id could not be loaded');
      _ddicts[id] = dd;
      added = true;
    }
    return added;
  }

  /// The id zstd stamped into the dictionary (NOT the table's id column).
  static int readDictionaryId(Uint8List d) {
    if (d.length < 8) throw const FormatException('zstd_dict row too short');
    final bd = ByteData.sublistView(d);
    if (bd.getUint32(0, Endian.little) != 0xEC30A437) throw const FormatException('zstd_dict row lacks dictionary magic');
    return bd.getUint32(4, Endian.little);
  }

  /// The dictionary id in a frame header.
  static int readFrameDictionaryId(Uint8List f) {
    if (f.length < 5 || ByteData.sublistView(f).getUint32(0, Endian.little) != 0xFD2FB528) {
      throw const FormatException('not a zstd frame');
    }
    final fhd = f[4];
    var width = fhd & 3;
    if (width == 3) width = 4;
    if (width == 0) throw const FormatException('zstd frame carries no dictionary id');
    final offset = (fhd & 0x20) != 0 ? 5 : 6;
    if (f.length < offset + width) throw const FormatException('truncated zstd frame header');
    var id = 0;
    for (var i = 0; i < width; i++) {
      id |= f[offset + i] << (8 * i);
    }
    return id;
  }

  Uint8List decode(Uint8List frame) {
    if (_closed) throw StateError('ZstdDictDecoder is closed');
    final id = readFrameDictionaryId(frame);
    var dd = _ddicts[id];
    // a dictionary appended since this decoder was built: re-read once, retry
    if (dd == null && _reload != null && _addDictionaries(_reload())) dd = _ddicts[id];
    if (dd == null) throw FormatException('dictionary $id not in zstd_dict');
    if (frame.length > _srcCap) {
      if (_src != nullptr) malloc.free(_src);
      _srcCap = frame.length * 2;
      _src = malloc<Uint8>(_srcCap);
    }
    _src.asTypedList(frame.length).setAll(0, frame);
    final declared = _z._contentSize(_src, frame.length);
    // ZSTD_CONTENTSIZE_UNKNOWN/ERROR are -1/-2 as u64 → negative here.
    if (declared < 0 || declared > maxLineBytes) throw FormatException('bad zstd content size $declared');
    if (declared == 0) return Uint8List(0);
    if (declared > _dstCap) {
      if (_dst != nullptr) malloc.free(_dst);
      _dstCap = declared < 4096 ? 4096 : declared;
      _dst = malloc<Uint8>(_dstCap);
    }
    final n = _z._decompress(_dctx, _dst, declared, _src, frame.length, dd);
    if (_z._isError(n) != 0) throw FormatException('zstd: ${_z._errorName(n).toDartString()}');
    return Uint8List.fromList(_dst.asTypedList(n));
  }

  void close() {
    if (_closed) return;
    _closed = true;
    for (final d in _ddicts.values) {
      _z._freeDDict(d);
    }
    _ddicts.clear();
    if (_dctx != nullptr) _z._freeDCtx(_dctx);
    _dctx = nullptr;
    if (_src != nullptr) malloc.free(_src);
    if (_dst != nullptr) malloc.free(_dst);
    _src = _dst = nullptr;
    _srcCap = _dstCap = 0;
  }
}
