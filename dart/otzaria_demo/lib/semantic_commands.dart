// Semantic search in the host (issue #18): the engine opens Otzaria's own
// vector set read-only and scans it; the host maps each book of the set
// (`id:<bookId>`) onto seforim.db line ids and embeds the query with
// Otzaria's Meivin model. The model and SQLite stay here, never in the engine.

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_search.dart';

import 'meivin_embedder.dart';
import 'otzaria_query.dart';
import 'seforim_db.dart';

String _ms(num ms) => ms < 1000 ? '${ms.toStringAsFixed(ms < 100 ? 1 : 0)} ms' : '${(ms / 60000).toStringAsFixed(2)} min';

/// Why a book of the vector set was not mapped.
enum OxvSkip { badName, notInDb, moreLinesThanDb }

int? _bookIdOf(String name) {
  final m = RegExp(r'^id:(\d+)$').firstMatch(name);
  return m == null ? null : int.parse(m.group(1)!);
}

/// Maps one book of an Otzaria vector set onto the DB's line ids WITHOUT the
/// text check (`--no-verify`): the line id of each 0-based ordinal
/// (`lineIndex, id` order). Null + [skip] when the book cannot be mapped.
OxvBookMap? mapOxvBook(SeforimDb db, OxvBookInfo book, void Function(OxvSkip) skip) {
  final bookId = _bookIdOf(book.name);
  if (bookId == null) {
    skip(OxvSkip.badName);
    return null;
  }
  final ids = [for (final line in db.bookLines(bookId)) line.id];
  if (ids.isEmpty) {
    skip(OxvSkip.notInDb);
    return null;
  }
  if (book.lines > ids.length) {
    skip(OxvSkip.moreLinesThanDb);
    return null;
  }
  return OxvBookMap(book.name, bookId, ids);
}

/// The book's `line_note` rows with a body, in `lineIndex, noteIndex` order
/// (empty when the DB has no `line_note`).
/// Bodies are stored like line content (zstd frames with a dictionary on
/// Otzaria 6+): decoded with the DB's codec, never read as raw UTF-8 bytes
/// (that turned every note into binary noise and failed the text check of
/// about 10% of the records: the books with footnotes).
List<VectorBookNote> bookNotes(SeforimDb db, int bookId) {
  if (!db.hasNotes) return const [];
  final cols = {for (final r in db.db.select('PRAGMA table_info(line_note)')) r['name'] as String};
  final rs = db.db.select(
      'SELECT l.lineIndex, ${cols.contains('charPos') ? 'n.charPos' : 'NULL'}, '
      '${cols.contains('hasMarker') ? 'n.hasMarker' : '0'}, typeof(n.body), CAST(n.body AS BLOB), n.lineId FROM line_note n JOIN line l ON l.id = n.lineId'
      ' WHERE l.bookId = ? AND n.body IS NOT NULL ORDER BY l.lineIndex, l.id, n.noteIndex',
      [bookId]);
  return [
    for (final r in rs)
      VectorBookNote(r.columnAt(0) as int,
          _keepBom(db.rawContent(r.columnAt(3) as String, r.columnAt(4) as Uint8List?, r.columnAt(5) as int)),
          charPos: (r.columnAt(1) as int?) ?? -1, hasMarker: ((r.columnAt(2) as int?) ?? 0) != 0),
  ];
}

/// UTF-8 [bytes] as a String with a leading BOM kept (utf8.decode drops it).
String _keepBom(Uint8List bytes) {
  final bom = bytes.length >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF;
  return bom ? '﻿${utf8.decode(bytes.sublist(3), allowMalformed: true)}' : utf8.decode(bytes, allowMalformed: true);
}

/// The `chunking_identity` of `<model dir>/model.json`; 0 (= the set's) when absent.
int modelChunkingIdentity(String modelDir) {
  final f = File('$modelDir/model.json');
  if (!f.existsSync()) return 0;
  try {
    final v = (jsonDecode(f.readAsStringSync()) as Map<String, dynamic>)['chunking_identity'];
    return v is int ? v : v is String ? int.parse(v) : 0;
  } on FormatException catch (e) {
    stderr.writeln('model.json: chunking_identity not read ($e)');
    return 0;
  }
}

