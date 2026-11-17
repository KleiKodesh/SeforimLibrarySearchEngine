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

  /// The engine's JSON.
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

/// Otzaria's `SemanticRetrievalMode` (and the executed mode).
enum HybridRetrieval {
  hybrid('hybrid'),
  semanticOnly('semantic_only'),
  lexicalOnly('lexical_only');

  const HybridRetrieval(this.json);
  final String json;

  static HybridRetrieval fromJson(String s) => values.firstWhere((m) => m.json == s);
}

/// Otzaria's `SemanticLexicalMode`.
enum HybridLexicalMode { exact, fuzzy }

/// A hybrid search request (Otzaria's hybrid session, issue #18).
final class HybridOptions {
  const HybridOptions(
      {this.books,
      this.foundationalBooks,
      this.ranking = const {},
      this.threads = 0,
      this.retrieval = HybridRetrieval.hybrid,
      this.lexicalMode = HybridLexicalMode.exact,
      this.fuzzyMaxDistance = 1,
      this.matchNikud = false,
      this.matchTeamim = false});

  /// Otzaria's retrieval mode: semantic only discards the lexical candidates
  /// (counted once), lexical only runs no vector scan.
  final HybridRetrieval retrieval;

  /// Otzaria's lexical mode (a query that is one quotation is exact).
  final HybridLexicalMode lexicalMode;

  /// The fuzzy mode's edit distance (1 or 2).
  final int fuzzyMaxDistance;

  /// Otzaria's match_nikud / match_taamim on every lexical candidate and
  /// count (needs a keep_marks index).
  final bool matchNikud;
  final bool matchTeamim;

  /// Only these books (both sides); null = all.
  final List<int>? books;

  /// The foundational books (Otzaria's `/base` facet: the host's base books),
  /// for the profile's foundational bonus and foundational query.
  final List<int>? foundationalBooks;

  /// Otzaria's ranking profile fields to set (`RankingProfile`, snake_case as
  /// the C ABI documents; empty = otzaria_search_engine's defaults: RRF k 60,
  /// threshold 0.55, window multiplier 2, foundational bonus 0.002 / share 0.5).
  final Map<String, Object?> ranking;

  /// Semantic scan threads; 0 = Otzaria's default.
  final int threads;

  /// Otzaria's `SearchProfile.balanced` (weighted fusion).
  static const Map<String, Object?> balanced = {
    'fusion_strategy': {'strategy': 'weighted'},
    'semantic_threshold': 0.0,
    'agreement_bonus': 0.10,
    'candidate_window_multiplier': 2.0,
    'foundational_bonus': 0.0,
    'foundational_candidate_share': 0.0,
  };

  Map<String, Object?> toJson() => {
        'books': books,
        'foundational_books': foundationalBooks,
        'ranking': ranking,
        'threads': threads,
        'retrieval': retrieval.json,
        'lexical_mode': lexicalMode.name,
        'fuzzy_max_distance': fuzzyMaxDistance,
        'match_nikud': matchNikud,
        'match_teamim': matchTeamim,
      };
}

/// How a hybrid session served its results (Otzaria's response fields).
final class HybridMeta {
  const HybridMeta(
      this.lexicalTotal,
      this.lexicalTruncated,
      this.totalCount,
      this.countsAreExact,
      this.requestedMode,
      this.executedMode,
      this.semanticAvailable,
      this.fallbackReason,
      this.fallbackKind,
      this.candidateWindowTruncated,
      this.staleDropped);

  /// Lines the lexical query matches.
  final int lexicalTotal;

  /// The lexical side hit an expansion cap.
  final bool lexicalTruncated;

  /// Candidates the last window fused (the fallback: the lexical total).
  final int totalCount;
  final bool countsAreExact;
  final HybridRetrieval requestedMode;

  /// The mode that ran (a hybrid search whose semantic side failed or was
  /// skipped ran lexical only).
  final HybridRetrieval executedMode;
  final bool semanticAvailable;

  /// The semantic side's failure and the session's notes, joined by "; ".
  final String? fallbackReason;

  /// 'not_configured' (no vector set: the lexical fallback), 'query_failed', or null.
  final String? fallbackKind;
  final bool candidateWindowTruncated;

  /// Semantic results dropped: their line is not in the index.
  final int staleDropped;

