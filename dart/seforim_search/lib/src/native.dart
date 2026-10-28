import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

/// Thrown when the native engine reports an error.
class SeforimEngineException implements Exception {
  SeforimEngineException(this.message);
  final String message;
  @override
  String toString() => 'SeforimEngineException: $message';
}

/// Thrown when a search's [SearchCancelToken] was cancelled.
class SearchCancelledException extends SeforimEngineException {
  SearchCancelledException(super.message);
  @override
  String toString() => 'SearchCancelledException: $message';
}

/// `SeBuffer {u32* data; usize len; usize cap}` — returned by value.
final class SeBuffer extends Struct {
  external Pointer<Uint32> data;
  @Size()
  external int len;
  @Size()
  external int cap;
}

typedef _CloseC = Void Function(Pointer<Void>);
typedef _CloseD = void Function(Pointer<Void>);

/// The loaded native library and its entry points.
///
/// Loading order: [SeforimNative.load] with an explicit path, else the
/// `SEFORIM_SEARCH_NATIVE` environment variable, else the platform file name
/// (`seforim_search_native.dll` / `libseforim_search_native.so` /
/// `libseforim_search_native.dylib`) through the OS loader search path.
final class SeforimNative {
  SeforimNative._(this.lib)
      : version = lib.lookupFunction<Pointer<Utf8> Function(), Pointer<Utf8> Function()>('se_version'),
        lastError = lib.lookupFunction<Pointer<Utf8> Function(), Pointer<Utf8> Function()>('se_last_error'),
        stringFree = lib.lookupFunction<_CloseC, _CloseD>('se_string_free'),
        bufferFree = lib.lookupFunction<Void Function(SeBuffer), void Function(SeBuffer)>('se_buffer_free'),
        indexCreate = lib.lookupFunction<
            Pointer<Void> Function(Pointer<Uint8>, Size, Pointer<Uint8>, Size),
            Pointer<Void> Function(Pointer<Uint8>, int, Pointer<Uint8>, int)>('se_index_create'),
        indexOpen = lib.lookupFunction<Pointer<Void> Function(Pointer<Uint8>, Size),
            Pointer<Void> Function(Pointer<Uint8>, int)>('se_index_open'),
        indexExists = lib.lookupFunction<Int32 Function(Pointer<Uint8>, Size),
            int Function(Pointer<Uint8>, int)>('se_index_exists'),
        indexInfo = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_index_info'),
        userRegexKind = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Uint8>, Size),
            Pointer<Utf8> Function(Pointer<Uint8>, int)>('se_user_regex_kind'),
        reducePairedReadings = lib.lookupFunction<Pointer<Uint8> Function(Pointer<Uint8>, Size, Pointer<Size>),
            Pointer<Uint8> Function(Pointer<Uint8>, int, Pointer<Size>)>('se_reduce_paired_readings'),
        bytesFree = lib.lookupFunction<Void Function(Pointer<Uint8>, Size), void Function(Pointer<Uint8>, int)>(
            'se_bytes_free'),
        indexBuildFmSidecar = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_index_build_fm_sidecar'),
        indexHasFmSidecar = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_index_has_fm_sidecar'),
        writerOpen = lib.lookupFunction<Pointer<Void> Function(Pointer<Void>, Size, Size),
            Pointer<Void> Function(Pointer<Void>, int, int)>('se_writer_open'),
        writerAdd = lib.lookupFunction<
            Int32 Function(Pointer<Void>, Uint64, Uint64, Uint64, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, int, int, int, Pointer<Uint8>, int)>('se_writer_add'),
        writerAddLine = lib.lookupFunction<
            Int32 Function(Pointer<Void>, Uint64, Uint64, Uint64, Uint64, Uint32, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, int, int, int, int, int, Pointer<Uint8>, int)>('se_writer_add_line'),
        searchCopyNextLines = lib.lookupFunction<Uint64 Function(Pointer<Void>, Uint64, Uint64, Pointer<Uint64>),
            int Function(Pointer<Void>, int, int, Pointer<Uint64>)>('se_search_copy_next_lines'),
        locatePairs = lib.lookupFunction<
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>,
                Pointer<Size>, Size),
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>,
                Pointer<Size>, int)>('se_search_locate_pairs'),
        countBatchPairs = lib.lookupFunction<
            Int64 Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>,
                Pointer<Size>, Size),
            int Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>,
                Pointer<Size>, int)>('se_search_count_batch_pairs'),
        writerDelete = lib.lookupFunction<Int32 Function(Pointer<Void>, Uint64),
            int Function(Pointer<Void>, int)>('se_writer_delete'),
        writerCommit = lib.lookupFunction<Int32 Function(Pointer<Void>, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, Pointer<Uint8>, int)>('se_writer_commit'),
        writerRollback = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_writer_rollback'),
        writerMergeAndClose = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_writer_merge_and_close'),
        search = lib.lookupFunction<Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, Size),
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int)>('se_search'),
        searchHitCount = lib.lookupFunction<Uint64 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_search_hit_count'),
        searchCopyHits = lib.lookupFunction<Uint64 Function(Pointer<Void>, Uint64, Uint64, Pointer<Uint64>),
            int Function(Pointer<Void>, int, int, Pointer<Uint64>)>('se_search_copy_hits'),
        locateLines = lib.lookupFunction<
            SeBuffer Function(Pointer<Void>, Pointer<Int64>, Pointer<Size>, Size, Pointer<Uint64>,
                Pointer<Pointer<Uint8>>, Pointer<Size>),
            SeBuffer Function(Pointer<Void>, Pointer<Int64>, Pointer<Size>, int, Pointer<Uint64>,
                Pointer<Pointer<Uint8>>, Pointer<Size>)>('se_search_locate_lines'),
        locateBatch = lib.lookupFunction<
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Size),
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int)>('se_search_locate_batch'),
        takeNewTerms = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_search_take_new_terms'),
        searchCancellable = lib.lookupFunction<
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Void>),
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>)>('se_search_cancellable'),
        lastErrorCancelled = lib.lookupFunction<Int32 Function(), int Function()>('se_last_error_cancelled'),
        cancelTokenNew = lib.lookupFunction<Pointer<Void> Function(), Pointer<Void> Function()>('se_cancel_token_new'),
        cancelTokenCancel = lib.lookupFunction<_CloseC, _CloseD>('se_cancel_token_cancel'),
        cancelTokenFinalizer =
            NativeFinalizer(lib.lookup<NativeFunction<_CloseC>>('se_cancel_token_free').cast()),
        countNeedsText = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_search_count_needs_text'),
        countBatch = lib.lookupFunction<
            Int64 Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Size),
            int Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int)>('se_search_count_batch'),
        searchCounts = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_search_counts'),
        searchInfo = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_search_info'),
        lineStatus = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_search_line_status'),
        lineMatchBatch = lib.lookupFunction<
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Size),
            SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int)>('se_search_line_match_batch'),
        lineScanBatch = lib.lookupFunction<
            SeBuffer Function(Pointer<Void>, Pointer<Uint64>, Pointer<Pointer<Uint8>>, Pointer<Size>, Size),
            SeBuffer Function(
                Pointer<Void>, Pointer<Uint64>, Pointer<Pointer<Uint8>>, Pointer<Size>, int)>('se_search_line_scan_batch'),
        copyLineHits = lib.lookupFunction<Uint64 Function(Pointer<Void>, Uint64, Uint64, Pointer<Uint64>),
            int Function(Pointer<Void>, int, int, Pointer<Uint64>)>('se_search_copy_line_hits'),
        indexCloseFinalizer = NativeFinalizer(lib.lookup<NativeFunction<_CloseC>>('se_index_close').cast()),
        writerCloseFinalizer = NativeFinalizer(lib.lookup<NativeFunction<_CloseC>>('se_writer_close').cast()),
        searchCloseFinalizer = NativeFinalizer(lib.lookup<NativeFunction<_CloseC>>('se_search_close').cast()),
        indexClose = lib.lookupFunction<_CloseC, _CloseD>('se_index_close'),
        writerClose = lib.lookupFunction<_CloseC, _CloseD>('se_writer_close'),
        searchClose = lib.lookupFunction<_CloseC, _CloseD>('se_search_close');

  final DynamicLibrary lib;
  final Pointer<Utf8> Function() version;
  final Pointer<Utf8> Function() lastError;
  final void Function(Pointer<Void>) stringFree;
  final void Function(SeBuffer) bufferFree;
  final Pointer<Void> Function(Pointer<Uint8>, int, Pointer<Uint8>, int) indexCreate;
  final Pointer<Void> Function(Pointer<Uint8>, int) indexOpen;
  final int Function(Pointer<Uint8>, int) indexExists;
  final Pointer<Utf8> Function(Pointer<Void>) indexInfo;
  final Pointer<Uint8> Function(Pointer<Uint8>, int, Pointer<Size>) reducePairedReadings;
  final void Function(Pointer<Uint8>, int) bytesFree;
  final Pointer<Utf8> Function(Pointer<Uint8>, int) userRegexKind;
  final int Function(Pointer<Void>) indexBuildFmSidecar;
  final int Function(Pointer<Void>) indexHasFmSidecar;
  final Pointer<Void> Function(Pointer<Void>, int, int) writerOpen;
  final int Function(Pointer<Void>, int, int, int, Pointer<Uint8>, int) writerAdd;
  final int Function(Pointer<Void>, int) writerDelete;
  final int Function(Pointer<Void>, int, int, int, int, int, Pointer<Uint8>, int) writerAddLine;
  final int Function(Pointer<Void>, int, int, Pointer<Uint64>) searchCopyNextLines;
  final SeBuffer Function(
      Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) locatePairs;
  final int Function(
      Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) countBatchPairs;
  final int Function(Pointer<Void>, Pointer<Uint8>, int) writerCommit;
  final int Function(Pointer<Void>) writerRollback;
  final int Function(Pointer<Void>) writerMergeAndClose;
  final Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int) search;
  final int Function(Pointer<Void>) searchHitCount;
  final int Function(Pointer<Void>, int, int, Pointer<Uint64>) searchCopyHits;
  final SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) locateBatch;
  final SeBuffer Function(Pointer<Void>, Pointer<Int64>, Pointer<Size>, int, Pointer<Uint64>,
      Pointer<Pointer<Uint8>>, Pointer<Size>) locateLines;
  final Pointer<Utf8> Function(Pointer<Void>) takeNewTerms;
  final Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>) searchCancellable;
  final int Function() lastErrorCancelled;
  final Pointer<Void> Function() cancelTokenNew;
  final void Function(Pointer<Void>) cancelTokenCancel;
  final NativeFinalizer cancelTokenFinalizer;
  final int Function(Pointer<Void>) countNeedsText;
  final int Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) countBatch;
  final Pointer<Utf8> Function(Pointer<Void>) searchCounts;
  final Pointer<Utf8> Function(Pointer<Void>) searchInfo;
  final Pointer<Utf8> Function(Pointer<Void>) lineStatus;
  final SeBuffer Function(Pointer<Void>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) lineMatchBatch;
  final SeBuffer Function(Pointer<Void>, Pointer<Uint64>, Pointer<Pointer<Uint8>>, Pointer<Size>, int) lineScanBatch;
  final int Function(Pointer<Void>, int, int, Pointer<Uint64>) copyLineHits;
  final NativeFinalizer indexCloseFinalizer;
  final NativeFinalizer writerCloseFinalizer;
  final NativeFinalizer searchCloseFinalizer;
  final void Function(Pointer<Void>) indexClose;
  final void Function(Pointer<Void>) writerClose;
  final void Function(Pointer<Void>) searchClose;

  static SeforimNative? _instance;

  /// The platform file name of the library.
  static String get defaultFileName {
    if (Platform.isWindows) return 'seforim_search_native.dll';
    if (Platform.isMacOS || Platform.isIOS) return 'libseforim_search_native.dylib';
    return 'libseforim_search_native.so';
  }

  /// Loads the library once. Call before any other API to choose the path;
  /// otherwise the first use loads it with the default resolution.
  static SeforimNative load([String? path]) {
    final existing = _instance;
    if (existing != null) return existing;
    path ??= Platform.environment['SEFORIM_SEARCH_NATIVE'];
    final DynamicLibrary lib;
    if (Platform.isIOS && path == null) {
      lib = DynamicLibrary.process(); // statically linked into the app
    } else {
      lib = DynamicLibrary.open(path ?? defaultFileName);
    }
    return _instance = SeforimNative._(lib);
  }

  static SeforimNative get instance => _instance ?? load();

  /// The native library's version.
  String get nativeVersion => version().toDartString();

  // ── helpers ──

  SeforimEngineException error([String fallback = 'native call failed']) {
    final msg = lastError().toDartString();
    final text = msg.isEmpty ? fallback : msg;
    return lastErrorCancelled() != 0 ? SearchCancelledException(text) : SeforimEngineException(text);
  }

  Pointer<Void> checkPtr(Pointer<Void> p) {
    if (p == nullptr) throw error();
    return p;
  }

  void checkStatus(int status) {
    if (status != 0) throw error();
  }

  /// Copies and frees engine-owned UTF-8 bytes given as pointer + length
  /// (an embedded NUL is kept).
  String takeBytes(Pointer<Uint8> p, int len) {
    if (p == nullptr) throw error();
    try {
      return utf8.decode(p.asTypedList(len));
    } finally {
      bytesFree(p, len);
    }
  }

  /// Takes an engine-owned JSON string and frees it.
  String takeString(Pointer<Utf8> s) {
    if (s == nullptr) throw error();
    try {
      return s.toDartString();
    } finally {
      stringFree(s.cast());
    }
  }

  /// Copies an engine-owned u32 buffer and frees it.
  Uint32List takeBuffer(SeBuffer b) {
    try {
      if (b.data == nullptr) {
        final msg = lastError().toDartString();
        if (msg.isNotEmpty) throw error();
        return Uint32List(0);
      }
      return Uint32List.fromList(b.data.asTypedList(b.len));
    } finally {
      bufferFree(b);
    }
  }
}

/// UTF-8 bytes of [s] in native memory (at least 1 byte, so never null).
/// Returns pointer and byte length.
(Pointer<Uint8>, int) utf8Native(String s, Allocator alloc) {
  final bytes = utf8.encode(s);
  final p = alloc<Uint8>(bytes.isEmpty ? 1 : bytes.length);
  p.asTypedList(bytes.length).setAll(0, bytes);
  return (p, bytes.length);
}
