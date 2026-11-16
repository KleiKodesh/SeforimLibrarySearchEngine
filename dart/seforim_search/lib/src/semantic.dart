import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'native.dart';

/// One segment file of the open Otzaria vector set.
final class VectorSegmentInfo {
  const VectorSegmentInfo(this.file, this.kind, this.fromLibraryVersion, this.toLibraryVersion, this.slots,
      this.slotsDead, this.foreign, this.foreignUnresolved, this.size);
  final String file;

  /// 'base' or 'delta'.
  final String kind;
  final int fromLibraryVersion, toLibraryVersion, slots, slotsDead, foreign, foreignUnresolved, size;

  factory VectorSegmentInfo.fromJson(Map<String, dynamic> j) => VectorSegmentInfo(
      j['file'] as String,
      j['kind'] as String,
      j['from_library_version'] as int,
      j['to_library_version'] as int,
      j['slots'] as int,
      j['slots_dead'] as int,
      j['foreign'] as int,
      j['foreign_unresolved'] as int,
      j['size'] as int);
}

/// The open Otzaria vector set ([SemanticIndex.openVectors], read-only).
final class VectorSetInfo {
  const VectorSetInfo(
      {required this.path,
      required this.generation,
      required this.recoveredFromPrevious,
      required this.dims,
      required this.codec,
      required this.libraryVersion,
      required this.libraryReleaseTag,
      required this.segments,
      required this.slotsLive,
      required this.slotsDead,
      required this.books,
      required this.booksMapped,
      required this.booksUnmapped,
      required this.bytesOnDisk});
  final String path;
  final int generation;
  final bool recoveredFromPrevious;
  final int dims;
  final String codec;
  final int libraryVersion;
  final String libraryReleaseTag;
  final List<VectorSegmentInfo> segments;
  final int slotsLive, slotsDead;

  /// Books in the set; [booksMapped] of them mapped by the host.
  final int books, booksMapped, booksUnmapped;
  final int bytesOnDisk;

  factory VectorSetInfo.fromJson(Map<String, dynamic> j) => VectorSetInfo(
      path: j['path'] as String,
      generation: j['generation'] as int,
      recoveredFromPrevious: j['recovered_from_previous'] as bool,
      dims: j['dims'] as int,
      codec: j['codec'] as String,
      libraryVersion: j['library_version'] as int,
      libraryReleaseTag: j['library_release_tag'] as String,
      segments: (j['segments'] as List).map((e) => VectorSegmentInfo.fromJson(e as Map<String, dynamic>)).toList(),
      slotsLive: j['slots_live'] as int,
      slotsDead: j['slots_dead'] as int,
      books: j['books'] as int,
      booksMapped: j['books_mapped'] as int,
      booksUnmapped: j['books_unmapped'] as int,
      bytesOnDisk: j['bytes_on_disk'] as int);
}

/// A semantic search request (Otzaria's exact scan).
final class SemanticOptions {
  const SemanticOptions({this.k = 10, this.books, this.threads = 0, this.windowPenalty = 0.04, this.aliasPenalty = 0.04});

  /// How many lines.
  final int k;

  /// Only these books (the lexical search's filter); null = all.
  final List<int>? books;

  /// Scan threads; 0 = half the cores, at most 8.
  final int threads;

  /// Taken off a window past a line's first one (Otzaria's window penalty).
  final double windowPenalty;

  /// Taken off an occurrence found through a neighbour's vector (Otzaria's alias penalty).
  final double aliasPenalty;

  Map<String, Object?> toJson() => {
        'k': k,
        'threads': threads,
        'window_penalty': windowPenalty,
        'alias_penalty': aliasPenalty,
        if (books != null) 'books': books
      };
}

/// One semantic hit: one per line (its best occurrence), best first.
final class SemanticHit {
  const SemanticHit(this.lineId, this.bookId, this.score, {this.chunk = 0});
  final int lineId;
  final int bookId;
  final double score;

