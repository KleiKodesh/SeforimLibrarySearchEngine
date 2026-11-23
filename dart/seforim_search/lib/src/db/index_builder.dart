// First-run index build and per-book update from seforim.db, for hosts
// (Otzaria, demos): the same steps as the demo's `build` command, with a
// progress stream and cancellation, run on a background isolate.
import 'dart:async';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

import '../../seforim_search.dart';
import 'book_updates.dart';
import 'note_ids.dart';
import 'data_source.dart';

/// The stage of an index build.
enum IndexBuildStage { indexing, merging, fmSidecar, updating, done }

/// One progress report of [SeforimIndexBuilder]: [done] of [total] lines
/// while indexing (total = every line + note of the DB); 0 / 0 in the
/// stages that are one native call.
final class IndexBuildProgress {
  const IndexBuildProgress(this.stage, this.done, this.total, {this.elapsedMs = 0, this.message});
  final IndexBuildStage stage;
  final int done, total, elapsedMs;
  final String? message;
  double? get fraction => total > 0 ? done / total : null;
  @override
  String toString() => '${stage.name} $done/$total ${elapsedMs}ms${message == null ? '' : ' $message'}';
}

/// Thrown by [buildIndexFromDb] when its `cancelled` callback returns true.
class IndexBuildCancelled implements Exception {
  const IndexBuildCancelled();
  @override
  String toString() => 'index build cancelled';
}

/// Builds a fresh index at [dir] from [db] in catalog order: every line (and
/// every footnote as its virtual notes book, issue #13) with its section and
/// no-cross flag, then one merged segment and the FM-index sidecar. The
/// payload records the DB fingerprint and line count (what
/// [updateChangedBooks] and [SeforimIndexBuilder.needsBuild] read).
/// Synchronous: call it on a background isolate (or use [SeforimIndexBuilder]).
/// A cancelled or failed build deletes the half-built directory.
void buildIndexFromDb(SeforimDataSource db, String dir,
    {IndexOptions options = const IndexOptions(),
    void Function(IndexBuildProgress)? onProgress,
    bool Function()? cancelled,
    int maxBooks = 0}) {
  db.checkNoteIds();
  final sw = Stopwatch()..start();
  void report(IndexBuildStage s, int done, int total) =>
      onProgress?.call(IndexBuildProgress(s, done, total, elapsedMs: sw.elapsedMilliseconds));
  final books = maxBooks > 0 ? db.catalogBookOrder().take(maxBooks).toList() : db.catalogBookOrder();
  var total = 0;
  if (onProgress != null) {
    for (final b in books) {
      total += db.bookLineCount(b);
    }
  }
  final d = Directory(dir);
  if (d.existsSync()) d.deleteSync(recursive: true);
  final index = SeforimIndex.create(dir, options);
  try {
    final w = index.openWriter();
    var position = 0;
    final batch = <LineDocument>[];
    var batchBytes = 0;
    runInBackgroundPriority(() {
      try {
        report(IndexBuildStage.indexing, 0, total);
        for (final bookId in books) {
          if (cancelled?.call() ?? false) throw const IndexBuildCancelled();
          setChunkKeys(w, db, bookId);
          for (final line in db.bookLines(bookId)) {
            batch.add(LineDocument(
                lineId: line.id,
                position: position++,
                bookId: line.bookId,
                text: line.text,
                sectionId: line.tocEntryId == 0 ? null : line.tocEntryId,
                noCross: line.text.startsWith('<h') || isVirtualId(line.id)));
            batchBytes += 2 * line.text.length + 64;
            if (batch.length == 1000 || batchBytes >= 16000000) {
              w.addAll(batch);
              batch.clear();
              batchBytes = 0;
              report(IndexBuildStage.indexing, position, total);
            }
          }
        }
        w.addAll(batch);
        w.commit('{"lines":$position,"db":"${db.fingerprint()}","notes":${db.hasNotes ? 1 : 0},"chunk_keys":1}');
      } catch (_) {
        w.rollback();
        rethrow;
      } finally {
        w.close();
      }
    });
    report(IndexBuildStage.indexing, position, total);
    if (cancelled?.call() ?? false) throw const IndexBuildCancelled();
    report(IndexBuildStage.merging, 0, 0);
    index.openWriter().mergeAndClose();
    report(IndexBuildStage.fmSidecar, 0, 0);
    index.buildFmSidecar();
    report(IndexBuildStage.done, position, total);
  } catch (_) {
    index.close();
    try {
      d.deleteSync(recursive: true);
    } catch (_) {}
    rethrow;
  }
  index.close();
}

/// A real book's semantic chunk keys ([IndexWriter.setBookChunkKeys]) before
/// its lines; a notes book has none (its lines are keyed with their book's).
void setChunkKeys(IndexWriter w, SeforimDataSource db, int bookId) {
  if (isVirtualId(bookId)) return;
  final b = db.bookChunkRows(bookId);
  if (b.ids.isNotEmpty) w.setBookChunkKeys(b.ids, b.rows, b.notes);
}

