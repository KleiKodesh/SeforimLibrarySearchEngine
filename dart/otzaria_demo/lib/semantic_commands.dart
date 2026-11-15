// Semantic search in the host (issue #18): Otzaria's prebuilt vector files
// (.oxv) mapped onto seforim.db line ids and imported into the engine's
// vector sidecar, and the query embedded with Otzaria's Meivin model. The
// model and SQLite stay here, never in the engine.

import 'dart:io';

import 'package:seforim_search/seforim_search.dart';

import 'meivin_embedder.dart';
import 'otzaria_query.dart';
import 'seforim_db.dart';

String _ms(int ms) => ms < 1000 ? '$ms ms' : '${(ms / 60000).toStringAsFixed(2)} min';

/// Why a book of the vector file was not mapped.
enum OxvSkip { badName, notInDb, moreLinesThanDb, rowWithNewline, notSelected }

/// Maps one book of an Otzaria vector file onto the DB's line ids: the line
/// id of each 0-based ordinal (`lineIndex, id` order). Null + [skip] when the
/// book cannot be mapped exactly.
OxvBookMap? mapOxvBook(SeforimDb db, OxvBookInfo book, void Function(OxvSkip) skip) {
  final m = RegExp(r'^id:(\d+)$').firstMatch(book.name);
  if (m == null) {
    skip(OxvSkip.badName);
    return null;
  }
  final bookId = int.parse(m.group(1)!);
  final ids = <int>[];
  for (final line in db.bookLines(bookId)) {
    // Otzaria splits a row holding a newline into several lines: the
    // ordinals of the file then no longer follow the DB rows
    if (line.text.contains('\n')) {
      skip(OxvSkip.rowWithNewline);
      return null;
    }
    ids.add(line.id);
  }
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

/// `semantic-import <index dir> <file.oxv | file.oxv.zst> [--books N]`:
/// [booksLimit] = only the first N books of the catalog order (an index built
/// with `build --max-books N`).
void semanticImport(SeforimDb db, String indexDir, String file, {int? booksLimit, int batch = 200}) {
  final sw = Stopwatch()..start();
  var path = file;
  if (path.endsWith('.zst')) {
    path = path.substring(0, path.length - 4);
    if (!File(path).existsSync()) {
      final bytes = expandOxv(file, path);
      stdout.writeln('expanded to $path: ${(bytes / 1e6).toStringAsFixed(1)} MB, ${_ms(sw.elapsedMilliseconds)}');
    }
  }
  final info = readOxvInfo(path);
  stdout.writeln('file: format ${info.format}, ${info.kind}, ${info.dims} dims, ${info.codec}, '
      'library ${info.libraryReleaseTag} (v${info.fromLibraryVersion}..${info.toLibraryVersion}), '
      '${info.books.length} books, ${info.slots} slots');
  final selected = booksLimit == null ? null : db.catalogBookOrder().take(booksLimit).map((b) => 'id:$b').toSet();
  final skips = {for (final s in OxvSkip.values) s: 0};
  final maps = <OxvBookMap>[];
  final mapWatch = Stopwatch()..start();
  for (final b in info.books) {
    if (selected != null && !selected.contains(b.name)) {
      skips[OxvSkip.notSelected] = skips[OxvSkip.notSelected]! + 1;
      continue;
    }
    final m = mapOxvBook(db, b, (s) => skips[s] = skips[s]! + 1);
    if (m != null) maps.add(m);
  }
  stdout.writeln('mapped ${maps.length} books in ${_ms(mapWatch.elapsedMilliseconds)}; skipped: '
      '${skips.entries.where((e) => e.value > 0).map((e) => '${e.key.name} ${e.value}').join(', ')}');
  final index = SeforimIndex.open(indexDir);
  try {
    var imported = 0, records = 0, vectors = 0, lines = 0, outOfRange = 0, duplicates = 0, unmapped = 0;
    final importWatch = Stopwatch()..start();
    for (var i = 0; i < maps.length; i += batch) {
      final r = index.importOxv(path, maps.sublist(i, i + batch > maps.length ? maps.length : i + batch));
      imported += r.booksImported;
      records += r.records;
      vectors += r.vectors;
      lines += r.lines;
      outOfRange += r.outOfRange;
      duplicates += r.duplicates;
      unmapped += r.booksUnmapped;
      if (r.booksNotInFile.isNotEmpty) stderr.writeln('not in the file: ${r.booksNotInFile.take(10).join(', ')}');
      stdout.writeln('  batch ${i ~/ batch + 1}: ${r.booksImported} books, generation ${r.generation}');
    }
    stdout.writeln('imported $imported books: $records records, $vectors vectors, $lines lines '
        '(out of range $outOfRange, duplicates $duplicates, unmapped $unmapped) in ${_ms(importWatch.elapsedMilliseconds)}');
    final vi = index.vectorInfo();
    if (vi != null) stdout.writeln('sidecar: ${vi.lines} lines, ${vi.books} books, ${vi.dims} dims');
  } finally {
    index.close();
  }
  stdout.writeln('total ${_ms(sw.elapsedMilliseconds)}');
}

/// `semantic-search <index dir> <query> [--k 10] [--hybrid]`.
void semanticSearchCommand(SeforimDb db, String indexDir, String query,
    {required String modelDir, required String ortLibrary, int k = 10, bool hybrid = false}) {
  final sw = Stopwatch()..start();
  final emb = MeivinEmbedder.open(modelDir, ortLibrary);
  final openMs = sw.elapsedMilliseconds;
  final index = SeforimIndex.open(indexDir);
  try {
    sw.reset();
    final tokens = emb.queryTokens(query);
    final v = emb.embedTokens(tokens);
    final embedMs = sw.elapsedMilliseconds;
    sw.reset();
    final sem = index.semanticSearch(v, SemanticOptions(k: k));
    stdout.writeln('model open ${_ms(openMs)}, embed ${_ms(embedMs)} (${tokens.length} tokens), '
        'search ${_ms(sw.elapsedMilliseconds)} (${sem.exact ? 'exact' : 'hnsw'}, ${sem.allowed} lines allowed)');
    for (final (i, h) in sem.hits.indexed) {
      stdout
          .writeln('${i + 1}. line ${h.lineId} book ${h.bookId} chunk ${h.chunk} score ${h.score.toStringAsFixed(4)}');
    }
    if (hybrid) {
      final request = OtzariaSearch(query: query).toOptions(contextWords: 8)..order = ResultsOrder.relevance;
      request.baseBooks.addAll(db.baseBooks());
      final session = index.search(request);
      try {
        // a streamed search (positions index) is pulled to the end first
        while (!session.pull(1 << 40)) {}
        final fused = session.fuseSemantic(query, sem);
        stdout.writeln('hybrid (${session.hitCount} lexical hits):');
        for (final (i, f) in fused.take(k).indexed) {
          stdout.writeln('${i + 1}. line ${f.lineId} book ${f.bookId} fused ${f.fused.toStringAsFixed(4)} '
              'lexical ${f.lexical?.toStringAsFixed(4) ?? '-'} semantic ${f.semantic?.toStringAsFixed(4) ?? '-'} '
              '(${f.source})');
        }
      } finally {
        session.close();
      }
    }
  } finally {
    index.close();
    emb.close();
  }
}
