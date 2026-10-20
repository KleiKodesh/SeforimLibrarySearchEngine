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

  /// Starts a writer. [heapBytes] is the total indexing memory (≥15 MB per thread).
  IndexWriter openWriter({int heapBytes = 50000000, int threads = 0}) {
    if (threads <= 0) threads = Platform.numberOfProcessors.clamp(1, 4);
    return IndexWriter._(this, _n.checkPtr(_n.writerOpen(handle, heapBytes, threads)));
  }

  /// Runs a search: every matching line in catalog order, plus the snippet session.
  SearchSession search(SearchRequest request) {
    return using((a) {
      final (j, jl) = utf8Native(jsonEncode(request.toJson()), a);
      final h = _n.checkPtr(_n.search(handle, j, jl));
      return SearchSession.internal(this, h, request.contextWords);
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
      _n.checkStatus(_n.writerAdd(_h, line.lineId, line.position, line.bookId, line.lineIndex, line.tocEntryId, t, tl));
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
