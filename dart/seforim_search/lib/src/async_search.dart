import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'models.dart';
import 'native.dart';
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
        case 'matchMode':
          s().matchMode = SnippetMatchMode.values[b as int];
        case 'close':
          sessions.remove(a as int)?.close();
        case 'dispose':
          for (final s in sessions.values) {
            s.close();
          }
          sessions.clear();
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
