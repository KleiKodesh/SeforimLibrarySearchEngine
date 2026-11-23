import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'meivin_embedder.dart';
import 'models.dart';
import 'vector_set.dart'
    show compactVectorSet, installVectorSegment, verifyOxv, VectorCompactionPolicy, VectorCompactionReport, VectorInstallReport;
import 'native.dart';
import 'semantic.dart';
import 'semantic_passages.dart';
import 'session.dart';

/// One page of hits streamed by [SearchWorker.searchStream] (issue #16).
final class HitBatch {
  const HitBatch(this.requestId, this.offset, this.hitCount, this.hits, [this.complete = true]);

  /// The request this batch belongs to (one id per [SearchWorker.searchStream]).
  final int requestId;

  /// Index of [hits]' first entry in the search's hits.
  final int offset;

  /// Matching lines of the search taken in so far (the total once
  /// [complete]; the search streams on an index with positions, issue #21).
  final int hitCount;

  /// True when every hit of the search is in ([hitCount] is the total).
  final bool complete;

  /// Hits `[offset, offset + hits.length)`, in the search's order.
  final List<PlacedHit> hits;

  /// True for the last batch.
  bool get isLast => complete && offset + hits.length >= hitCount;
}

/// A background isolate that owns its own [SeforimIndex] and runs searches
/// there, so the calling isolate (a UI) never blocks on the engine
/// (issue #16). Requests are served one at a time, in order; one worker
/// serves any number of searches (pool several workers for parallel ones).
/// Line ids only: text fetching stays in the host.
///
/// Cancellation goes through [SearchCancelToken]: the token's native flag is
/// set from the calling isolate, so a running search stops at its next
/// check even while the worker is busy.
final class SearchWorker {
  SearchWorker._(this._isolate, this._send, this._receive);

  final Isolate _isolate;
  final SendPort _send;
  final ReceivePort _receive;
  final _pending = <int, Completer<Object?>>{};
  var _nextId = 1;
  var _closed = false;

  /// Starts a worker isolate that opens the index at [directory].
  /// [nativePath] is the native library to load there (the
  /// `SEFORIM_SEARCH_NATIVE` / default resolution when null).
  static Future<SearchWorker> spawn(String directory, {String? nativePath}) async {
    final receive = ReceivePort();
    final ready = Completer<SendPort>();
    SearchWorker? worker;
    receive.listen((m) {
      final w = worker;
      if (m == null) {
        // the worker isolate exited (onExit): fail whatever still waits
        if (!ready.isCompleted) ready.completeError(StateError('SearchWorker isolate exited'));
        w?._failAll(StateError('SearchWorker isolate exited'));
      } else if (w != null) {
        w._onReply(m);
      } else if (m is SendPort) {
        ready.complete(m);
      } else {
        final (e, st) = m as (Object, String);
        ready.completeError(e, StackTrace.fromString(st));
      }
    });
    try {
      final isolate = await Isolate.spawn(_workerMain, (receive.sendPort, directory, nativePath),
          debugName: 'seforim_search worker', onExit: receive.sendPort);
      final send = await ready.future;
      return worker = SearchWorker._(isolate, send, receive);
    } catch (_) {
      receive.close();
      rethrow;
    }
  }

  void _failAll(Object error) {
    _closed = true;
    _receive.close();
    final waiting = _pending.values.toList();
    _pending.clear();
    for (final c in waiting) {
      c.completeError(error);
    }
  }

  void _onReply(Object? m) {
    final (id, ok, value, st) = m as (int, bool, Object?, String?);
    final c = _pending.remove(id);
    if (c == null) return;
    if (ok) {
      c.complete(value);
    } else {
      c.completeError(value!, st == null ? null : StackTrace.fromString(st));
    }
  }

  Future<T> _call<T>(String op, [Object? a, Object? b, Object? c]) {
    if (_closed) return Future.error(StateError('SearchWorker is closed'));
    final id = _nextId++;
    final done = Completer<Object?>();
    _pending[id] = done;
    _send.send((id, op, a, b, c));
    return done.future.then((v) => v as T);
  }

  /// Runs a search on the worker; the session stays there until
  /// [AsyncSearchSession.close]. [cancel] stops it and every later call of
  /// the session ([SearchCancelledException]).
  Future<AsyncSearchSession> search(SearchOptions options, {SearchCancelToken? cancel}) =>
      _open(options.toJson(), options.contextWords, cancel);

  /// [SemanticIndex.semanticSearch] on the worker (issue #18). [rows] reads
  /// the host rows a window check needs, on this isolate (the host's own
  /// database); they are sent to the worker and the search asked again.
  Future<SemanticResult> semanticSearch(Float32List query,
      [SemanticOptions options = const SemanticOptions(), SearchCancelToken? cancel, SemanticRows? rows]) async {
    // a Finalizable local: the token outlives every step of this call
    final SearchCancelToken? c = cancel;
    while (true) {
      final r = await _call<SemanticResult>('semanticSearch', null, (query, options), c?.address);
      if (r.needRows.isEmpty) return r;
      if (rows == null) throw StateError('the semantic search needs host rows: pass rows');
      await addSemanticRows(await rows(r.needRows));
    }
  }