/// Background-isolate index build / update from seforim.db for a host's
/// first run and later DB changes.
abstract final class SeforimIndexBuilder {
  /// True when [dir] holds no usable index: missing, not
  /// openable (an older index format), built without a payload, or (with
  /// [options]) built with other index options.
  static bool needsBuild(String dir, {IndexOptions? options}) {
    if (!SeforimIndex.exists(dir)) return true;
    try {
      final i = SeforimIndex.open(dir);
      try {
        final info = i.info();
        final p = info.payload;
        // built with other options (positions / keep marks): rebuild
        if (options != null && (info.positions != options.positions || info.keepMarks != options.keepMarks)) {
          return true;
        }
        // built before the semantic chunk key column was written: rebuild
        return p == null || !p.contains('"lines"') || !p.contains('"chunk_keys"');
      } finally {
        i.close();
      }
    } catch (_) {
      return true;
    }
  }

  /// Builds [dir] on a new isolate from the database [open] opens there
  /// (the host's own [SeforimDataSource]; closed after the build); the
  /// stream reports progress and closes after [IndexBuildStage.done] (or with
  /// the error, [IndexBuildCancelled] after [cancel] completes).
  /// [nativePath]: the native library for the isolate (default: the
  /// package's own).
  static Stream<IndexBuildProgress> build(SeforimDataSourceOpener open, String dir,
          {IndexOptions options = const IndexOptions(), Future<void>? cancel, int maxBooks = 0, String? nativePath}) =>
      _run(cancel, nativePath, (send, isCancelled) {
        final db = open();
        try {
          buildIndexFromDb(db, dir,
              options: options, onProgress: send, cancelled: isCancelled, maxBooks: maxBooks);
        } finally {
          db.close();
        }
      });

  /// Builds when [needsBuild], otherwise runs the per-book update
  /// ([updateChangedBooks]); an update that cannot serve the change
  /// (replaced DB without content hashes) rebuilds.
  static Stream<IndexBuildProgress> ensure(SeforimDataSourceOpener open, String dir,
      {IndexOptions options = const IndexOptions(), Future<void>? cancel, String? nativePath}) {
    if (needsBuild(dir, options: options)) return build(open, dir, options: options, cancel: cancel, nativePath: nativePath);
    return _run(cancel, nativePath, (send, isCancelled) {
      final db = open();
      try {
        final index = SeforimIndex.open(dir);
        BookUpdateReport r;
        try {
          send(const IndexBuildProgress(IndexBuildStage.updating, 0, 0));
          r = updateChangedBooks(index, db, cancelled: isCancelled);
          if (r.applicable && r.mergeStarted) index.waitBackgroundMerge();
        } finally {
          index.close();
        }
        if (!r.applicable) {
          buildIndexFromDb(db, dir, options: options, onProgress: send, cancelled: isCancelled);
        } else {
          send(IndexBuildProgress(IndexBuildStage.done, r.linesWritten, r.linesWritten, message: '$r'));
        }
      } finally {
        db.close();
      }
    });
  }

  static Stream<IndexBuildProgress> _run(
      Future<void>? cancel, String? nativePath, void Function(void Function(IndexBuildProgress), bool Function()) body) {
    final flag = calloc<Uint8>();
    final address = flag.address;
    final ctl = StreamController<IndexBuildProgress>();
    final port = ReceivePort();
    var freed = false;
    cancel?.then((_) {
      if (!freed) flag.value = 1;
    });
    port.listen((m) {
      if (m is IndexBuildProgress) {
        ctl.add(m);
      } else {
        port.close();
        freed = true;
        calloc.free(flag);
        if (m is List && m.length == 2) {
          ctl.addError(m[0] == 'cancelled' ? const IndexBuildCancelled() : Exception(m[0]), m[1] as StackTrace?);
        }
        ctl.close();
      }
    });
    _launch(port.sendPort, address, nativePath, body).catchError((Object e) {
      port.sendPort.send(['$e', null]);
    });
    return ctl.stream;
  }

  // separate so the isolate's closure captures only sendable values
  static Future<void> _launch(SendPort send, int address, String? nativePath,
          void Function(void Function(IndexBuildProgress), bool Function()) body) =>
      Isolate.run(() {
        final f = Pointer<Uint8>.fromAddress(address);
        try {
          SeforimNative.load(nativePath);
          body(send.send, () => f.value != 0);
          send.send(null);
        } on IndexBuildCancelled {
          send.send(['cancelled', null]);
        } on BookUpdateCancelled {
          send.send(['cancelled', null]);
        } catch (e, st) {
          send.send(['$e', st]);
        }
      });
}
