import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

import 'models.dart';
import 'native.dart';
import 'session.dart';

/// Cancels a search (Otzaria's cancellation token). One token per search;
/// it cannot be reset. [cancel] returns at once; the search notices at its
/// next check. From another isolate, pass [address] and call
/// [SearchCancelToken.cancelAt].
final class SearchCancelToken implements Finalizable {
  SearchCancelToken() : handle = SeforimNative.instance.cancelTokenNew() {
    SeforimNative.instance.cancelTokenFinalizer.attach(this, handle);
  }

  /// The native token (kept alive by this object and by the sessions using it).
  final Pointer<Void> handle;

  /// The native address, for [cancelAt] on another isolate while this
  /// object is alive.
  int get address => handle.address;

  /// Asks the search holding this token to stop.
  void cancel() => SeforimNative.instance.cancelTokenCancel(handle);

  /// Cancels the token at [address] (from [SearchCancelToken.address]).
  static void cancelAt(int address) => SeforimNative.instance.cancelTokenCancel(Pointer.fromAddress(address));
}

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

  /// Otzaria's user-regex kind of [query]: `none` (not `/.../`), `invalid`
  /// (searched as plain text), `word`, `sequence`, `line` (matched on each
  /// line's text: [SearchSession.lineRegex]). Set [SearchOptions.userRegex] to search `/.../` as a regex.
  static String userRegexKind(String query) {
    return using((a) {
      final (q, ql) = utf8Native(query, a);
      return jsonDecode(_n.takeString(_n.userRegexKind(q, ql))) as String;
    });
  }

  /// Otzaria's query sanitizer step: every paired reading `(X) [Y]` becomes
  /// ` Y `, as the engine's query parser does. A host that numbers the
  /// query's words itself (per-word options, per-pair spacing) runs it first.
  static String reducePairedReadings(String query) {
    return using((a) {
      final (q, ql) = utf8Native(query, a);
      final n = a<Size>();
      final r = _n.reducePairedReadings(q, ql, n);
      return _n.takeBytes(r, n.value);
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

  /// Starts a writer. [heapBytes] is the total indexing memory (≥15 MB per
  /// thread); 0 (the default) gives 50 MB per thread, so [threads] (0 = cores,
  /// at most 4) are always granted.
  /// With [lowPriority], tantivy's indexing and merge threads run below
  /// normal OS priority (a background build that keeps the machine usable).
  IndexWriter openWriter({int heapBytes = 0, int threads = 0, bool lowPriority = false}) {
    if (threads <= 0) threads = Platform.numberOfProcessors.clamp(1, 4);
    if (heapBytes <= 0) heapBytes = threads * 50000000;
    return IndexWriter._(
        this, _n.checkPtr(_n.writerOpenEx(handle, heapBytes, threads, lowPriority ? 1 : 0)));
  }

  /// Runs a search (the public API): the query string is parsed by FtsLib's
  /// rules and each word becomes one walk with its options. Every matching
  /// line in the search's order (catalog by default), plus the snippet session.
  ///
  /// With [cancel], [SearchCancelToken.cancel] stops the search (checked
  /// before the walks, per segment and every 4096 collected docs) and every
  /// later `locateBatch` / `countBatch` of the session (per chunk of work):
  /// they throw [SearchCancelledException].
  SearchSession search(SearchOptions options, {SearchCancelToken? cancel}) =>
      _search(options.toJson(), options.contextWords, cancel);

  /// The low-level form: slots built by the caller ([SearchRequest]).
  SearchSession searchRequest(SearchRequest request) => _search(request.toJson(), request.contextWords);

  SearchSession _search(Map<String, Object?> json, int contextWords, [SearchCancelToken? cancel]) {
    return using((a) {
      final (j, jl) = utf8Native(jsonEncode(json), a);
      final h = _n.checkPtr(cancel == null
          ? _n.search(handle, j, jl)
          : _n.searchCancellable(handle, j, jl, cancel.handle));
      return SearchSession.internal(this, h, contextWords, cancel);
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
      final flags = (line.noCross ? 1 : 0) | (line.sectionId != null ? 2 : 0);
      _n.checkStatus(
          _n.writerAddLine(_h, line.lineId, line.position, line.bookId, line.sectionId ?? 0, flags, t, tl));
    });
  }

  /// Queues many lines in one native call (`se_writer_add_lines`): far
  /// fewer FFI crossings than [add] per line when indexing a whole library.
  /// On an error the lines before the failing one stay queued.
  void addAll(List<LineDocument> lines) {
    if (lines.isEmpty) return;
    final texts = [for (final l in lines) utf8.encode(l.text)];
    var total = 0;
    for (final t in texts) {
      total += t.length;
    }
    using((a) {
      final recs = a<Uint64>(lines.length * 6);
      final words = recs.asTypedList(lines.length * 6);
      final p = a<Uint8>(total == 0 ? 1 : total);
      final bytes = p.asTypedList(total);
      var at = 0;
      for (var i = 0; i < lines.length; i++) {
        final l = lines[i];
        final t = texts[i];
        bytes.setRange(at, at + t.length, t);
        at += t.length;
        final w = i * 6;
        words[w] = l.lineId;
        words[w + 1] = l.position;
        words[w + 2] = l.bookId;
        words[w + 3] = l.sectionId ?? 0;
        words[w + 4] = t.length;
        // flags in the low half (little-endian), reserved = 0
        words[w + 5] = (l.noCross ? 1 : 0) | (l.sectionId != null ? 2 : 0);
      }
      if (_n.writerAddLines(_h, recs, lines.length, p, total) < 0) _n.checkStatus(-1);
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
