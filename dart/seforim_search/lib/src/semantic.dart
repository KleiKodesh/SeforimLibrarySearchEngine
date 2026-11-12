import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'native.dart';

/// One line's vector from the host's embedding model (the engine runs no model).
final class LineVector {
  const LineVector(this.lineId, this.bookId, this.vector);
  final int lineId;
  final int bookId;
  final Float32List vector;
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
  const VectorUpdateStats(this.generation, this.lines, this.books, this.removed, this.added, this.rebuilt);
  final int generation, lines, books, removed, added;

  /// True: the HNSW graph was built afresh (first build, or many removals).
  final bool rebuilt;

  factory VectorUpdateStats.fromJson(Map<String, dynamic> j) => VectorUpdateStats(j['generation'] as int,
      j['lines'] as int, j['books'] as int, j['removed'] as int, j['added'] as int, j['rebuilt'] as bool);
}

/// The vector sidecar's size.
final class VectorInfo {
  const VectorInfo(this.dims, this.lines, this.books, this.generation);
  final int dims, lines, books, generation;
}

/// A semantic search request.
final class SemanticOptions {
  const SemanticOptions({this.k = 10, this.books, this.oversample = 3.0, this.exactThreshold = 20000});

  /// How many lines (a ranked cut list).
  final int k;

  /// Only these books (the lexical search's filter); null = all.
  final List<int>? books;

  /// HNSW candidates = k * oversample, rescored with the full vectors.
  final double oversample;

  /// A filter leaving at most this many lines is an exact scan.
  final int exactThreshold;

  Map<String, Object?> toJson() =>
      {'k': k, 'oversample': oversample, 'exact_threshold': exactThreshold, if (books != null) 'books': books};
}

/// One semantic hit: cosine similarity, best first, ties by line id.
final class SemanticHit {
  const SemanticHit(this.lineId, this.bookId, this.score);
  final int lineId;
  final int bookId;
  final double score;
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
        .map((h) => SemanticHit(h['line_id'] as int, h['book_id'] as int, (h['score'] as num).toDouble()))
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
      final floats = al<Float>(count * dims == 0 ? 1 : count * dims);
      final view = floats.asTypedList(count * dims);
      for (var i = 0; i < count; i++) {
        final v = update.add[i];
        if (v.vector.length != dims) {
          throw ArgumentError('line ${v.lineId} has ${v.vector.length} dims, the update $dims');
        }
        lines[i] = v.lineId;
        books[i] = v.bookId;
        view.setRange(i * dims, (i + 1) * dims, v.vector);
      }
      final token = cancel?.handle ?? (cancelAddress == null ? nullptr : Pointer<Void>.fromAddress(cancelAddress));
      return n.takeString(n.vectorsUpdate(handle, dims, u64s(update.removeBooks), update.removeBooks.length,
          u64s(update.removeLines), update.removeLines.length, lines, books, floats, count, token));
    });
    return VectorUpdateStats.fromJson(jsonDecode(text) as Map<String, dynamic>);
  }

  /// The vector sidecar's size; null when the index has no vectors.
  VectorInfo? vectorInfo() {
    final n = SeforimNative.instance;
    final j = jsonDecode(n.takeString(n.vectorsInfo(handle)));
    if (j == null) return null;
    final m = j as Map<String, dynamic>;
    return VectorInfo(m['dims'] as int, m['lines'] as int, m['books'] as int, m['generation'] as int);
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