/// The books of an opened set (name -> line count), read from its segment
/// files' headers ([vectors] = the set directory or one base `.oxv`).
Map<String, OxvBookInfo> setBooks(String vectors, VectorSetInfo info) {
  final files = <String>[];
  if (File(vectors).existsSync()) {
    files.add(vectors);
  } else {
    for (final s in info.segments) {
      final name = s.file.split(RegExp(r'[\\/]')).last;
      final hit = [s.file, '$vectors/${s.file}', '$vectors/segments/$name'].where((f) => File(f).existsSync());
      if (hit.isEmpty) {
        stderr.writeln('segment file not found: ${s.file}');
      } else {
        files.add(hit.first);
      }
    }
  }
  final books = <String, OxvBookInfo>{};
  for (final f in files) {
    final OxvInfo oi;
    try {
      oi = readOxvInfo(f);
    } on SeforimEngineException catch (e) {
      stderr.writeln('segment header not read ($f): $e');
      continue;
    }
    for (final b in oi.books) {
      final old = books[b.name];
      if (old == null || b.lines > old.lines) books[b.name] = b;
    }
  }
  return books;
}

/// Totals of the text check over every book ([mapVectorBooks]).
final class VerifyTotals {
  int books = 0, lines = 0, records = 0, verified = 0, moved = 0, unverified = 0;

  /// Per mapped book: (book id, DB rows, notes, the check's counts).
  final perBook = <(int, int, int, VectorBookVerify)>[];
}

/// Maps every `id:<bookId>` book of the opened set onto the DB. Default:
/// Otzaria's text check ([SemanticIndex.mapVectorBookRows]: the book's raw
/// rows in `lineIndex, id` order and its `line_note` rows; a row holding a
/// newline gives several lines). [verify] false: the unchecked ordinal
/// mapping ([SemanticIndex.mapVectorBook]). Returns the skip counts.
Map<OxvSkip, int> mapVectorBooks(SeforimIndex index, SeforimDb db, String vectors, VectorSetInfo info,
    {bool verify = true, int chunkingIdentity = 0, VerifyTotals? totals}) {
  final skips = {for (final s in OxvSkip.values) s: 0};
  for (final b in setBooks(vectors, info).values) {
    if (!verify) {
      final m = mapOxvBook(db, b, (s) => skips[s] = skips[s]! + 1);
      if (m != null) {
        index.mapVectorBook(m.name, m.bookId, m.lineIds);
        totals?.books++;
      }
      continue;
    }
    final bookId = _bookIdOf(b.name);
    if (bookId == null) {
      skips[OxvSkip.badName] = skips[OxvSkip.badName]! + 1;
      continue;
    }
    final ids = <int>[];
    final rows = <Uint8List>[];
    for (final (id, raw) in db.bookRawRows(bookId)) {
      ids.add(id);
      rows.add(raw);
    }
    if (ids.isEmpty) {
      skips[OxvSkip.notInDb] = skips[OxvSkip.notInDb]! + 1;
      continue;
    }
    final notes = bookNotes(db, bookId);
    final v = index.mapVectorBookRows(b.name, bookId, ids, rows, notes, chunkingIdentity);
    if (totals != null) {
      totals.perBook.add((bookId, ids.length, notes.length, v));
      totals
        ..books += 1
        ..lines += v.lines
        ..records += v.records
        ..verified += v.verified
        ..moved += v.moved
        ..unverified += v.unverified;
    }
  }
  return skips;
}