  /// [SemanticIndex.addSemanticRows] on the worker.
  Future<void> addSemanticRows(List<SemanticRow> rows) => _call('addSemanticRows', rows);

  /// [SemanticIndex.openVectors] on the worker.
  Future<VectorSetInfo> openVectors(String path) => _call('openVectors', path);

  /// [SemanticIndex.closeVectors] on the worker.
  Future<void> closeVectors() => _call('closeVectors');

  /// [installVectorSegment] on the worker (hashes and verifies the whole segment
  /// off the calling isolate).
  Future<VectorInstallReport> installVectorSegment(String setDir, String segmentPath, String manifestJson,
          {String? publishedManifestSha256, String? expectedModelJson, SearchCancelToken? cancel}) =>
      _call('installVectorSegment', setDir, (segmentPath, manifestJson, publishedManifestSha256, expectedModelJson),
          cancel?.address);

  /// [compactVectorSet] on the worker (reads and copies the whole set off the
  /// calling isolate). [live] stays the caller's: keep it until the future completes.
  Future<VectorCompactionReport> compactVectorSet(String setDir,
          {VectorCompactionPolicy? policy, SearchCancelToken? cancel, VectorLiveKeys? live}) =>
      _call('compactVectorSet', setDir, (policy, live?.handle.address), cancel?.address);

  /// [verifyOxv] on the worker.
  Future<Map<String, dynamic>> verifyOxv(String path, {String? manifestJson, SearchCancelToken? cancel}) =>
      _call('verifyOxv', path, manifestJson, cancel?.address);

  /// [SemanticIndex.vectorInfo] on the worker.
  Future<VectorSetInfo?> vectorInfo() => _call('vectorInfo');

  // -- hybrid search and the query model on the worker (issue #18) --

  /// [SemanticIndex.hybridSession] on the worker: the session lives there
  /// until [AsyncHybridSession.close]. [vector]: the query's embedding; when
  /// null, the worker embeds [query] with its loaded embedder ([loadEmbedder];
  /// none loaded, a query with no text to embed, or
  /// [HybridRetrieval.lexicalOnly] = an empty vector: the lexical side alone).
  /// [texts]: the default line-text provider of the session's calls (an index
  /// without positions scores a query of several words from the line texts).
  /// [rows]: the default host-row provider (the semantic resolver's window
  /// checks, `need_rows`).
  Future<AsyncHybridSession> hybridSession(String query,
      {Float32List? vector, HybridOptions options = const HybridOptions(), HybridTexts? texts, SemanticRows? rows}) async {
    final id = await _call<int>('hybridOpen', query, vector, options);
    return AsyncHybridSession._(this, id, texts, rows);
  }

  /// Loads Otzaria's query model on the worker ([MeivinEmbedder.open] on
  /// [modelDir] with [graphFile]); with [passageGraphFile] (in
  /// [passageModelDir], default [modelDir]) also the passage embedder
  /// (Otzaria's FP32 passage model for `embed_passages`). Replaces the
  /// embedders loaded before. Returns the query model's output dimensions.
  Future<int> loadEmbedder(String modelDir, String ortLibrary,
          {int threads = 1, String graphFile = 'query.int8.onnx', String? passageModelDir, String? passageGraphFile}) =>
      _call('embedder', 'load', (modelDir, ortLibrary, threads, graphFile), (passageModelDir, passageGraphFile));

  /// Frees the worker's embedders (no-op without one).
  Future<void> unloadEmbedder() => _call('embedder', 'unload');

  /// True while an embedder is loaded on the worker.
  Future<bool> hasEmbedder() => _call('embedder', 'has');

  /// [MeivinEmbedder.embedQuery] on the worker.
  Future<Float32List> embedQuery(String query) => _call('embedder', 'query', query);

  /// [MeivinEmbedder.embedQueries] on the worker.
  Future<List<Float32List>> embedQueries(List<String> queries) => _call('embedder', 'queries', queries);

  /// [MeivinEmbedder.embedPassages] on the worker, by the passage embedder
  /// when one is loaded, else the query model (Otzaria's `embed_passages`).
  Future<List<Float32List>> embedPassages(List<String> texts) => _call('embedder', 'passages', texts);

  /// [semanticPassageHighlights] on the worker: plan, embed (the worker's
  /// embedders) and mark there.
  Future<List<PassageHighlight>> semanticPassageHighlights(String query, List<PassageTarget> targets,
          {List<String> forms = const [], int? chunkingIdentity, int maxChars = 800}) =>
      _call('embedder', 'highlights', (query, targets), (forms, chunkingIdentity, maxChars));

