import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

import 'models.dart';
import 'native.dart';
import 'session.dart';

/// A line index on disk. Searches may run concurrently on separate isolates
/// only through separate [SeforimIndex.open] calls; one writer at a time.
///
/// Native memory is released by [close], or by a finalizer if the object is
/// garbage-collected first. Never both: [close] detaches the finalizer.
final class SeforimIndex implements Finalizable {
  SeforimIndex._(this._handle) {
    _n.indexCloseFinalizer.attach(this, _handle, detach: this);
  }

  static SeforimNative get _n => SeforimNative.instance;
  Pointer<Void> _handle;

  Pointer<Void> get handle {
    if (_handle == nullptr) throw StateError('SeforimIndex is closed');
    return _handle;
  }

  /// The native library's version.
  static String get nativeVersion => _n.nativeVersion;

  /// Otzaria's query sanitizer step: every paired reading `(X) [Y]` becomes
  /// ` Y `, as the engine's query parser does. A host that numbers the
  /// query's words itself (per-word options, per-pair spacing) runs it first.
  static String reducePairedReadings(String query) {
    return using((a) {
      final (q, ql) = utf8Native(query, a);
      return _n.takeString(_n.reducePairedReadings(q, ql));
    });
  }

  /// Creates a fresh index in [directory], WIPING it.
  static SeforimIndex create(String directory, [IndexOptions options = const IndexOptions()]) {
    return using((a) {
      final (d, dl) = utf8Native(directory, a);
      final (o, ol) = utf8Native(jsonEncode(options.toJson()), a);
      return SeforimIndex._(_n.checkPtr(_n.indexCreate(d, dl, o, ol)));
    });
  }

  /// Opens an existing index.
  static SeforimIndex open(String directory) {
    return using((a) {
      final (d, dl) = utf8Native(directory, a);
      return SeforimIndex._(_n.checkPtr(_n.indexOpen(d, dl)));
    });
  }

  /// True when [directory] holds an index.
  static bool exists(String directory) {
    return using((a) {
      final (d, dl) = utf8Native(directory, a);
      return _n.indexExists(d, dl) == 1;
    });
  }

  /// Line count, build options and the last committed payload.
  IndexInfo info() => IndexInfo.fromJson(jsonDecode(_n.takeString(_n.indexInfo(handle))) as Map<String, dynamic>);

  /// Builds the FM-index sidecar over the term dictionary (FtsLib's FM sidecar):
  /// leading-`*` wildcards and fuzzy words then run through it instead of
  /// walking the whole dictionary, with identical results. Needs a single
  /// segment: call after [IndexWriter.mergeAndClose]. Idempotent. Returns
  /// false (nothing built; searches use the walk) when the index has several
  /// segments, no terms, or a dictionary with more than 64 distinct
  /// characters (a marked index with nikud and te'amim).
  bool buildFmSidecar() {
    final r = _n.indexBuildFmSidecar(handle);
    if (r < 0) throw _n.error();
    return r == 1;
  }

  /// True when a sidecar matching the current single segment exists.
  bool get hasFmSidecar => _n.indexHasFmSidecar(handle) == 1;

  /// Starts a writer. [heapBytes] is the total indexing memory (≥15 MB per thread).
  IndexWriter openWriter({int heapBytes = 50000000, int threads = 0}) {
    if (threads <= 0) threads = Platform.numberOfProcessors.clamp(1, 4);
    return IndexWriter._(this, _n.checkPtr(_n.writerOpen(handle, heapBytes, threads)));
  }

  /// Runs a search (the public API): the query string is parsed by FtsLib's
  /// rules and each word becomes one walk with its options. Every matching
  /// line in catalog order, plus the snippet session.
  SearchSession search(SearchOptions options) => _search(options.toJson(), options.contextWords);

  /// The low-level form: slots built by the caller ([SearchRequest]).
  SearchSession searchRequest(SearchRequest request) => _search(request.toJson(), request.contextWords);

  SearchSession _search(Map<String, Object?> json, int contextWords) {
    return using((a) {
      final (j, jl) = utf8Native(jsonEncode(json), a);
      final h = _n.checkPtr(_n.search(handle, j, jl));
      return SearchSession.internal(this, h, contextWords);
    });
  }

  /// Frees the index. Close writers and sessions first.
  void close() {
    if (_handle == nullptr) return;
    _n.indexCloseFinalizer.detach(this);
    _n.indexClose(_handle);
    _handle = nullptr;
  }
}

/// Adds, deletes and commits lines. Not thread-safe.
final class IndexWriter implements Finalizable {
  IndexWriter._(this._index, this._handle) {
    _n.writerCloseFinalizer.attach(this, _handle, detach: this);
  }

  static SeforimNative get _n => SeforimNative.instance;

  // Held so the index outlives the writer (the native writer borrows it).
  // ignore: unused_field
  final SeforimIndex _index;
  Pointer<Void> _handle;

  Pointer<Void> get _h {
    if (_handle == nullptr) throw StateError('IndexWriter is closed');
    return _handle;
  }

  /// Queues a line. Not visible to searches until [commit].
  void add(LineDocument line) {
    using((a) {
      final (t, tl) = utf8Native(line.text, a);
      _n.checkStatus(_n.writerAdd(_h, line.lineId, line.position, line.bookId, t, tl));
    });
  }

  /// Queues deletion of a line. An update is delete + add.
  void delete(int lineId) => _n.checkStatus(_n.writerDelete(_h, lineId));

  /// Commits; [payload] is stored atomically with the documents.
  void commit([String? payload]) {
    if (payload == null) {
      _n.checkStatus(_n.writerCommit(_h, nullptr, 0));
      return;
    }
    using((a) {
      final (p, pl) = utf8Native(payload, a);
      _n.checkStatus(_n.writerCommit(_h, p, pl));
    });
  }

  /// Drops everything queued since the last commit.
  void rollback() => _n.checkStatus(_n.writerRollback(_h));

  /// Merges all segments into one and closes the writer.
  void mergeAndClose() {
    final h = _h;
    _n.writerCloseFinalizer.detach(this);
    _handle = nullptr; // native side frees it whatever the outcome
    _n.checkStatus(_n.writerMergeAndClose(h));
  }

  /// Closes the writer, discarding anything uncommitted.
  void close() {
    if (_handle == nullptr) return;
    _n.writerCloseFinalizer.detach(this);
    _n.writerClose(_handle);
    _handle = nullptr;
  }
}