/// `semantic-verify <scratch index dir> --vectors <set dir | file.oxv> [--chunking-identity N] [--report N]`:
/// Otzaria's text check over every `id:N` book of the set, the totals, the time and the
/// [report] books with the most unverified records (DB rows, notes, lines, records, unverified).
void semanticVerifyCommand(SeforimDb db, String indexDir, String vectors, {int chunkingIdentity = 0, int report = 30}) {
  final dir = Directory(indexDir);
  final index = dir.existsSync() && dir.listSync().isNotEmpty ? SeforimIndex.open(indexDir) : SeforimIndex.create(indexDir);
  try {
    final info = index.openVectors(vectors);
    final totals = VerifyTotals();
    final sw = Stopwatch()..start();
    final skips = mapVectorBooks(index, db, vectors, info, chunkingIdentity: chunkingIdentity, totals: totals);
    final ms = sw.elapsedMilliseconds;
    stdout.writeln('mapping + verify: ${ms < 1000 ? '$ms ms' : '${(ms / 60000).toStringAsFixed(2)} min'}, '
        'books ${totals.books}, skipped ${skips.entries.where((e) => e.value > 0).map((e) => '${e.key.name} ${e.value}').join(', ')}');
    stdout.writeln('text check: lines ${totals.lines}, records ${totals.records}, verified ${totals.verified}, '
        'moved ${totals.moved}, unverified ${totals.unverified}');
    final bad = totals.perBook.where((b) => b.$4.unverified > 0).length;
    final all = totals.perBook.where((b) => b.$4.records > 0 && b.$4.unverified == b.$4.records).length;
    stdout.writeln('books with unverified records: $bad; all records unverified: $all');
    final worst = [...totals.perBook]..sort((a, b) => b.$4.unverified.compareTo(a.$4.unverified));
    for (final (book, rows, notes, v) in worst.take(report)) {
      stdout.writeln('  book $book: rows $rows notes $notes lines ${v.lines} records ${v.records} '
          'verified ${v.verified} moved ${v.moved} unverified ${v.unverified}');
    }
  } finally {
    index.close();
  }
}

String _stripTags(String s) => s.replaceAll(RegExp(r'<[^>]*>'), ' ');

