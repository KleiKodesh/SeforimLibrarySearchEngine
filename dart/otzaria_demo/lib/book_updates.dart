// Issue #29: the per-book update now lives in package:seforim_search_sqlite/seforim_search_sqlite.dart;
// the demo keeps its `update` command here.
import 'dart:async';
import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:seforim_search_sqlite/seforim_search_sqlite.dart';

export 'package:seforim_search_sqlite/seforim_search_sqlite.dart';

/// The demo's `update [--wait] [--watch]` command: one update, optionally
/// waiting for the background merge; `--watch` polls the DB file stamp
/// (size + write time of the file and its WAL) every 2 s and updates again
/// after a change. Returns the exit code (3 = rebuild needed).
Future<int> runUpdateCommand(SeforimIndex index, SeforimDb db, {bool wait = false, bool watch = false}) async {
  String stamp() {
    String part(String p) {
      final f = File(p);
      if (!f.existsSync()) return '-';
      final s = f.statSync();
      return '${s.size}:${s.modified.microsecondsSinceEpoch}';
    }

    return '${part(db.path)}|${part('${db.path}-wal')}';
  }

  int once() {
    final sw = Stopwatch()..start();
    final r = updateChangedBooks(index, db, log: stderr.writeln);
    stderr.writeln('update: $r; ${sw.elapsedMilliseconds} ms');
    if (!r.applicable) return 3;
    if (wait && r.mergeStarted) {
      final m = Stopwatch()..start();
      index.waitBackgroundMerge();
      final ms = m.elapsedMilliseconds;
      stderr.writeln('background merge done: ${ms < 1000 ? '$ms ms' : '${(ms / 60000).toStringAsFixed(2)} min'}');
    }
    return 0;
  }

  var last = stamp();
  var code = once();
  while (watch) {
    await Future<void>.delayed(const Duration(seconds: 2));
    final now = stamp();
    if (now == last) continue;
    last = now;
    stderr.writeln('seforim.db changed');
    code = once();
  }
  return code;
}