  /// [SeforimIndex.openWriter] on the worker (issue #38): the writer lives
  /// on the worker's isolate and every call is a Future, so a commit, merge
  /// or re-sort never blocks the caller. The worker serves one request at a
  /// time: a search sent while a merge runs waits for it (use a second
  /// worker for searches during long writes). One writer at a time.
  Future<AsyncIndexWriter> openWriter({int heapBytes = 0, int threads = 0, bool lowPriority = false}) async {
    final id = await _call<int>('openWriter', heapBytes, threads, lowPriority);
    return AsyncIndexWriter._(this, id);
  }

  /// [SeforimIndex.buildFmSidecar] on the worker.
  Future<bool> buildFmSidecar() => _call('index', 'buildFmSidecar');

  // -- the user index (issue #44) --

  /// Opens the user index at [directory] on the worker (with [create], a
  /// fresh one: [SeforimIndex.createUser], WIPING the directory; else
  /// [SeforimIndex.openUser]) and attaches it to the worker's library
  /// index: searches then read both ([SearchOptions.sources]). A user index
  /// already open is detached and closed first. [options] must equal the
  /// library's.
  Future<void> openUserIndex(String directory, {bool create = false, IndexOptions options = const IndexOptions()}) =>
      _call('user', 'open', (directory, create, options));

  /// Detaches and closes the user index (no-op without one).
  Future<void> closeUserIndex() => _call('user', 'close');

  /// True while a user index is attached.
  Future<bool> hasUserIndex() => _call('user', 'has');

  /// [SeforimIndex.documents] (the attached user index's documents).
  Future<List<UserDocumentInfo>> documents() => _call('index', 'documents');

  /// [SeforimIndex.lineSources]: key, locator and line number of user lines
  /// (null for a library line).
  Future<List<UserLineSource?>> lineSources(List<int> lineIds) => _call('index', 'lineSources', lineIds);

  /// [SeforimIndex.lineTexts]: the stored texts of user lines (null for a
  /// library line: those come from the host's database).
  Future<List<String?>> lineTexts(List<int> lineIds) => _call('index', 'lineTexts', lineIds);

  /// [openWriter] on the attached user index: [AsyncIndexWriter.putDocument]
  /// / [AsyncIndexWriter.deleteDocuments], commit, then
  /// [AsyncIndexWriter.mergeAndClose] and [buildUserFmSidecar] after a large
  /// change. A commit is seen by the next search.
  Future<AsyncIndexWriter> openUserWriter({int heapBytes = 0, int threads = 0, bool lowPriority = false}) async {
    final id = await _call<int>('openUserWriter', heapBytes, threads, lowPriority);
    return AsyncIndexWriter._(this, id);
  }

  /// [SeforimIndex.buildFmSidecar] on the user index (after its merge).
  Future<bool> buildUserFmSidecar() => _call('user', 'buildFmSidecar');

  /// [SeforimIndex.reload] on the user index.
  Future<void> reloadUser() => _call('user', 'reload');

  /// [SeforimIndex.reload] on the worker.
  Future<void> reload() => _call('index', 'reload');

  /// [SeforimIndex.releaseMemory] on the worker (issue #40).
  Future<void> releaseMemory() => _call('index', 'releaseMemory');

  /// [SeforimIndex.numLines] on the worker.
  Future<int> numLines() => _call('index', 'numLines');

  /// [SeforimIndex.info] on the worker (positions, keep marks, payload).
  Future<IndexInfo> info() => _call('index', 'info');

  /// [SeforimIndex.startBackgroundMerge] on the worker.
  Future<bool> startBackgroundMerge([List<int>? bookOrder]) => _call('index', 'startBackgroundMerge', bookOrder);

  /// [SeforimIndex.recordBackgroundMerge] on the worker.
  Future<void> recordBackgroundMerge(List<int>? bookOrder) => _call('index', 'recordBackgroundMerge', bookOrder);

  /// [SeforimIndex.waitBackgroundMerge] on the worker (the worker is busy
  /// until the merge ends).
  Future<void> waitBackgroundMerge() => _call('index', 'waitBackgroundMerge');

  /// [SeforimIndex.stopBackgroundMerge] on the worker.
  Future<void> stopBackgroundMerge() => _call('index', 'stopBackgroundMerge');

  /// True while the background merge runs ([SeforimIndex.backgroundMergeRunning]).
  Future<bool> backgroundMergeRunning() => _call('index', 'backgroundMergeRunning');

  /// [search] with slots built by the caller ([SearchRequest]).
  Future<AsyncSearchSession> searchRequest(SearchRequest request, {SearchCancelToken? cancel}) =>
      _open(request.toJson(), request.contextWords, cancel);

  Future<AsyncSearchSession> _open(Map<String, Object?> json, int contextWords, SearchCancelToken? cancel) async {
    final (id, hitCount) = await _call<(int, int)>('search', jsonEncode(json), contextWords, cancel?.address);
    return AsyncSearchSession._(this, id, hitCount, contextWords, cancel);
  }