/// `semantic-search <index dir> --vectors <set dir | file.oxv> (--line N | --lines a,b,c | --query text) [--k N] [--books a,b] [--threads N] [--hybrid] [--repeat N] [--no-verify] [--chunking-identity N]`.
void semanticSearchCommand(SeforimDb db, String indexDir, String vectors,
    {required String modelDir,
    required String ortLibrary,
    List<int> lines = const [],
    String? query,
    int k = 10,
    List<int>? books,
    int threads = 0,
    bool hybrid = false,
    int repeat = 1,
    bool verify = true,
    int? chunkingIdentity}) {
  final queries = <(int, String)>[];
  final texts = db.lineTexts(lines);
  for (final id in lines) {
    final t = _stripTags(texts[id] ?? '');
    if (t.trim().isEmpty) {
      stdout.writeln('line $id: no text, skipped');
    } else {
      queries.add((id, t));
    }
  }
  if (query != null && query.trim().isNotEmpty) queries.add((0, query));
  if (queries.isEmpty) {
    stderr.writeln('no query: pass --line, --lines or --query');
    exitCode = 64;
    return;
  }
  final index = SeforimIndex.open(indexDir);
  final sw = Stopwatch()..start();
  try {
    var info = index.openVectors(vectors);
    stdout.writeln('open: ${_ms(sw.elapsedMicroseconds / 1000)}');
    sw.reset();
    final totals = VerifyTotals();
    final skips = mapVectorBooks(index, db, vectors, info,
        verify: verify, chunkingIdentity: chunkingIdentity ?? modelChunkingIdentity(modelDir), totals: totals);
    info = index.vectorInfo()!;
    stdout.writeln('mapping + verify: ${_ms(sw.elapsedMicroseconds / 1000)}, books mapped ${totals.books} '
        '(set: mapped ${info.booksMapped}, unmapped ${info.booksUnmapped}); skipped: '
        '${skips.entries.where((e) => e.value > 0).map((e) => '${e.key.name} ${e.value}').join(', ')}');
    if (verify) {
      stdout.writeln('text check: lines ${totals.lines}, records ${totals.records}, verified ${totals.verified}, '
          'moved ${totals.moved}, unverified ${totals.unverified}');
    }
    stdout.writeln('set: generation ${info.generation}, dims ${info.dims}, codec ${info.codec}, '
        'library ${info.libraryReleaseTag}, slots_live ${info.slotsLive}, slots_dead ${info.slotsDead}, '
        'books ${info.books}, ${(info.bytesOnDisk / 1048576).toStringAsFixed(0)} MB, ${info.segments.length} segments');
    sw.reset();
    final emb = MeivinEmbedder.open(modelDir, ortLibrary);
    stdout.writeln('model load: ${_ms(sw.elapsedMicroseconds / 1000)}');
    final latencies = <double>[];
    var withLine = 0, selfFirst = 0;
    final options = SemanticOptions(k: k, books: books, threads: threads);
    try {
      for (final (line, text) in queries) {
        sw.reset();
        final tokens = emb.queryTokens(text);
        final v = emb.embedTokens(tokens);
        final embedMs = sw.elapsedMicroseconds / 1000;
        late SemanticResult sem;
        var last = 0.0;
        for (var i = 0; i < repeat; i++) {
          sw.reset();
          sem = index.semanticSearch(v, options);
          last = sw.elapsedMicroseconds / 1000;
          latencies.add(last);
        }
        stdout.writeln('${line != 0 ? 'line $line' : 'query'}: embed ${_ms(embedMs)} (${tokens.length} tokens), '
            'search ${_ms(last)}; vectors ${sem.vectors}, records ${sem.records}, unmapped ${sem.unmapped}, '
            'scanned ${sem.scanned}, threads ${sem.threads}');
        for (final (i, h) in sem.hits.indexed) {
          final alias = h.chunk & SemanticHit.borrowed != 0 ? ' (alias)' : '';
          stdout.writeln('  ${i + 1}. line ${h.lineId} book ${h.bookId} score ${h.score.toStringAsFixed(4)} '
              'chunk ${h.chunk & ~SemanticHit.borrowed}$alias${line != 0 && h.lineId == line ? '   <- source line' : ''}');
        }
        if (line != 0) {
          withLine++;
          final first = sem.hits.isNotEmpty && sem.hits.first.lineId == line;
          if (first) selfFirst++;
          stdout.writeln('  source line first: ${first ? 'yes' : 'no'}');
        }
        if (hybrid) {
          final request = OtzariaSearch(query: text).toOptions(contextWords: 8)..order = ResultsOrder.relevance;
          request.baseBooks.addAll(db.baseBooks());
          if (books != null) request.books = books;
          final session = index.search(request);
          try {
            // a streamed search (positions index) is pulled to the end first
            while (!session.pull(1 << 40)) {}
            final fused = session.fuseSemantic(text, sem);
            stdout.writeln('  hybrid (${session.hitCount} lexical hits):');
            for (final (i, f) in fused.take(k).indexed) {
              stdout.writeln('    ${i + 1}. line ${f.lineId} book ${f.bookId} fused ${f.fused.toStringAsFixed(4)} '
                  'lexical ${f.lexical?.toStringAsFixed(4) ?? '-'} semantic ${f.semantic?.toStringAsFixed(4) ?? '-'} '
                  '(${f.source})');
            }
          } finally {
            session.close();
          }
        }
      }
    } finally {
      emb.close();
    }
    final sorted = [...latencies]..sort();
    final p95 = sorted[((0.95 * sorted.length).ceil() - 1).clamp(0, sorted.length - 1)];
    final mean = sorted.reduce((a, b) => a + b) / sorted.length;
    stdout.writeln('search latency over ${sorted.length} runs: mean ${mean.toStringAsFixed(1)} ms, '
        'p95 ${p95.toStringAsFixed(1)} ms');
    if (withLine > 0) stdout.writeln('source line ranked first: $selfFirst of $withLine');
    stdout.writeln('peak working set: ${(ProcessInfo.maxRss / 1048576).toStringAsFixed(0)} MB');
  } finally {
    index.close();
  }
}
