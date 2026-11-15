import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'native.dart';

/// One vector of a line from the host's embedding model (the engine runs no
/// model). A long line may have several (chunks); the line scores as its
/// best chunk.
final class LineVector {
  const LineVector(this.lineId, this.bookId, this.vector, {this.chunk = 0});
  final int lineId;
  final int bookId;
  final Float32List vector;

  /// 0 = the line's first (or only) vector; 1, 2, ... further chunks; may
  /// carry [borrowed].
  final int chunk;

  /// [chunk] bit: a vector borrowed from a neighbour (Otzaria's alias).
  static const int borrowed = 1 << 31;
}

/// One vector-sidecar write (issue #18): removals (whole books, single
/// lines), then additions. A line added again replaces its vector; an
/// updated book = [removeBooks] + its lines.
final class VectorUpdate {
  VectorUpdate(this.dims, {this.removeBooks = const [], this.removeLines = const [], this.add = const []});
  final int dims;
  final List<int> removeBooks;
  final List<int> removeLines;
  final List<LineVector> add;
}

/// What a vector write did.
final class VectorUpdateStats {
  const VectorUpdateStats(this.generation, this.lines, this.books, this.removed, this.added, this.rebuilt,
      {this.vectors = 0});
  final int generation, lines, books, removed, added;

  /// Vectors (chunks) in the store.
  final int vectors;

  /// True: the HNSW graph was built afresh (first build, or many removals).
  final bool rebuilt;

  factory VectorUpdateStats.fromJson(Map<String, dynamic> j) => VectorUpdateStats(j['generation'] as int,
      j['lines'] as int, j['books'] as int, j['removed'] as int, j['added'] as int, j['rebuilt'] as bool,
      vectors: j['vectors'] as int);
}

/// The vector sidecar's size.
final class VectorInfo {
  const VectorInfo(this.dims, this.lines, this.books, this.generation, {this.vectors = 0});
  final int dims, lines, books, generation;

  /// Vectors (a long line may have several chunks).
  final int vectors;
}

/// A semantic search request.
final class SemanticOptions {
  const SemanticOptions(
      {this.k = 10, this.books, this.oversample = 3.0, this.exactThreshold = 20000, this.chunkPenalty = 0.04});

  /// How many lines (a ranked cut list).
  final int k;

  /// Only these books (the lexical search's filter); null = all.
  final List<int>? books;

  /// HNSW candidates = k * oversample, rescored with the full vectors.
  final double oversample;

  /// A filter leaving at most this many vectors is an exact scan.
  final int exactThreshold;

  /// Taken off a chunk past a line's first one before its best chunk is
  /// chosen (Otzaria's window penalty).
  final double chunkPenalty;

  Map<String, Object?> toJson() => {
        'k': k,
        'oversample': oversample,
        'exact_threshold': exactThreshold,
        'chunk_penalty': chunkPenalty,
        if (books != null) 'books': books
      };
}

/// One semantic hit, a line: its best chunk's cosine minus that chunk's
/// penalty, best first, ties by line id.
final class SemanticHit {
  const SemanticHit(this.lineId, this.bookId, this.score, {this.chunk = 0});
  final int lineId;
  final int bookId;
  final double score;

  /// The chunk that scored the line (0 = its first vector).
  final int chunk;
}

/// One book of an Otzaria vector file.
final class OxvBookInfo {
  const OxvBookInfo(this.name, this.records, this.lines);

  /// Otzaria's index file path: `id:<bookId>` for a library book.
  final String name;

  /// Records (line occurrences of a vector, chunks included).
  final int records;

  /// Highest line ordinal + 1: the line count a mapping must cover.
  final int lines;
}

/// What an Otzaria vector file (`.oxv`, base segment) holds.
final class OxvInfo {
  const OxvInfo(this.format, this.kind, this.dims, this.codec, this.libraryReleaseTag, this.fromLibraryVersion,
      this.toLibraryVersion, this.slots, this.books);
  final int format;
  final String kind;
  final int dims;
  final String codec;
  final String libraryReleaseTag;
  final int fromLibraryVersion, toLibraryVersion, slots;
  final List<OxvBookInfo> books;

  factory OxvInfo.fromJson(Map<String, dynamic> j) => OxvInfo(
      j['format'] as int,
      j['kind'] as String,
      j['dims'] as int,
      j['codec'] as String,
      j['library_release_tag'] as String,
      j['from_library_version'] as int,
      j['to_library_version'] as int,
      j['slots'] as int,
      (j['books'] as List)
          .map((e) => e as Map<String, dynamic>)
          .map((b) => OxvBookInfo(b['name'] as String, b['records'] as int, b['lines'] as int))
          .toList());
}

