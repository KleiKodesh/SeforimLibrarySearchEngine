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

/// Thrown when a resume ([SearchOptions.afterPosition] with
/// [SearchOptions.resumeStamp]) found the index changed since the first
/// page: restart the search from the start (issue #12).
class StaleResumeException extends SeforimEngineException {
  StaleResumeException(super.message);
  @override
  String toString() => 'StaleResumeException: $message';
}

/// The C ABI version this package was built for: the native library's
/// `se_abi_version()` must return exactly this (engine-ffi `SE_ABI_VERSION`;
/// `tools/release/check_sync.py` keeps the two in sync).
const int seforimAbiVersion = 7;

/// Thrown at load when the native library's C ABI version is not the one
/// this package was built for.
class NativeAbiMismatchException extends SeforimEngineException {
  NativeAbiMismatchException(this.expected, this.loaded, this.path)
      : super('seforim_search_native ABI mismatch: the Dart package needs ABI $expected, '
            'the library at "$path" has ABI ${loaded == 0 ? '0 (older than ABI versioning)' : loaded}. '
            'Use the native library built for this package version.');
  final int expected;
  final int loaded;
  final String path;
  @override
  String toString() => 'NativeAbiMismatchException: $message';
}

/// The library's own path; resolved through the Dart native asset the
/// package's build hook bundles (`hook/build.dart`). Throws when no asset is
/// bundled (e.g. a plain `dart compile` without hooks).
@Native<Pointer<Utf8> Function()>(
    symbol: 'se_native_library_path', assetId: 'package:seforim_search/seforim_search_native')