  /// 0 = the line's first window, n its n-th; may carry [borrowed].
  final int chunk;

  /// [chunk] bit: found through a neighbour's vector (Otzaria's alias).
  static const int borrowed = 1 << 31;
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

/// The host's mapping of one book of an Otzaria vector set ([SemanticIndex.setVectorMapping]).
final class OxvBookMap {
  const OxvBookMap(this.name, this.bookId, this.lineIds);
  final String name;
  final int bookId;

  /// The line id of each 0-based line ordinal (seforim.db rows in
  /// `lineIndex, id` order).
  final List<int> lineIds;

  Map<String, Object?> toJson() => {'name': name, 'book_id': bookId, 'line_ids': lineIds};
}

/// One `line_note` row of a book (non-NULL body) for
/// [SemanticIndex.mapVectorBookRows].
final class VectorBookNote {
  const VectorBookNote(this.lineIndex, this.body, {this.charPos = -1, this.hasMarker = false});

  /// The `lineIndex` of the note's line.
  final int lineIndex;

  /// `charPos`, -1 when absent.
  final int charPos;
  final bool hasMarker;
  final String body;

  Map<String, Object?> toJson() => {'line_index': lineIndex, 'char_pos': charPos, 'has_marker': hasMarker, 'body': body};
}

/// The text check of one book ([SemanticIndex.mapVectorBookRows]).
final class VectorBookVerify {
  const VectorBookVerify(this.lines, this.records, this.verified, this.moved, this.unverified);
  factory VectorBookVerify.fromJson(Map<String, dynamic> j) => VectorBookVerify(
      j['lines'] as int, j['records'] as int, j['verified'] as int, j['moved'] as int, j['unverified'] as int);
  final int lines;
  final int records;
  final int verified;
  final int moved;
  final int unverified;
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

/// A semantic search's hits and what the scan did.
final class SemanticResult {
  SemanticResult._(this.hits, this.vectors, this.records, this.unmapped, this.scanned, this.threads, this.generation,
      this.json);
  final List<SemanticHit> hits;

  /// Vectors scored; records = line occurrences of the best vectors read.
  final int vectors, records;

  /// Records the host's mapping does not cover (dropped, never guessed).
  final int unmapped;
  final int scanned, threads, generation;

  /// The engine's JSON (passed back for [FusionOptions] fusion).
  final String json;