  factory HybridMeta.fromJson(Map<String, dynamic> j) => HybridMeta(
      j['lexical_total'] as int,
      j['lexical_truncated'] as bool,
      j['total_count'] as int,
      j['counts_are_exact'] as bool,
      HybridRetrieval.fromJson(j['requested_mode'] as String),
      HybridRetrieval.fromJson(j['executed_mode'] as String),
      j['semantic_available'] as bool,
      j['fallback_reason'] as String?,
      j['fallback_kind'] as String?,
      j['candidate_window_truncated'] as bool,
      j['stale_dropped'] as int);
}

/// One fused window ([HybridSession.window]): every result of the window in
/// fused order, for a host that groups results itself.
final class HybridWindow {
  const HybridWindow(this.hits, this.window, this.lexicalDone, this.semanticDone, this.fusedCount, this.meta);
  final List<FusedHit> hits;
  final int window;
  final bool lexicalDone;
  final bool semanticDone;

  /// Candidates the fusion produced before the cut to [window].
  final int fusedCount;
  final HybridMeta meta;
}

/// One page of a [HybridSession].
final class HybridPage {
  const HybridPage(this.hits, this.more, this.window, this.lexicalTotal, this.exhausted, this.meta);
  final List<FusedHit> hits;

  /// True when another page follows.
  final bool more;

  /// The candidate window fused last (Otzaria's).
  final int window;

  /// Lines the lexical query matches.
  final int lexicalTotal;
  final bool exhausted;

  /// Otzaria's response fields (modes, fallback, notes).
  final HybridMeta meta;
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
    final j = _step(() => n.hybridPage(_h, offset, limit, cancel?.handle ?? nullptr), texts);
    return HybridPage([for (final e in j['hits'] as List) FusedHit.fromJson(e as Map<String, dynamic>)], j['more'] as bool,
        j['window'] as int, j['lexical_total'] as int, j['exhausted'] as bool, HybridMeta.fromJson(j));
  }

  /// One fused window of [window] candidates (at most 10,000), apart from the
  /// pages: for a host that groups results itself (Otzaria's grouping counts
  /// its window in groups; the host grows the window and admits groups as
  /// Otzaria's session does). Stale lines are left out.
  HybridWindow window(int window, {Map<int, Uint8List> Function(List<int> lineIds)? texts, SearchCancelToken? cancel}) {
    final n = SeforimNative.instance;
    final j = _step(() => n.hybridWindow(_h, window, cancel?.handle ?? nullptr), texts);
    return HybridWindow([for (final e in j['hits'] as List) FusedHit.fromJson(e as Map<String, dynamic>)], j['window'] as int,
        j['lexical_done'] as bool, j['semantic_done'] as bool, j['fused_count'] as int, HybridMeta.fromJson(j));
  }

  /// Calls [call] until it answers with something other than `need_texts`,
  /// giving the session the texts it asks for.
  Map<String, dynamic> _step(Pointer<Utf8> Function() call, Map<int, Uint8List> Function(List<int> lineIds)? texts) {
    final n = SeforimNative.instance;
    if (_h == nullptr) throw StateError('HybridSession is closed');
    while (true) {
      final j = jsonDecode(n.takeString(call())) as Map<String, dynamic>;
      final need = j['need_texts'] as List?;
      if (need == null) return j;
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
  const FusedHit(this.lineId, this.bookId, this.position, this.fused, this.lexical, this.semantic, this.source,
      [this.matchedWindow = 0, this.sectionId = 0]);
  final int lineId;
  final int bookId;

  /// The lexical hit's catalog position; null for a semantic-only line.
  final int? position;
  final double fused;
  final double? lexical;
  final double? semantic;

  /// 'lexical', 'semantic' or 'both'.
  final String source;

  /// The semantic match's window (Otzaria's `matched_window`: 0 = the line's
  /// first, n its n-th; bit 31 = through a neighbour's vector), for
  /// passage highlights; 0 when the semantic side did not find the line.
  final int matchedWindow;

  /// The line's section (the index's `section_id`).
  final int sectionId;

  factory FusedHit.fromJson(Map<String, dynamic> j) => FusedHit(
      j['line_id'] as int,
      j['book_id'] as int,
      j['position'] as int?,
      (j['fused'] as num).toDouble(),
      (j['lexical'] as num?)?.toDouble(),
      (j['semantic'] as num?)?.toDouble(),
      j['source'] as String,
      j['matched_window'] as int? ?? 0,
      j['section_id'] as int? ?? 0);
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