  /// [SeforimIndex.searchStreaming] on the worker: completes as soon as the
  /// first hits are decided (issue #21); take the rest in with
  /// [AsyncSearchSession.pull].
  Future<AsyncSearchSession> searchStreaming(SearchOptions options, {SearchCancelToken? cancel}) async {
    final (id, hitCount, complete) = await _call<(int, int, bool)>(
        'searchStreaming', jsonEncode(options.toJson()), options.contextWords, cancel?.address);
    return AsyncSearchSession._(this, id, hitCount, options.contextWords, cancel).._complete = complete;
  }

  /// Runs a search and streams every hit in pages of [pageSize], pulled one
  /// page at a time (a paused listener pauses the worker's paging). The
  /// session is freed on the worker when the stream ends: after the last
  /// page, on an error, or when the listener cancels (which also cancels
  /// the search's token when [cancel] is null). Errors arrive as the
  /// engine's typed exceptions ([SearchCancelledException],
  /// [StaleResumeException], [SeforimEngineException]).
  Stream<HitBatch> searchStream(SearchOptions options, {SearchCancelToken? cancel, int pageSize = 4096}) {
    if (pageSize <= 0) throw RangeError.range(pageSize, 1, null, 'pageSize');
    final requestId = _nextId++;
    final token = cancel ?? SearchCancelToken();
    AsyncSearchSession? session;
    var stopped = false;
    Completer<void>? resumed;
    void wake() {
      final r = resumed;
      resumed = null;
      if (r != null && !r.isCompleted) r.complete();
    }

    late final StreamController<HitBatch> out;

    Future<void> finish() async {
      final s = session;
      session = null;
      if (s != null) await s.close();
    }

    Future<void> run() async {
      try {
        // on an index with positions the search streams (issue #21): the
        // first page comes as soon as it is decided
        final s = session = await searchStreaming(options, cancel: token);
        var offset = 0;
        var sentLast = false;
        while (true) {
          if (stopped) break;
          if (out.isPaused) {
            resumed = Completer<void>();
            await resumed!.future;
          }
          if (stopped) break;
          if (offset + pageSize > s.hitCount && !s.isComplete) await s.pull(offset + pageSize);
          if (stopped) break;
          if (offset >= s.hitCount) {
            // complete with every page sent: the last batch says so (empty
            // when the last page was sent before the stream completed)
            if (!sentLast) out.add(HitBatch(requestId, offset, s.hitCount, const []));
            break;
          }
          final page = await s.hits(offset, pageSize);
          if (stopped) break;
          offset += page.length;
          final batch = HitBatch(requestId, offset - page.length, s.hitCount, page, s.isComplete);
          sentLast = batch.isLast;
          out.add(batch);
          if (page.isEmpty) break;
        }
      } catch (e, st) {
        if (!stopped) out.addError(e, st);
      } finally {
        await finish();
        if (!stopped) await out.close();
      }
    }

    out = StreamController<HitBatch>(
      onListen: run,
      onResume: wake,
      onCancel: () async {
        stopped = true;
        wake();
        if (cancel == null) token.cancel();
        await finish();
      },
    );
    return out.stream;
  }

  Future<void>? _closing;

  /// Stops the worker: frees its sessions and its index, then the isolate.
  /// Idempotent; calls still in flight fail with [StateError].
  Future<void> close() => _closing ??= _close();

  Future<void> _close() async {
    if (_closed) return;
    try {
      await _call<void>('dispose');
    } finally {
      _isolate.kill(priority: Isolate.beforeNextEvent);
      _failAll(StateError('SearchWorker is closed'));
    }
  }
}

/// A [SearchSession] living on a [SearchWorker]: the same calls, each a
/// Future served by the worker. [close] frees the native session there.
final class AsyncSearchSession {
  AsyncSearchSession._(this._worker, this._id, this.hitCount, this.contextWords, this._cancel);

  final SearchWorker _worker;
  final int _id;
  // Keeps the cancel token alive as long as the session.
  // ignore: unused_field
  final SearchCancelToken? _cancel;
  var _closed = false;

  /// [SearchSession.hitCount] (taken in so far: see [pull]).
  int hitCount;

  var _complete = true;

  /// [SearchSession.isComplete].
  bool get isComplete => _complete;

  /// Words of context per side this search locates with.
  final int contextWords;

  /// [SearchSession.pull] on the worker; updates [hitCount] and [isComplete].
  Future<bool> pull(int minHits, {int timeoutMs = -1}) async {
    if (_complete) return true;
    final (complete, count) = await _call<(bool, int)>('pull', minHits, timeoutMs);
    hitCount = count;
    _complete = complete;
    return complete;
  }

  Future<T> _call<T>(String op, [Object? a, Object? b]) {
    if (_closed) return Future.error(StateError('SearchSession is closed'));
    return _worker._call<T>(op, _id, a, b);
  }