  factory SemanticResult.fromJsonText(String text) {
    final j = jsonDecode(text) as Map<String, dynamic>;
    final hits = (j['hits'] as List)
        .map((e) => e as Map<String, dynamic>)
        .map((h) => SemanticHit(h['line_id'] as int, h['book_id'] as int, (h['score'] as num).toDouble(),
            chunk: h['chunk'] as int? ?? 0))
        .toList();
    return SemanticResult._(hits, j['vectors'] as int, j['records'] as int, j['unmapped'] as int,
        j['scanned'] as int, j['threads'] as int, j['generation'] as int, text);
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

/// A hybrid search request (Otzaria's hybrid session, issue #18).
final class HybridOptions {
  const HybridOptions(
      {this.books, this.fusion = const FusionOptions(), this.threads = 0, this.windowPenalty = 0.04, this.aliasPenalty = 0.04});

  /// Only these books (both sides); null = all.
  final List<int>? books;
  final FusionOptions fusion;

  /// Semantic scan threads; 0 = Otzaria's default.
  final int threads;
  final double windowPenalty, aliasPenalty;

  Map<String, Object?> toJson() => {
        'books': books,
        'fusion': fusion.toJson(),
        'threads': threads,
        'window_penalty': windowPenalty,
        'alias_penalty': aliasPenalty,
      };
}

/// One page of a [HybridSession].
final class HybridPage {
  const HybridPage(this.hits, this.more, this.window, this.lexicalTotal, this.exhausted);
  final List<FusedHit> hits;

  /// True when another page follows.
  final bool more;

  /// The candidate window fused last (Otzaria's).
  final int window;

  /// Lines the lexical query matches.
  final int lexicalTotal;
  final bool exhausted;
}

/// Otzaria's hybrid search session ([SemanticIndex.hybridSession]): pages of
/// fused results, its candidate window growing as Otzaria's does. On an
/// index without positions a query of several words is scored from the line
/// texts: [page] asks [texts] for them (raw row content, as stored).
final class HybridSession {
  HybridSession._(this._h);
  Pointer<Void> _h;

  HybridPage page(int offset, int limit,
      {Map<int, Uint8List> Function(List<int> lineIds)? texts, SearchCancelToken? cancel}) {
    final n = SeforimNative.instance;
    if (_h == nullptr) throw StateError('HybridSession is closed');
    while (true) {
      final j = jsonDecode(n.takeString(n.hybridPage(_h, offset, limit, cancel?.handle ?? nullptr))) as Map<String, dynamic>;
      final need = j['need_texts'] as List?;
      if (need == null) {
        return HybridPage([for (final e in j['hits'] as List) FusedHit.fromJson(e as Map<String, dynamic>)],
            j['more'] as bool, j['window'] as int, j['lexical_total'] as int, j['exhausted'] as bool);
      }
      if (texts == null) throw StateError('the hybrid session needs line texts: pass texts');
      final ids = [for (final e in need) e as int];
      final got = texts(ids);
      using((al) {
        final idp = al<Uint64>(ids.isEmpty ? 1 : ids.length);
        idp.asTypedList(ids.length).setAll(0, ids);
        final offsets = al<Uint64>(ids.length + 1);
        final off = offsets.asTypedList(ids.length + 1);
        var total = 0;
        for (var i = 0; i < ids.length; i++) {
          off[i] = total;
          total += got[ids[i]]?.length ?? 0;
        }
        off[ids.length] = total;
        final content = al<Uint8>(total == 0 ? 1 : total);
        final ct = content.asTypedList(total);
        for (var i = 0; i < ids.length; i++) {
          final t = got[ids[i]];
          if (t != null) ct.setAll(off[i], t);
        }
        if (n.hybridAddTexts(_h, idp, ids.length, content, total, offsets) != 0) throw n.error();
      });
    }
  }

  void close() {
    if (_h == nullptr) return;
    SeforimNative.instance.hybridClose(_h);
    _h = nullptr;
  }
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

/// Semantic search on an index (issue #18), like Otzaria: the index opens
/// Otzaria's own vector set read-only and runs an exact integer scan; the
/// host maps each book of the set (`id:<bookId>`) to its line ids. Hybrid:
/// [SemanticIndex.hybridSession]. Blocking; [SearchWorker] has the async
/// versions.
extension SemanticIndex on SeforimIndex {
  /// Opens Otzaria's vector set read-only: a set directory (`CURRENT`,
  /// `gen-N/set.json`, `segments/*.oxv`) or one base `.oxv`. Replaces a set
  /// opened before; the line mapping stays.
  VectorSetInfo openVectors(String path) {
    final n = SeforimNative.instance;
    final text = using((al) {
      final (p, pl) = utf8Native(path, al);
      return n.takeString(n.vectorsOpen(handle, p, pl));
    });
    return VectorSetInfo.fromJson(jsonDecode(text) as Map<String, dynamic>);
  }

  /// Forgets the vector set (the line mapping stays).
  void closeVectors() {
    final n = SeforimNative.instance;
    if (n.vectorsClose(handle) != 0) throw n.error();
  }

  /// Maps one book of the set ([name], `id:<bookId>` for a library book) to
  /// the host's lines: [lineIds][ordinal] for each 0-based line ordinal
  /// (seforim.db rows of the book in `lineIndex, id` order). Replaces that
  /// book's mapping; an empty list removes it.
  void mapVectorBook(String name, int bookId, List<int> lineIds) {
    final n = SeforimNative.instance;
    final rc = using((al) {
      final (p, pl) = utf8Native(name, al);
      final ids = al<Uint64>(lineIds.isEmpty ? 1 : lineIds.length);
      ids.asTypedList(lineIds.length).setAll(0, lineIds);
      return n.vectorsMapBook(handle, p, pl, bookId, ids, lineIds.length);
    });
    if (rc != 0) throw n.error();
  }

  /// Maps one book WITH Otzaria's text check: [rows] = the book's raw rows
  /// (UTF-8 content, seforim.db `lineIndex, id` order, NULL as empty),
  /// [rowLineIds] their line ids, [notes] its `line_note` rows (lineIndex,
  /// noteIndex order). Records whose recomputed chunk key fails are dropped
  /// and counted. [chunkingIdentity] 0 = the set's (a single `.oxv` needs the
  /// model's).
  VectorBookVerify mapVectorBookRows(String name, int bookId, List<int> rowLineIds, List<Uint8List> rows,
      [List<VectorBookNote> notes = const [], int chunkingIdentity = 0]) {
    if (rows.length != rowLineIds.length) {
      throw ArgumentError('rows and rowLineIds differ in length');
    }
    final n = SeforimNative.instance;
    final text = using((al) {
      final (p, pl) = utf8Native(name, al);
      final ids = al<Uint64>(rowLineIds.isEmpty ? 1 : rowLineIds.length);
      ids.asTypedList(rowLineIds.length).setAll(0, rowLineIds);
      final offsets = al<Uint64>(rows.length + 1);
      final off = offsets.asTypedList(rows.length + 1);
      var total = 0;
      for (var i = 0; i < rows.length; i++) {
        off[i] = total;
        total += rows[i].length;
      }
      off[rows.length] = total;
      final content = al<Uint8>(total == 0 ? 1 : total);
      final c = content.asTypedList(total);
      for (var i = 0; i < rows.length; i++) {
        c.setAll(off[i], rows[i]);
      }
      final (j, jl) = utf8Native(jsonEncode([for (final t in notes) t.toJson()]), al);
      return n.takeString(n.vectorsMapBookRows(
          handle, p, pl, bookId, ids, rowLineIds.length, content, total, offsets, j, jl, chunkingIdentity));
    });
    return VectorBookVerify.fromJson(jsonDecode(text) as Map<String, dynamic>);
  }

  /// Maps several books at once (each as [mapVectorBook]); [replace] drops
  /// every book mapped before.
  void setVectorMapping(List<OxvBookMap> books, {bool replace = false}) {
    final n = SeforimNative.instance;
    final rc = using((al) {
      final (j, jl) = utf8Native(jsonEncode([for (final b in books) b.toJson()]), al);
      return n.vectorsSetMapping(handle, j, jl, replace ? 1 : 0);
    });
    if (rc != 0) throw n.error();
  }

  /// The open vector set; null when none is open.
  VectorSetInfo? vectorInfo() {
    final n = SeforimNative.instance;
    final j = jsonDecode(n.takeString(n.vectorsInfo(handle)));
    return j == null ? null : VectorSetInfo.fromJson(j as Map<String, dynamic>);
  }

  /// The k lines closest to [query] (Otzaria's query model): an exact scan
  /// of every live vector of the set (or the filter's books).
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

  /// Opens Otzaria's hybrid search session for [query] (the lexical text)
  /// and [vector] (its embedding by the query model; empty or no open set =
  /// the lexical side alone). Close it with [HybridSession.close].
  HybridSession hybridSession(String query, Float32List vector, [HybridOptions options = const HybridOptions()]) {
    final n = SeforimNative.instance;
    final h = using((al) {
      final (q, ql) = utf8Native(query, al);
      final v = al<Float>(vector.isEmpty ? 1 : vector.length);
      v.asTypedList(vector.length).setAll(0, vector);
      final (j, jl) = utf8Native(jsonEncode(options.toJson()), al);
      return n.hybridOpen(handle, q, ql, v, vector.length, j, jl);
    });
    if (h == nullptr) throw n.error();
    return HybridSession._(h);
  }
}
