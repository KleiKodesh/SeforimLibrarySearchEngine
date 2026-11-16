// Issue #29: the per-book index update, Otzaria's rule (book.contentHash).
import 'dart:convert';

import '../../seforim_search.dart';

import 'note_ids.dart';
import 'data_source.dart';

/// [updateChangedBooks] stopped by its `cancelled` callback (the books not
/// committed yet are redone by the next call).
class BookUpdateCancelled implements Exception {
  const BookUpdateCancelled();
  @override
  String toString() => 'per-book update cancelled';
}

/// What [updateChangedBooks] did.
class BookUpdateReport {
  /// False: a per-book update cannot serve this change (no built index, the
  /// DB has no `book.contentHash`, or an index without the per-book record
  /// whose DB already changed): rebuild.
  bool applicable = true;
  String reason = '';
  int changed = 0, added = 0, removed = 0, linesWritten = 0;

  /// A line count changed or a book was added / removed: the background
  /// merge is a repack in the catalog order (positions renumbered).
  bool repack = false;
  bool mergeStarted = false;

  @override
  String toString() => !applicable
      ? 'per-book update not applicable: $reason'
      : '$changed changed, $added added, $removed removed books; $linesWritten lines written; '
          'background ${repack ? 'repack' : 'merge'} ${mergeStarted ? 'started' : 'not needed'}';
}

/// The index payload of the demo: `{"lines":N,"db":fingerprint,"notes":0|1}`
/// from the build, plus (issue #29) `"books":{"id":[hash,lines,notes]}` and
/// `"pending":[ids]` once per-book updates run.
class _Payload {
  _Payload(this.json);
  final Map<String, dynamic> json;

  static _Payload? parse(String? s) {
    if (s == null || s.isEmpty) return null;
    try {
      final j = jsonDecode(s);
      return j is Map<String, dynamic> && j.containsKey('lines') ? _Payload(j) : null;
    } catch (_) {
      return null;
    }
  }

  Map<int, List<int>>? get books => (json['books'] as Map<String, dynamic>?)
      ?.map((k, v) => MapEntry(int.parse(k), [for (final x in v as List) (x as num).toInt()]));
  set books(Map<int, List<int>>? b) => json['books'] = b?.map((k, v) => MapEntry('$k', v));
  List<int> get pending => [for (final x in (json['pending'] as List?) ?? const []) (x as num).toInt()];
  set pending(List<int> p) => p.isEmpty ? json.remove('pending') : json['pending'] = p;

  @override
  String toString() => jsonEncode(json);
}

/// A stamp of the catalog book order the positions follow (FNV-1a, 32 bit).
String _orderStamp(List<int> order) {
  var h = 0x811c9dc5;
  for (final id in order) {
    h = ((h ^ (id & 0xffffffff) ^ (id >> 32)) * 0x01000193) & 0xffffffff;
  }
  return h.toRadixString(16);
}