  /// [SearchSession.hits].
  Future<List<PlacedHit>> hits(int offset, int count) => _call('hits', offset, count);

  /// [SearchSession.locateBatch].
  Future<List<Snippet?>> locateBatch(List<String> texts) => _call('locateBatch', texts);

  /// [SearchSession.locateLines].
  Future<List<Snippet?>> locateLines(List<HitLines> hits) => _call('locateLines', hits);

  /// [SearchSession.locatePairs].
  Future<List<Snippet?>> locatePairs(List<String> texts, List<String?> nextTexts) =>
      _call('locatePairs', texts, nextTexts);

  /// [SearchSession.wordWindows].
  Future<List<WordWindow>> wordWindows(int offset, int count) => _call('wordWindows', offset, count);

  /// [SearchSession.finishWindows].
  Future<List<Snippet?>> finishWindows(List<WordWindow> windows, List<String> texts, [List<String?>? nextTexts]) =>
      _call('finishWindows', windows, (texts, nextTexts));

  /// [SearchSession.countNeedsText].
  Future<bool> get countNeedsText => _call('countNeedsText');

  /// [SearchSession.countBatch].
  Future<int> countBatch(List<String> texts) => _call('countBatch', texts);

  /// [SearchSession.countBatchPairs].
  Future<int> countBatchPairs(List<String> texts, List<String?> nextTexts) =>
      _call('countBatchPairs', texts, nextTexts);

  /// [SearchSession.counts].
  Future<HitCounts> counts() => _call('counts');


  /// [SearchSession.info].
  Future<SearchInfo> info() => _call('info');

  /// [SearchSession.takeNewTerms].
  Future<List<String>> takeNewTerms() => _call('takeNewTerms');

  /// [SearchSession.groupTerms].
  Future<List<List<String>>> groupTerms() => _call('groupTerms');

  /// Sets [SearchSession.matchMode].
  Future<void> setMatchMode(SnippetMatchMode mode) => _call('matchMode', mode.index);

  /// Frees the native session on the worker. Idempotent.
  Future<void> close() {
    if (_closed || _worker._closed) {
      _closed = true;
      return Future.value();
    }
    _closed = true;
    return _worker._call<void>('close', _id);
  }
}

/// Gives a hybrid session the texts of [lineIds] (raw row content as UTF-8;
/// an absent id counts as an empty line).
typedef HybridTexts = FutureOr<Map<int, Uint8List>> Function(List<int> lineIds);

/// A [HybridSession] living on a [SearchWorker]: the same calls, each a
/// Future served by the worker. When the session needs line texts (an index
/// without positions, `need_texts`), the worker answers the step with the
/// ids; this side asks [HybridTexts] (the call's, else the session's
/// default), sends them back ([HybridSession.addTexts] there) and repeats the
/// step. So the texts come from the calling isolate's own database, and the
/// worker never waits on the host.
final class AsyncHybridSession {
  AsyncHybridSession._(this._worker, this._id, this._texts, this._rows);

  final SearchWorker _worker;
  final int _id;
  final HybridTexts? _texts;
  final SemanticRows? _rows;
  var _closed = false;

  /// [HybridSession.page]; [cancel] stops the running step
  /// ([SearchCancelledException]).
  Future<HybridPage> page(int offset, int limit, {HybridTexts? texts, SemanticRows? rows, SearchCancelToken? cancel}) async =>
      hybridPageFromJson(await _step('page', offset, limit, texts, rows, cancel));

  /// [HybridSession.window].
  Future<HybridWindow> window(int window, {HybridTexts? texts, SemanticRows? rows, SearchCancelToken? cancel}) async =>
      hybridWindowFromJson(await _step('window', window, 0, texts, rows, cancel));

  /// [HybridSession.addRows] on the worker.
  Future<void> addRows(List<SemanticRow> rows) => _call('addRows', rows);

  /// [HybridSession.addTexts] on the worker (texts given ahead of a step).
  Future<void> addTexts(List<int> ids, Map<int, Uint8List> texts) => _call('addTexts', ids, texts);

  Future<Map<String, dynamic>> _step(
      String kind, int a, int b, HybridTexts? texts, SemanticRows? rows, SearchCancelToken? cancel) async {
    // a Finalizable local: the token outlives every step of this call
    final SearchCancelToken? c = cancel;
    while (true) {
      final j = jsonDecode(await _call<String>('step', (kind, a, b), c?.address ?? 0)) as Map<String, dynamic>;
      final needRows = j['need_rows'] as List?;
      if (needRows != null) {
        final provider = rows ?? _rows ?? (throw StateError('the hybrid session needs host rows: pass rows'));
        await addRows(await provider([for (final e in needRows) e as int]));
        continue;
      }
      final need = j['need_texts'] as List?;
      if (need == null) return j;
      final provider = texts ?? _texts ?? (throw StateError('the hybrid session needs line texts: pass texts'));
      final ids = [for (final e in need) e as int];
      await addTexts(ids, await provider(ids));
    }
  }