/// The host's mapping of one book of an Otzaria vector file.
final class OxvBookMap {
  const OxvBookMap(this.name, this.bookId, this.lineIds);
  final String name;
  final int bookId;

  /// The line id of each 0-based line ordinal (seforim.db rows in
  /// `lineIndex, id` order).
  final List<int> lineIds;

  Map<String, Object?> toJson() => {'name': name, 'book_id': bookId, 'line_ids': lineIds};
}

/// What [SemanticIndex.importOxv] did.
final class OxvImportReport {
  const OxvImportReport(this.booksImported, this.booksNotInFile, this.booksUnmapped, this.records, this.vectors,
      this.lines, this.outOfRange, this.duplicates, this.generation);
  final int booksImported;
  final List<String> booksNotInFile;
  final int booksUnmapped, records, vectors, lines;

  /// Records past the host's line list (dropped).
  final int outOfRange;
  final int duplicates, generation;

  factory OxvImportReport.fromJson(Map<String, dynamic> j) => OxvImportReport(
      j['books_imported'] as int,
      (j['books_not_in_file'] as List).cast<String>(),
      j['books_unmapped'] as int,
      j['records'] as int,
      j['vectors'] as int,
      j['lines'] as int,
      j['out_of_range'] as int,
      j['duplicates'] as int,
      j['generation'] as int);
}

/// Expands Otzaria's downloaded vector file (`.oxv.zst`) to a plain `.oxv`;
/// returns the bytes written.
int expandOxv(String source, String destination) {
  final n = SeforimNative.instance;
  return using((al) {
    final (s, sl) = utf8Native(source, al);
    final (d, dl) = utf8Native(destination, al);
    final written = n.oxvExpand(s, sl, d, dl);
    if (written < 0) throw n.error();
    return written;
  });
}

/// What an Otzaria vector file holds: its books (named as Otzaria's index
/// files them, `id:<bookId>` for a library book) and their line counts.
OxvInfo readOxvInfo(String path) {
  final n = SeforimNative.instance;
  final text = using((al) {
    final (p, pl) = utf8Native(path, al);
    return n.takeString(n.oxvInfo(p, pl));
  });
  return OxvInfo.fromJson(jsonDecode(text) as Map<String, dynamic>);
}

/// A semantic search's hits and how it was served.
final class SemanticResult {
  SemanticResult._(this.hits, this.exact, this.allowed, this.generation, this.json);
  final List<SemanticHit> hits;

  /// True: exact scan of the filtered lines; false: HNSW + rescore.
  final bool exact;
  final int allowed;
  final int generation;

  /// The engine's JSON (passed back for [FusionOptions] fusion).
  final String json;

  factory SemanticResult.fromJsonText(String text) {
    final j = jsonDecode(text) as Map<String, dynamic>;
    final hits = (j['hits'] as List)
        .map((e) => e as Map<String, dynamic>)
        .map((h) => SemanticHit(h['line_id'] as int, h['book_id'] as int, (h['score'] as num).toDouble(),
            chunk: h['chunk'] as int? ?? 0))
        .toList();
    return SemanticResult._(hits, j['exact'] as bool, j['allowed'] as int, j['generation'] as int, text);
  }
}

/// Hybrid fusion strategy.
enum FusionStrategy { weighted, rrf }

/// How to fuse lexical and semantic results (Otzaria's fusion).
final class FusionOptions {
  const FusionOptions({this.strategy = FusionStrategy.weighted, this.alpha, this.bm25K = 10.0, this.rrfK = 60});
  final FusionStrategy strategy;

  /// Weighted: lexical weight 0..1; null = Otzaria's table by query type.
  final double? alpha;

  /// Weighted: BM25 saturation k (x / (k + x)).
  final double bm25K;

  /// RRF: k (1 / (k + rank)).
  final int rrfK;

  Map<String, Object?> toJson() => strategy == FusionStrategy.rrf
      ? {'strategy': 'rrf', 'k': rrfK}
      : {'strategy': 'weighted', 'bm25_k': bm25K, if (alpha != null) 'alpha': alpha};
}

/// One hybrid result.
final class FusedHit {
  const FusedHit(this.lineId, this.bookId, this.position, this.fused, this.lexical, this.semantic, this.source);
  final int lineId;
  final int bookId;

  /// The lexical hit's catalog position; null for a semantic-only line.
  final int? position;
  final double fused;
  final double? lexical;
  final double? semantic;

  /// 'lexical', 'semantic' or 'both'.
  final String source;