/// The per-book update (issue #29, Otzaria's rule): compares each book's
/// `book.contentHash` with the hashes the index recorded in its payload, then
/// for every changed, new or removed book deletes all its lines (and its
/// virtual notes book's, issue #13) and adds its current lines; then starts
/// the engine's background full merge. Run it on app start and whenever the
/// DB file changes.
///
/// Positions: a written book's lines get their positions in the CURRENT
/// catalog numbering. When no line count changed and no book was added or
/// removed, that equals the index's numbering: a plain merge keeps every
/// position. Otherwise the books after the change moved: the merge is a
/// REPACK in the current catalog order (the engine renumbers every line
/// densely, = a fresh build), recorded with the engine BEFORE the first write
/// so a crash mid-update still repacks.
///
/// Crash safety: the books to write are recorded in the payload (`pending`)
/// first; each commit holds whole books only, with their new hashes; the
/// next call redoes what is left. Counts stay "exact" meanwhile (Otzaria's
/// behaviour, no stale flag).
BookUpdateReport updateChangedBooks(SeforimIndex index, SeforimDataSource db,
    {void Function(String)? log, int linesPerCommit = 20000, bool Function()? cancelled}) {
  final report = BookUpdateReport();
  BookUpdateReport notApplicable(String why) => report
    ..applicable = false
    ..reason = why;

  final payload = _Payload.parse(index.info().payload);
  if (payload == null) return notApplicable('no built index');
  final hashes = db.bookContentHashes();
  if (hashes == null) return notApplicable('the database has no book.contentHash');
  final fingerprint = db.fingerprint();
  final dbChanged = payload.json['db'] != fingerprint;
  final order = db.catalogBookOrder();
  final counts = db.bookLineCounts();
  final first = <int, int>{}; // engine positions, from 0
  var next = 0;
  for (final b in order) {
    first[b] = next;
    next += counts[b] ?? 0;
  }
  final current = {
    for (final e in hashes.entries) e.key: [e.value, counts[e.key] ?? 0, counts[notesBookId(e.key)] ?? 0]
  };

  final recorded = payload.books;
  if (recorded == null) {
    // built before the per-book record: adopt the DB's hashes when it still is
    // the database the index was built from
    if (dbChanged) return notApplicable('index has no per-book record and the DB changed');
    payload.books = current;
    payload.json['order'] = _orderStamp(order);
    final w = index.openWriter(heapBytes: 30000000, threads: 1);
    try {
      w.commit('$payload');
    } finally {
      w.close();
    }
    log?.call('recorded ${current.length} book hashes');
    return report;
  }

  final todo = {...payload.pending};
  for (final e in current.entries) {
    final was = recorded[e.key];
    if (was == null || was[0] != e.value[0]) todo.add(e.key);
  }
  for (final b in recorded.keys) {
    if (!current.containsKey(b)) todo.add(b);
  }
  if (todo.isEmpty) {
    if (dbChanged) {
      payload.json['db'] = fingerprint;
      final w = index.openWriter(heapBytes: 30000000, threads: 1);
      try {
        w.commit('$payload');
      } finally {
        w.close();
      }
    }
    // a recorded repack the engine has not finished resumes
    report.mergeStarted = index.startBackgroundMerge();
    return report;
  }
  for (final b in todo) {
    final now = current[b], was = recorded[b];
    if (now != null && was != null) {
      report.changed++;
    } else if (now != null) {
      report.added++;
    } else {
      report.removed++;
    }
    if (now == null || was == null || now[1] != was[1] || now[2] != was[2]) report.repack = true;
  }
  // the books' tree order changed since the index's numbering: the written
  // books' new positions would overlap the others' old ones, so repack (an
  // order change alone is not acted on: re-sorting is the host's choice)
  final orderStamp = _orderStamp(order);
  if (payload.json['order'] != orderStamp) report.repack = true;
  final rank = {for (var i = 0; i < order.length; i++) order[i]: i};
  final books = todo.toList()..sort((a, b) => (rank[a] ?? -1).compareTo(rank[b] ?? -1));
  log?.call('per-book update: ${report.changed} changed, ${report.added} added, ${report.removed} removed'
      '${report.repack ? ' (line counts changed: repack after)' : ''}');

  final repackOrder = report.repack ? order : null;
  final w = index.openWriter(heapBytes: 64000000, threads: 2);
  try {
    if (repackOrder != null) index.recordBackgroundMerge(repackOrder);
    payload.pending = books;
    w.commit('$payload');
    final remaining = [...books];
    var sinceCommit = 0, done = 0;
    for (final book in books) {
      if (cancelled?.call() ?? false) throw const BookUpdateCancelled();
      w.deleteBook(book);
      w.deleteBook(notesBookId(book));
      final stamp = current[book];
      if (stamp != null) {
        for (final id in [book, notesBookId(book)]) {
          final start = first[id];
          if (start == null) continue;
          var position = start;
          final batch = <LineDocument>[];
          for (final line in db.bookLines(id)) {
            batch.add(LineDocument(
                lineId: line.id,
                position: position++,
                bookId: id,
                text: line.text,
                sectionId: line.tocEntryId == 0 ? null : line.tocEntryId,
                noCross: line.text.startsWith('<h') || isVirtualId(line.id)));
            if (batch.length == 1000) {
              w.addAll(batch);
              batch.clear();
            }
            sinceCommit++;
            report.linesWritten++;
          }
          if (batch.isNotEmpty) w.addAll(batch);
        }
        recorded[book] = stamp;
      } else {
        recorded.remove(book);
      }
      remaining.remove(book);
      done++;
      // whole books only per commit, with their new hashes
      if (sinceCommit >= linesPerCommit || remaining.isEmpty) {
        payload.books = recorded;
        payload.pending = remaining;
        if (remaining.isEmpty) {
          payload.json['db'] = fingerprint;
          payload.json['lines'] = next;
          if (report.repack) payload.json['order'] = orderStamp;
        }
        w.commit('$payload');
        sinceCommit = 0;
        log?.call('  $done / ${books.length} books, ${report.linesWritten} lines');
      }
    }
  } finally {
    w.close(); // discards an uncommitted book (the next call redoes it)
  }
  // the engine picks up a recorded repack's order
  report.mergeStarted = index.startBackgroundMerge(repackOrder);
  return report;
}