  Future<T> _call<T>(String op, [Object? a, Object? b]) {
    if (_closed) return Future.error(StateError('HybridSession is closed'));
    return _worker._call<T>('hybrid', _id, op, (a, b));
  }

  /// Frees the session on the worker. Idempotent.
  Future<void> close() {
    if (_closed || _worker._closed) {
      _closed = true;
      return Future.value();
    }
    final f = _call<void>('close');
    _closed = true;
    return f;
  }
}

/// An [IndexWriter] living on a [SearchWorker] (issue #38): the same calls,
/// each a Future served by the worker. [mergeAndClose], [resortAndClose]
/// and [close] end it (terminal: a closed writer never reopens).
final class AsyncIndexWriter {
  AsyncIndexWriter._(this._worker, this._id);

  final SearchWorker _worker;
  final int _id;
  var _closed = false;

  Future<T> _call<T>(String op, [Object? a, Object? b]) {
    if (_closed) return Future.error(StateError('IndexWriter is closed'));
    return _worker._call<T>('writer', _id, op, (a, b));
  }

  Future<T> _end<T>(String op, [Object? a, Object? b]) {
    final f = _call<T>(op, a, b);
    _closed = true;
    return f;
  }

  /// [IndexWriter.memoryBudgetPerThread].
  Future<int> memoryBudgetPerThread() => _call('memoryBudget');

  /// [IndexWriter.add].
  Future<void> add(LineDocument line) => _call('addAll', [line]);

  /// [IndexWriter.addAll]: one message and one native call for the batch.
  Future<void> addAll(List<LineDocument> lines) => _call('addAll', lines);

  /// [IndexWriter.delete].
  Future<void> delete(int lineId) => _call('delete', lineId);

  /// [IndexWriter.putDocument] (a user index's writer: [SearchWorker.openUserWriter]).
  Future<int> putDocument(String key, String stamp, List<UserDocumentLine> lines) =>
      _call('putDocument', key, (stamp, lines));

  /// [IndexWriter.deleteDocuments] (a user index's writer).
  Future<int> deleteDocuments(List<String> keys) => _call('deleteDocuments', keys);

  /// [IndexWriter.deleteBook].
  Future<void> deleteBook(int bookId) => _call('deleteBook', bookId);

  /// [IndexWriter.commit]; the worker's index (and its searches) see it after.
  Future<void> commit([String? payload]) => _call('commit', payload);

  /// [IndexWriter.rollback].
  Future<void> rollback() => _call('rollback');

  /// [IndexWriter.mergeAndClose] (terminal; merges drained before it returns).
  Future<void> mergeAndClose() => _end('mergeAndClose');

  /// [IndexWriter.resortAndClose] (terminal). [cancel] stops it from the
  /// calling isolate; read [progress] here while it runs.
  Future<int> resortAndClose(List<int> bookOrder, {SearchCancelToken? cancel, ResortProgressCell? progress}) async {
    // both objects are Finalizable locals of this async function: kept
    // alive until it ends, so their native addresses stay valid
    final SearchCancelToken? c = cancel;
    final ResortProgressCell? p = progress;
    return await _end<int>('resortAndClose', bookOrder, (c?.address ?? 0, p?.address ?? 0));
  }

  /// [IndexWriter.close]: discards anything uncommitted. Idempotent.
  Future<void> close() {
    if (_closed) return Future.value();
    return _end('close');
  }
}

// -- worker isolate --