  factory FusedHit.fromJson(Map<String, dynamic> j) => FusedHit(
      j['line_id'] as int,
      j['book_id'] as int,
      j['position'] as int?,
      (j['fused'] as num).toDouble(),
      (j['lexical'] as num?)?.toDouble(),
      (j['semantic'] as num?)?.toDouble(),
      j['source'] as String);
}

/// Semantic search on an index (issue #18): the vector sidecar and the
/// semantic-only search. Hybrid: [SearchSession.fuseSemantic]. Blocking;
/// [SearchWorker] has the async versions.
extension SemanticIndex on SeforimIndex {
  /// Applies a vector write and publishes a new generation (background
  /// priority). [cancel] (or a raw token address) stops it before publishing.
  VectorUpdateStats updateVectors(VectorUpdate update, {SearchCancelToken? cancel, int? cancelAddress}) {
    final n = SeforimNative.instance;
    final dims = update.dims;
    final count = update.add.length;
    final text = using((al) {
      Pointer<Uint64> u64s(List<int> v) {
        final p = al<Uint64>(v.isEmpty ? 1 : v.length);
        for (var i = 0; i < v.length; i++) {
          p[i] = v[i];
        }
        return p;
      }

      final lines = al<Uint64>(count == 0 ? 1 : count);
      final books = al<Uint64>(count == 0 ? 1 : count);
      final chunks = al<Uint32>(count == 0 ? 1 : count);
      final floats = al<Float>(count * dims == 0 ? 1 : count * dims);
      final view = floats.asTypedList(count * dims);
      for (var i = 0; i < count; i++) {
        final v = update.add[i];
        if (v.vector.length != dims) {
          throw ArgumentError('line ${v.lineId} has ${v.vector.length} dims, the update $dims');
        }
        lines[i] = v.lineId;
        books[i] = v.bookId;
        chunks[i] = v.chunk;
        view.setRange(i * dims, (i + 1) * dims, v.vector);
      }
      final token = cancel?.handle ?? (cancelAddress == null ? nullptr : Pointer<Void>.fromAddress(cancelAddress));
      return n.takeString(n.vectorsUpdate(handle, dims, u64s(update.removeBooks), update.removeBooks.length,
          u64s(update.removeLines), update.removeLines.length, lines, books, chunks, floats, count, token));
    });
    return VectorUpdateStats.fromJson(jsonDecode(text) as Map<String, dynamic>);
  }

  /// The vector sidecar's size; null when the index has no vectors.
  VectorInfo? vectorInfo() {
    final n = SeforimNative.instance;
    final j = jsonDecode(n.takeString(n.vectorsInfo(handle)));
    if (j == null) return null;
    final m = j as Map<String, dynamic>;
    return VectorInfo(m['dims'] as int, m['lines'] as int, m['books'] as int, m['generation'] as int,
        vectors: m['vectors'] as int);
  }

  /// Imports an Otzaria vector file into the vector sidecar by the host's
  /// line ids ([books]: per book of the file, its book id and the line id
  /// of each 0-based line ordinal). Every chunk of a mapped line is kept;
  /// each mapped book replaces its old vectors; one store write per call,
  /// so pass books in batches.
  OxvImportReport importOxv(String path, List<OxvBookMap> books, {SearchCancelToken? cancel, int? cancelAddress}) {
    final n = SeforimNative.instance;
    final text = using((al) {
      final (p, pl) = utf8Native(path, al);
      final (j, jl) = utf8Native(jsonEncode([for (final b in books) b.toJson()]), al);
      final token = cancel?.handle ?? (cancelAddress == null ? nullptr : Pointer<Void>.fromAddress(cancelAddress));
      return n.takeString(n.vectorsImportOxv(handle, p, pl, j, jl, token));
    });
    return OxvImportReport.fromJson(jsonDecode(text) as Map<String, dynamic>);
  }

  /// The k lines closest (cosine) to [query] (same model as the stored vectors).
  SemanticResult semanticSearch(Float32List query,
      [SemanticOptions options = const SemanticOptions(), SearchCancelToken? cancel, int? cancelAddress]) {
    final n = SeforimNative.instance;
    final text = using((al) {
      final q = al<Float>(query.isEmpty ? 1 : query.length);
      q.asTypedList(query.length).setAll(0, query);
      final (j, jl) = utf8Native(jsonEncode(options.toJson()), al);
      final token = cancel?.handle ?? (cancelAddress == null ? nullptr : Pointer<Void>.fromAddress(cancelAddress));
      return n.takeString(n.semanticSearch(handle, q, query.length, j, jl, token));
    });
    return SemanticResult.fromJsonText(text);
  }
}
