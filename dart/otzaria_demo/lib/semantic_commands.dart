// Semantic search in the host (issue #18): the engine opens Otzaria's own
// vector set read-only, scans it and resolves the hits lazily by the index's
// chunk key column; the host gives the rows a window check needs (`need_rows`)
// and embeds the query with Otzaria's Meivin model. The model and SQLite stay
// here, never in the engine.

import 'dart:io';

import 'package:seforim_search/seforim_search.dart';

import 'seforim_db.dart';

String _ms(num ms) => ms < 1000 ? '${ms.toStringAsFixed(ms < 100 ? 1 : 0)} ms' : '${(ms / 60000).toStringAsFixed(2)} min';

String _stripTags(String s) => s.replaceAll(RegExp(r'<[^>]*>'), ' ');

/// `semantic-search <index dir> --vectors <set dir | file.oxv> (--line N | --lines a,b,c | --query text) [--k N] [--books a,b] [--threads N] [--hybrid] [--repeat N]`.
void semanticSearchCommand(SeforimDb db, String indexDir, String vectors,
    {required String modelDir,
    required String ortLibrary,
    List<int> lines = const [],
    String? query,
    int k = 10,
    List<int>? books,
    int threads = 0,
    bool hybrid = false,
    int repeat = 1}) {
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
    final info = index.openVectors(vectors);
    stdout.writeln('open (nothing mapped): ${_ms(sw.elapsedMicroseconds / 1000)}; '
        'set books in the index ${info.booksMapped}, not in it ${info.booksUnmapped}');
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
          sem = index.semanticSearch(v, options, null, null, db.semanticRows);
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
          // Otzaria's hybrid session: its exact query's top window fused with the semantic one
          final h = index.hybridSession(text, v, HybridOptions(books: books));
          try {
            final page = h.page(0, k, texts: (ids) => db.rawContents(ids), rows: db.semanticRows);
            stdout.writeln('  hybrid (window ${page.window}, ${page.lexicalTotal} lexical matches):');
            for (final (i, f) in page.hits.indexed) {
              stdout.writeln('    ${i + 1}. line ${f.lineId} book ${f.bookId} fused ${f.fused.toStringAsFixed(4)} '
                  'lexical ${f.lexical?.toStringAsFixed(4) ?? '-'} semantic ${f.semantic?.toStringAsFixed(4) ?? '-'} '
                  '(${f.source})');
            }
          } finally {
            h.close();
          }
        }
      }
    } finally {
      emb.dispose();
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