external Pointer<Utf8> _assetLibraryPath();

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
/// `SEFORIM_SEARCH_NATIVE` environment variable (dev override), else the
/// native asset bundled by the package's build hook (prebuilt download,
/// cargo build or local file: see README), else the platform file name
/// (`seforim_search_native.dll` / `libseforim_search_native.so` /
/// `libseforim_search_native.dylib`) through the OS loader search path.
/// Every load checks the library's C ABI version ([seforimAbiVersion]).
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
        indexSetFmAutoRepair = lib.lookupFunction<Int32 Function(Pointer<Void>, Int32), int Function(Pointer<Void>, int)>(
            'se_index_set_fm_auto_repair'),
        indexFmSidecarRebuilding = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_index_fm_sidecar_rebuilding'),
        indexFmSidecarProblem = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_index_fm_sidecar_problem'),
        indexNumLines = lib.lookupFunction<Int64 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_index_num_lines'),
        indexReload = lib.lookupFunction<Int32 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_index_reload'),
        writerOpen = lib.lookupFunction<Pointer<Void> Function(Pointer<Void>, Size, Size),
            Pointer<Void> Function(Pointer<Void>, int, int)>('se_writer_open'),
        writerOpenEx = lib.lookupFunction<
            Pointer<Void> Function(Pointer<Void>, Size, Size, Uint32),
            Pointer<Void> Function(Pointer<Void>, int, int, int)>('se_writer_open_ex'),
        writerMemoryBudget = lib.lookupFunction<Uint64 Function(Pointer<Void>), int Function(Pointer<Void>)>(
            'se_writer_memory_budget'),
        threadBackgroundBegin =
            lib.lookupFunction<Void Function(), void Function()>('se_thread_background_begin'),
        threadBackgroundEnd = lib.lookupFunction<Void Function(), void Function()>('se_thread_background_end'),
        writerAdd = lib.lookupFunction<
            Int32 Function(Pointer<Void>, Uint64, Uint64, Uint64, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, int, int, int, Pointer<Uint8>, int)>('se_writer_add'),
        writerAddLine = lib.lookupFunction<
            Int32 Function(Pointer<Void>, Uint64, Uint64, Uint64, Uint64, Uint32, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, int, int, int, int, int, Pointer<Uint8>, int)>('se_writer_add_line'),
        writerAddLines = lib.lookupFunction<
            Int64 Function(Pointer<Void>, Pointer<Uint64>, Size, Pointer<Uint8>, Size),
            int Function(Pointer<Void>, Pointer<Uint64>, int, Pointer<Uint8>, int)>('se_writer_add_lines'),
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
        searchSetMatchMode = lib.lookupFunction<Int32 Function(Pointer<Void>, Uint32),
            int Function(Pointer<Void>, int)>('se_search_set_match_mode'),
        findAllMatches = lib.lookupFunction<
            SeBuffer Function(Pointer<Uint8>, Size, Pointer<Pointer<Uint8>>, Pointer<Size>, Size, Uint32),
            SeBuffer Function(Pointer<Uint8>, int, Pointer<Pointer<Uint8>>, Pointer<Size>, int, int)>(
            'se_find_all_matches'),
        takeNewTerms = lib.lookupFunction<Pointer<Utf8> Function(Pointer<Void>),
            Pointer<Utf8> Function(Pointer<Void>)>('se_search_take_new_terms'),
        searchCancellable = lib.lookupFunction<
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Void>),
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>)>('se_search_cancellable'),
        searchStream = lib.lookupFunction<
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, Size, Pointer<Void>),
            Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>)>('se_search_stream'),
        searchStreamPull = lib.lookupFunction<Int32 Function(Pointer<Void>, Uint64, Int64),
            int Function(Pointer<Void>, int, int)>('se_search_stream_pull'),
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
  final int Function(Pointer<Void>, int) indexSetFmAutoRepair;
  final int Function(Pointer<Void>) indexFmSidecarRebuilding;
  final Pointer<Utf8> Function(Pointer<Void>) indexFmSidecarProblem;
  final int Function(Pointer<Void>) indexNumLines;
  final int Function(Pointer<Void>) indexReload;
  final Pointer<Void> Function(Pointer<Void>, int, int) writerOpen;
  final Pointer<Void> Function(Pointer<Void>, int, int, int) writerOpenEx;
  final int Function(Pointer<Void>) writerMemoryBudget;
  final void Function() threadBackgroundBegin;
  final void Function() threadBackgroundEnd;
  final int Function(Pointer<Void>, int, int, int, Pointer<Uint8>, int) writerAdd;
  final int Function(Pointer<Void>, int) writerDelete;
  final int Function(Pointer<Void>, int, int, int, int, int, Pointer<Uint8>, int) writerAddLine;

  /// `se_writer_add_lines`: records of 6 u64 words (line, position, book,
  /// section, text length, flags | reserved << 32), texts concatenated.
  final int Function(Pointer<Void>, Pointer<Uint64>, int, Pointer<Uint8>, int) writerAddLines;
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
  final int Function(Pointer<Void>, int) searchSetMatchMode;
  final SeBuffer Function(Pointer<Uint8>, int, Pointer<Pointer<Uint8>>, Pointer<Size>, int, int) findAllMatches;
  final SeBuffer Function(Pointer<Void>, Pointer<Int64>, Pointer<Size>, int, Pointer<Uint64>,
      Pointer<Pointer<Uint8>>, Pointer<Size>) locateLines;
  final Pointer<Utf8> Function(Pointer<Void>) takeNewTerms;
  final Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>) searchCancellable;
  final Pointer<Void> Function(Pointer<Void>, Pointer<Uint8>, int, Pointer<Void>) searchStream;
  final int Function(Pointer<Void>, int, int) searchStreamPull;
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
    path ??= _bundledAssetPath();
    final DynamicLibrary lib;
    if (Platform.isIOS && path == null) {
      lib = DynamicLibrary.process(); // statically linked into the app
    } else {
      lib = DynamicLibrary.open(path ?? defaultFileName);
    }
    checkAbi(lib, path ?? defaultFileName);
    return _instance = SeforimNative._(lib);
  }

  static String? _bundledAssetPath() {
    try {
      final p = _assetLibraryPath().toDartString();
      return p.isEmpty ? null : p;
    } on Object {
      return null; // no native asset bundled
    }
  }

  /// The C ABI version of [lib] (0 for a library older than ABI versioning).
  static int abiVersionOf(DynamicLibrary lib) {
    if (!lib.providesSymbol('se_abi_version')) return 0;
    return lib.lookupFunction<Uint32 Function(), int Function()>('se_abi_version')();
  }

  /// Throws [NativeAbiMismatchException] unless [lib]'s ABI version is
  /// [expected]; [requested] names the library when the OS cannot tell its path.
  static void checkAbi(DynamicLibrary lib, String requested, {int expected = seforimAbiVersion}) {
    final loaded = abiVersionOf(lib);
    if (loaded == expected) return;
    var where = requested;
    if (lib.providesSymbol('se_native_library_path')) {
      final p = lib.lookupFunction<Pointer<Utf8> Function(), Pointer<Utf8> Function()>('se_native_library_path')()
          .toDartString();
      if (p.isNotEmpty) where = p;
    }
    throw NativeAbiMismatchException(expected, loaded, where);
  }

  static SeforimNative get instance => _instance ?? load();

  /// The native library's version.
  String get nativeVersion => version().toDartString();

  // ── helpers ──

  SeforimEngineException error([String fallback = 'native call failed']) {
    final msg = lastError().toDartString();
    final text = msg.isEmpty ? fallback : msg;
    if (lastErrorCancelled() != 0) return SearchCancelledException(text);
    return text.startsWith('stale resume:') ? StaleResumeException(text) : SeforimEngineException(text);
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