void _workerMain((SendPort, String, String?) args) {
  final (reply, directory, nativePath) = args;
  final SeforimIndex index;
  try {
    SeforimNative.load(nativePath);
    index = SeforimIndex.open(directory);
  } catch (e, st) {
    reply.send((e, st.toString()));
    return;
  }
  final control = ReceivePort();
  reply.send(control.sendPort);
  final sessions = <int, SearchSession>{};
  var nextSession = 1;
  final writers = <int, IndexWriter>{};
  final hybrids = <int, HybridSession>{};
  MeivinEmbedder? embedder, passageEmbedder;
  void unloadEmbedder() {
    embedder?.dispose();
    passageEmbedder?.dispose();
    embedder = passageEmbedder = null;
  }

  MeivinEmbedder model() => embedder ?? (throw StateError('no embedder is loaded (loadEmbedder)'));
  SeforimIndex? user;
  void closeUser() {
    final u = user;
    if (u == null) return;
    user = null;
    index.attachUserIndex(null);
    u.close();
  }

  control.listen((m) {
    final (id, op, a, b, c) = m as (int, String, Object?, Object?, Object?);
    try {
      Object? value;
      SearchSession s() => sessions[a as int] ?? (throw StateError('SearchSession is closed'));
      switch (op) {
        case 'search':
          final n = SeforimNative.instance;
          final h = using((al) {
            final (j, jl) = utf8Native(a as String, al);
            return n.checkPtr(c == null
                ? n.search(index.handle, j, jl)
                : n.searchCancellable(index.handle, j, jl, Pointer.fromAddress(c as int)));
          });
          // the native session holds its own reference to the token
          final session = SearchSession.internal(index, h, b as int);
          final sid = nextSession++;
          sessions[sid] = session;
          value = (sid, session.hitCount);
        case 'searchStreaming':
          final n = SeforimNative.instance;
          final h = using((al) {
            final (j, jl) = utf8Native(a as String, al);
            return n.checkPtr(n.searchStream(index.handle, j, jl, c == null ? nullptr : Pointer.fromAddress(c as int)));
          });
          final session = SearchSession.internal(index, h, b as int, null, true);
          final sid = nextSession++;
          sessions[sid] = session;
          value = (sid, session.hitCount, session.isComplete);
        case 'pull':
          final session = s();
          value = (session.pull(b as int, timeoutMs: c as int), session.hitCount);
        case 'hits':
          value = s().hits(b as int, c as int);
        case 'locateBatch':
          value = s().locateBatch((b as List).cast<String>());
        case 'locateLines':
          value = s().locateLines((b as List).cast<HitLines>());
        case 'locatePairs':
          value = s().locatePairs((b as List).cast<String>(), (c as List).cast<String?>());
        case 'wordWindows':
          value = s().wordWindows(b as int, c as int);
        case 'finishWindows':
          final (texts, next) = c as (List<String>, List<String?>?);
          value = s().finishWindows((b as List).cast<WordWindow>(), texts, next);
        case 'countNeedsText':
          value = s().countNeedsText;
        case 'countBatch':
          value = s().countBatch((b as List).cast<String>());
        case 'countBatchPairs':
          value = s().countBatchPairs((b as List).cast<String>(), (c as List).cast<String?>());
        case 'counts':
          value = s().counts();
        case 'info':
          value = s().info();
        case 'takeNewTerms':
          value = s().takeNewTerms();
        case 'groupTerms':
          value = s().groupTerms();
        case 'matchMode':
          s().matchMode = SnippetMatchMode.values[b as int];
        case 'semanticSearch':
          final (query, options) = b as (Float32List, SemanticOptions);
          value = index.semanticSearchStep(query, options, null, c as int?);
        case 'addSemanticRows':
          index.addSemanticRows((a as List).cast<SemanticRow>());
        case 'openVectors':
          value = index.openVectors(a as String);
        case 'closeVectors':
          index.closeVectors();
        case 'installVectorSegment':
          final (segment, manifest, published, model) = b as (String, String, String?, String?);
          value = installVectorSegment(a as String, segment, manifest,
              publishedManifestSha256: published, expectedModelJson: model, cancelAddress: c as int?);
        case 'compactVectorSet':
          final (policy, live) = b as (VectorCompactionPolicy?, int?);
          value = compactVectorSet(a as String, policy: policy, cancelAddress: c as int?, liveAddress: live);
        case 'verifyOxv':
          value = verifyOxv(a as String, manifestJson: b as String?, cancelAddress: c as int?);
        case 'vectorInfo':
          value = index.vectorInfo();
        case 'hybridOpen':
          final options = c as HybridOptions;
          var vector = b as Float32List?;
          final emb = embedder;
          if (vector == null && emb != null && options.retrieval != HybridRetrieval.lexicalOnly) {
            try {
              vector = emb.embedQuery(a as String);
            } on ArgumentError {
              vector = null; // no text to embed: the lexical side alone
            }
          }
          final h = index.hybridSession(a as String, vector ?? Float32List(0), options);
          final hid = nextSession++;
          hybrids[hid] = h;
          value = hid;
        case 'hybrid':
          final hid = a as int;
          final h = hybrids[hid] ?? (throw StateError('HybridSession is closed'));
          final (x, y) = c as (Object?, Object?);
          switch (b as String) {
            case 'step':
              final (kind, p, q) = x as (String, int, int);
              value = jsonEncode(hybridStep(h, kind, p, q, y as int));
            case 'addTexts':
              h.addTexts((x as List).cast<int>(), (y as Map).cast<int, Uint8List>());
            case 'addRows':
              h.addRows((x as List).cast<SemanticRow>());
            case 'close':
              hybrids.remove(hid);
              h.close();
            default:
              throw StateError('unknown hybrid op $b');
          }
        case 'embedder':
          switch (a as String) {
            case 'load':
              final (dir, ort, threads, graph) = b as (String, String, int, String);
              final (passDir, passGraph) = c as (String?, String?);
              final q = MeivinEmbedder.open(dir, ort, threads: threads, graphFile: graph);
              MeivinEmbedder? p;
              try {
                if (passGraph != null) p = MeivinEmbedder.open(passDir ?? dir, ort, threads: threads, graphFile: passGraph);
              } catch (_) {
                q.dispose();
                rethrow;
              }
              unloadEmbedder();
              embedder = q;
              passageEmbedder = p;
              value = q.dims;
            case 'unload':
              unloadEmbedder();
            case 'has':
              value = embedder != null;
            case 'query':
              value = model().embedQuery(b as String);
            case 'queries':
              value = model().embedQueries((b as List).cast<String>());
            case 'passages':
              value = (passageEmbedder ?? model()).embedPassages((b as List).cast<String>());
            case 'highlights':
              final (query, targets) = b as (String, List<PassageTarget>);
              final (forms, identity, maxChars) = c as (List<String>, int?, int);
              value = semanticPassageHighlights(query, targets, model(),
                  passageEmbedder: passageEmbedder, forms: forms, chunkingIdentity: identity, maxChars: maxChars);
            default:
              throw StateError('unknown embedder op $a');
          }
        case 'openWriter':
          final wid = nextSession++;
          writers[wid] = index.openWriter(heapBytes: a as int, threads: b as int, lowPriority: c as bool);
          value = wid;
        case 'openUserWriter':
          final u = user ?? (throw StateError('no user index is open (openUserIndex)'));
          final wid = nextSession++;
          writers[wid] = u.openWriter(heapBytes: a as int, threads: b as int, lowPriority: c as bool);
          value = wid;
        case 'user':
          switch (a as String) {
            case 'open':
              final (dir, create, options) = b as (String, bool, IndexOptions);
              closeUser();
              final u = create ? SeforimIndex.createUser(dir, options) : SeforimIndex.openUser(dir);
              try {
                index.attachUserIndex(u);
              } catch (_) {
                u.close();
                rethrow;
              }
              user = u;
            case 'close':
              closeUser();
            case 'has':
              value = user != null;
            case 'buildFmSidecar':
              value = (user ?? (throw StateError('no user index is open'))).buildFmSidecar();
            case 'reload':
              (user ?? (throw StateError('no user index is open'))).reload();
            default:
              throw StateError('unknown user op $a');
          }
        case 'writer':
          final wid = a as int;
          final w = writers[wid] ?? (throw StateError('IndexWriter is closed'));
          final (x, y) = c as (Object?, Object?);
          switch (b as String) {
            case 'memoryBudget':
              value = w.memoryBudgetPerThread;
            case 'addAll':
              w.addAll((x as List).cast<LineDocument>());
            case 'delete':
              w.delete(x as int);
            case 'putDocument':
              final (stamp, lines) = y as (String, List<UserDocumentLine>);
              value = w.putDocument(x as String, stamp, lines);
            case 'deleteDocuments':
              value = w.deleteDocuments((x as List).cast<String>());
            case 'deleteBook':
              w.deleteBook(x as int);
            case 'commit':
              w.commit(x as String?);
            case 'rollback':
              w.rollback();
            case 'mergeAndClose':
              writers.remove(wid);
              w.mergeAndClose();
            case 'resortAndClose':
              writers.remove(wid);
              final (cancel, progress) = y as (int, int);
              value = writerResortAndCloseAt(w, (x as List).cast<int>(), cancel, progress);
            case 'close':
              writers.remove(wid);
              w.close();
            default:
              throw StateError('unknown writer op $b');
          }
        case 'index':
          switch (a as String) {
            case 'buildFmSidecar':
              value = index.buildFmSidecar();
            case 'reload':
              index.reload();
            case 'releaseMemory':
              index.releaseMemory();
            case 'numLines':
              value = index.numLines;
            case 'info':
              value = index.info();
            case 'documents':
              value = index.documents();
            case 'lineSources':
              value = index.lineSources((b as List).cast<int>());
            case 'lineTexts':
              value = index.lineTexts((b as List).cast<int>());
            case 'startBackgroundMerge':
              value = index.startBackgroundMerge((b as List?)?.cast<int>());
            case 'recordBackgroundMerge':
              index.recordBackgroundMerge((b as List?)?.cast<int>());
            case 'waitBackgroundMerge':
              index.waitBackgroundMerge();
            case 'stopBackgroundMerge':
              index.stopBackgroundMerge();
            case 'backgroundMergeRunning':
              value = index.backgroundMergeRunning;
            default:
              throw StateError('unknown index op $a');
          }
        case 'close':
          sessions.remove(a as int)?.close();
        case 'dispose':
          for (final h in hybrids.values) {
            h.close();
          }
          hybrids.clear();
          unloadEmbedder();
          for (final w in writers.values) {
            w.close();
          }
          writers.clear();
          for (final s in sessions.values) {
            s.close();
          }
          sessions.clear();
          closeUser();
          index.close();
          control.close();
        default:
          throw StateError('unknown worker op $op');
      }
      reply.send((id, true, value, null));
    } catch (e, st) {
      // engine exceptions and Dart errors cross as they are; anything else
      // (not sendable for sure) as its text
      Object err = e;
      if (e is! SeforimEngineException && e is! Error) {
        err = SeforimEngineException(e.toString());
      }
      reply.send((id, false, err, st.toString()));
    }
  });
}
