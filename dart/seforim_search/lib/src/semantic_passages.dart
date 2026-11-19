// Otzaria's semantic helpers around the vector set (otzaria_search_engine 7.2.0):
// passage highlights (`semanticPassageHighlights`: the engine cuts and marks, the
// host's MeivinEmbedder embeds), the set directory's info (`semanticVectorsInfo`),
// its coverage of the library (`semanticCoverage`, fed book by book from SQLite by
// the host) and the session status (`semanticStatus`).

import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'index.dart' show SearchCancelToken;
import 'meivin_embedder.dart';
import 'native.dart';
import 'semantic.dart' show VectorBookNote, VectorSetInfo;

/// A semantic hit whose passage is to be marked (Otzaria `SemanticHighlightTarget`): the
/// line's text, the window the hit matched ([SemanticHit.chunk]; 255 = an alias record)
/// and, for a short line, its book's lines around it ([contextLines], [contextSections];
/// [contextIndex] = the line's place in them; Otzaria's recipe reads up to 2 on each side
/// in the same section). Without a context, or when a section it needs is missing from
/// [contextSections] (as Otzaria's `context_window` on a line without a section id), a short
/// line is marked on its own text.
final class PassageTarget {
  const PassageTarget(this.text, {this.window = 0, this.contextLines, this.contextSections, this.contextIndex = 0});
  final String text;
  final int window;
  final List<String>? contextLines;
  final List<int>? contextSections;
  final int contextIndex;

  Map<String, Object?> toJson() => {
    'text': text,
    'window': window,
    if (contextLines != null)
      'context': {'lines': contextLines, 'sections': contextSections ?? const <int>[], 'index': contextIndex},
  };
}

/// One line of a [PassagePlan]: the text marked and the clauses the host embeds.
final class PassagePlanLine {
  const PassagePlanLine(this.text, this.chosen, this.clauses, this.outOfBudget);

  /// The text the highlight is computed on (the line, or its embedded window).
  final String text;
  final List<int> chosen;

  /// The clauses to embed as passages, in order; empty when nothing is to be marked.
  final List<String> clauses;

  /// The line had clauses but the call's 48 ran out before it (no highlight, as Otzaria).
  final bool outOfBudget;
}

/// Step 1 of a passage highlight call ([planPassageHighlights]).
final class PassagePlan {
  const PassagePlan(this.queryText, this.needsQueryVector, this.lines);

  /// The query as it is embedded (Otzaria's `smart_query::canonical`).
  final String queryText;
  final bool needsQueryVector;
  final List<PassagePlanLine> lines;
}

/// A marked line (Otzaria `SemanticPassageHighlight`): the snippet html (escaped, the clause
/// in `<mark>`, an ellipsis where cut; empty when nothing is marked) and the clause's cosine.
final class PassageHighlight {
  const PassageHighlight(this.snippetHtml, this.isHighlighted, this.spanScore);
  final String snippetHtml;
  final bool isHighlighted;
  final double? spanScore;
}

/// The embedded window text of each semantic hit (Otzaria's `embedded_window_texts`), shown
/// in place of the line: a short line's capped context (give its [PassageTarget.contextLines]
/// with their sections), a long line's matched window past its first ([PassageTarget.window]
/// = the hit's matched window). Null where the line's own text is shown. [chunkingIdentity]:
/// the open set's recipe (null = production).
List<String?> passageWindowTexts(List<PassageTarget> targets, {int? chunkingIdentity}) {
  final n = SeforimNative.instance;
  final req = jsonEncode({
    'targets': [for (final t in targets) t.toJson()],
    if (chunkingIdentity != null) 'chunking_identity': chunkingIdentity,
  });
  final j = jsonDecode(using((al) {
    final (p, pl) = utf8Native(req, al);
    return n.takeString(n.passageWindowTexts(p, pl));
  })) as List;
  return j.cast<String?>();
}

/// Cuts each target's text into clauses and picks those to embed (at most 6 per line, 48
/// per call). [forms]: the host dictionary's highlight forms of the query words, which
/// choose among a line's clauses with the query's own words. [chunkingIdentity]: the open
/// set's recipe (null = production).
PassagePlan planPassageHighlights(
  String query,
  List<PassageTarget> targets, {
  List<String> forms = const [],
  int? chunkingIdentity,
}) {
  final n = SeforimNative.instance;
  final req = jsonEncode({
    'query': query,
    'targets': [for (final t in targets) t.toJson()],
    'forms': forms,
    if (chunkingIdentity != null) 'chunking_identity': chunkingIdentity,
  });
  final j =
      jsonDecode(
            using((al) {
              final (p, pl) = utf8Native(req, al);
              return n.takeString(n.passagePlan(p, pl));
            }),
          )
          as Map<String, dynamic>;
  return PassagePlan(j['query_text'] as String, j['needs_query_vector'] as bool, [
    for (final l in (j['lines'] as List).cast<Map<String, dynamic>>())
      PassagePlanLine(
        l['text'] as String,
        (l['chosen'] as List).cast<int>(),
        (l['clauses'] as List).cast<String>(),
        l['out_of_budget'] as bool,
      ),
  ]);
}

/// Marks each planned line's clause nearest [queryVector]: [vectors] holds one list per plan
/// line, the passage vectors of its clauses in order. [maxChars]: the snippet's byte budget.
List<PassageHighlight> markPassageHighlights(
  String query,
  PassagePlan plan,
  Float32List? queryVector,
  List<List<Float32List>> vectors, {
  int maxChars = 800,
}) {
  final n = SeforimNative.instance;
  final req = jsonEncode({
    'query': query,
    'query_vector': queryVector ?? const <double>[],
    'max_chars': maxChars,
    'lines': [
      for (var i = 0; i < plan.lines.length; i++)
        {
          'text': plan.lines[i].text,
          'chosen': plan.lines[i].chosen,
          'vectors': i < vectors.length ? vectors[i] : const [],
        },
    ],
  });
  final j =
      jsonDecode(
            using((al) {
              final (p, pl) = utf8Native(req, al);
              return n.takeString(n.passageMark(p, pl));
            }),
          )
          as List;
  return [
    for (final h in j.cast<Map<String, dynamic>>())
      PassageHighlight(h['snippet_html'] as String, h['is_highlighted'] as bool, (h['span_score'] as num?)?.toDouble()),
  ];
}

/// Otzaria's `semanticPassageHighlights` end to end, the model in the host: plan, embed the
/// query (once, only when some line needs it) with [embedder] and the chosen clauses with
/// [passageEmbedder] (Otzaria's `embed_passages`: the set's FP32 passage model when the family
/// ships one, else [embedder]), mark.
List<PassageHighlight> semanticPassageHighlights(
  String query,
  List<PassageTarget> targets,
  MeivinEmbedder embedder, {
  MeivinEmbedder? passageEmbedder,
  List<String> forms = const [],
  int? chunkingIdentity,
  int maxChars = 800,
}) {
  final plan = planPassageHighlights(query, targets, forms: forms, chunkingIdentity: chunkingIdentity);
  if (!plan.needsQueryVector) return markPassageHighlights(query, plan, null, const [], maxChars: maxChars);
  final queryVector = embedder.embedQuery(plan.queryText);
  final vectors = [for (final l in plan.lines) l.clauses.isEmpty ? <Float32List>[] : (passageEmbedder ?? embedder).embedPassages(l.clauses)];
  return markPassageHighlights(query, plan, queryVector, vectors, maxChars: maxChars);
}

/// One segment of [VectorsDirInfo] (Otzaria `SemanticSegmentInfo`).
final class VectorsDirSegment {
  const VectorsDirSegment(
    this.id,
    this.kind,
    this.fromLibraryVersion,
    this.toLibraryVersion,
    this.slots,
    this.slotsDead,
    this.foreignUnresolved,
    this.size,
  );
  final String id, kind;
  final int fromLibraryVersion, toLibraryVersion, slots, slotsDead, foreignUnresolved, size;
}

/// What a vector set's directory holds, from its small files alone (Otzaria
/// `SemanticVectorsInfo`); [present] false when nothing is installed.
final class VectorsDirInfo {
  const VectorsDirInfo(this.json);
  final Map<String, dynamic> json;
  bool get present => json['present'] as bool;
  String get identityDigest => json['identity_digest'] as String;
  String get modelFamilyId => json['model_family_id'] as String;
  int get libraryVersion => json['library_version'] as int;
  String get libraryReleaseTag => json['library_release_tag'] as String;
  int get generation => json['generation'] as int;
  List<VectorsDirSegment> get segments => [
    for (final s in (json['segments'] as List).cast<Map<String, dynamic>>())
      VectorsDirSegment(
        s['id'] as String,
        s['kind'] as String,
        s['from_library_version'] as int,
        s['to_library_version'] as int,
        s['slots'] as int,
        s['slots_dead'] as int,
        s['foreign_unresolved'] as int,
        s['size'] as int,
      ),
  ];
  int get slotsLive => json['slots_live'] as int;
  int get slotsDead => json['slots_dead'] as int;
  int get bytesOnDisk => json['bytes_on_disk'] as int;
  bool get needsCompaction => json['needs_compaction'] as bool;
  bool get recoveredFromPrevious => json['recovered_from_previous'] as bool;

  /// The set's recipe (`identity.model.chunking_identity`), 0 when absent.
  int get chunkingIdentity => json['chunking_identity'] as int;
}

/// Reads [vectorsDir]'s set info (Otzaria `semanticVectorsInfo`). Throws
/// [SeforimEngineException] (`vector install [corrupt|incompatible]`) when what is there
/// does not read.
VectorsDirInfo readVectorsDirInfo(String vectorsDir) {
  final n = SeforimNative.instance;
  return VectorsDirInfo(
    jsonDecode(
          using((al) {
            final (p, pl) = utf8Native(vectorsDir, al);
            return n.takeString(n.vectorsDirInfo(p, pl));
          }),
        )
        as Map<String, dynamic>,
  );
}

/// Otzaria's `SemanticCoverage`.
final class VectorCoverage {
  const VectorCoverage(
    this.liveKeyedLines,
    this.coveredLines,
    this.booksLive,
    this.booksCovered,
    this.vectorsLibraryVersion,
    this.ratio,
  );
  final int liveKeyedLines, coveredLines, booksLive, booksCovered, vectorsLibraryVersion;
  final double ratio;
}

/// Otzaria's `semanticCoverage`, fed by the host: [open] reads every key of the set, then
/// [addBook] takes each live book's lines in order (with their section ids, one per line:
/// Otzaria's heading-count sections; no PDF), [result]
/// the counts. [dispose] when done.
final class VectorCoverageCounter {
  VectorCoverageCounter._(this._handle, {bool owned = true}) : _owned = owned;
  Pointer<Void> _handle;
  final bool _owned;

  /// The native address, to feed books from another isolate ([borrow]).
  int get address {
    if (_handle == nullptr) throw StateError('VectorCoverageCounter is disposed');
    return _handle.address;
  }

  /// The counter at [address] (from [address] on another isolate), borrowed: [dispose] on
  /// it does nothing; the owner keeps it alive while the borrower feeds it, one isolate at
  /// a time.
  static VectorCoverageCounter borrow(int address) =>
      VectorCoverageCounter._(Pointer<Void>.fromAddress(address), owned: false);

  /// Throws [SeforimEngineException] (`vector install [missing]` when nothing is installed)
  /// or [SearchCancelledException] when [cancel] fires.
  static VectorCoverageCounter open(String vectorsDir, {SearchCancelToken? cancel}) {
    final n = SeforimNative.instance;
    final h = using((al) {
      final (p, pl) = utf8Native(vectorsDir, al);
      return n.coverageOpen(p, pl, cancel?.handle ?? nullptr);
    });
    if (h == nullptr) throw n.error();
    return VectorCoverageCounter._(h);
  }

  void addBook(List<String> lines, List<int> sections) {
    final n = SeforimNative.instance;
    if (_handle == nullptr) throw StateError('VectorCoverageCounter is disposed');
    final rc = using((al) {
      final (p, pl) = utf8Native(jsonEncode({'lines': lines, 'sections': sections}), al);
      return n.coverageAddBook(_handle, p, pl);
    });
    if (rc != 0) throw n.error();
  }

  /// One book from its raw rows (UTF-8 content as stored, seforim.db `lineIndex, id`
  /// order, NULL as empty) and its `line_note` rows (`lineIndex, noteIndex` order), as
  /// [SemanticIndex.mapVectorBookRows] takes them: the engine rebuilds the lines as Otzaria
  /// indexes them (a row holding a newline gives several lines, heading-count sections).
  void addBookRows(List<Uint8List> rows, [List<VectorBookNote> notes = const []]) {
    final n = SeforimNative.instance;
    if (_handle == nullptr) throw StateError('VectorCoverageCounter is disposed');
    final rc = using((al) {
      final (content, total, offsets) = _packRows(rows, al);
      final (j, jl) = utf8Native(jsonEncode([for (final t in notes) t.toJson()]), al);
      return n.coverageAddBookRows(_handle, rows.length, content, total, offsets, j, jl);
    });
    if (rc != 0) throw n.error();
  }

  VectorCoverage result() {
    final n = SeforimNative.instance;
    if (_handle == nullptr) throw StateError('VectorCoverageCounter is disposed');
    final j = jsonDecode(n.takeString(n.coverageResult(_handle))) as Map<String, dynamic>;
    return VectorCoverage(
      j['live_keyed_lines'] as int,
      j['covered_lines'] as int,
      j['books_live'] as int,
      j['books_covered'] as int,
      j['vectors_library_version'] as int,
      (j['ratio'] as num).toDouble(),
    );
  }

  void dispose() {
    if (_handle != nullptr && _owned) SeforimNative.instance.coverageFree(_handle);
    _handle = nullptr;
  }
}

/// A book's rows packed for the C ABI: the content, its length and the row offsets.
(Pointer<Uint8>, int, Pointer<Uint64>) _packRows(List<Uint8List> rows, Allocator al) {
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
  return (content, total, offsets);
}

/// One line of a book as Otzaria indexes it ([vectorBookLines]).
final class VectorBookLine {
  const VectorBookLine(this.row, this.sub, this.text, this.section);

  /// The row it comes from (index into the rows given) and its newline-separated piece.
  final int row, sub;
  final String text;

  /// Otzaria's heading-count section (the `<h` lines up to and including this one).
  final int section;
}

/// A book's lines as Otzaria indexes them, from its raw rows and notes (as
/// [VectorCoverageCounter.addBookRows] takes them): a row holding a newline gives several
/// lines, notes put back, heading-count sections. A short semantic hit's context
/// ([PassageTarget.contextLines] / [PassageTarget.contextSections]) comes from these.
List<VectorBookLine> vectorBookLines(List<Uint8List> rows, [List<VectorBookNote> notes = const []]) {
  final n = SeforimNative.instance;
  final j = jsonDecode(using((al) {
    final (content, total, offsets) = _packRows(rows, al);
    final (p, pl) = utf8Native(jsonEncode([for (final t in notes) t.toJson()]), al);
    return n.takeString(n.vectorsBookLines(rows.length, content, total, offsets, p, pl));
  })) as List;
  return [
    for (final l in j.cast<Map<String, dynamic>>())
      VectorBookLine(l['row'] as int, l['sub'] as int, l['text'] as String, l['section'] as int),
  ];
}

/// The host's live library for a vector set compaction (Otzaria's `live_library_version`,
/// its index's chunk keys): [open] for the set and the library version the host holds, then
/// [addBookRows] for every live book (`id:<bookId>`, raw rows + notes as
/// [VectorCoverageCounter.addBookRows]), then pass it to `compactVectorSet(live:)`.
/// [dispose] when the compaction is done.
final class VectorLiveKeys {
  VectorLiveKeys._(this._handle, {bool owned = true}) : _owned = owned;
  Pointer<Void> _handle;
  final bool _owned;

  /// The keys at [address] (`handle.address` on another isolate), borrowed: [dispose] on it
  /// does nothing; the owner keeps it alive while the borrower feeds it, one isolate at a time.
  static VectorLiveKeys borrow(int address) => VectorLiveKeys._(Pointer<Void>.fromAddress(address), owned: false);

  /// The native handle (for a compaction on another isolate).
  Pointer<Void> get handle {
    if (_handle == nullptr) throw StateError('VectorLiveKeys is disposed');
    return _handle;
  }

  /// Throws [SeforimEngineException] (`vector install [missing]` / `[incompatible]`).
  static VectorLiveKeys open(String vectorsDir, int libraryVersion) {
    final n = SeforimNative.instance;
    final h = using((al) {
      final (p, pl) = utf8Native(vectorsDir, al);
      return n.liveKeysOpen(p, pl, libraryVersion);
    });
    if (h == nullptr) throw n.error();
    return VectorLiveKeys._(h);
  }

  void addBookRows(String name, List<Uint8List> rows, [List<VectorBookNote> notes = const []]) {
    final n = SeforimNative.instance;
    final h = handle;
    final rc = using((al) {
      final (b, bl) = utf8Native(name, al);
      final (content, total, offsets) = _packRows(rows, al);
      final (j, jl) = utf8Native(jsonEncode([for (final t in notes) t.toJson()]), al);
      return n.liveKeysAddBookRows(h, b, bl, rows.length, content, total, offsets, j, jl);
    });
    if (rc != 0) throw n.error();
  }

  void dispose() {
    if (_handle != nullptr && _owned) SeforimNative.instance.liveKeysFree(_handle);
    _handle = nullptr;
  }
}

/// Otzaria's `SemanticState`.
enum SemanticState { notInBuild, notConfigured, empty, ready, needsReindex, failed }

/// Otzaria's `SemanticStatus` for a session over an installed vector set (its `OfficialIndex`
/// status): built from the open set ([vectors], null = no session) and the host's model.
final class SemanticStatus {
  const SemanticStatus({
    required this.state,
    required this.enabled,
    required this.available,
    required this.modelLoaded,
    required this.indexedBookCount,
    required this.vectorCount,
    required this.modelId,
    required this.embeddingDim,
    required this.embeddingBackend,
    required this.vectorBackend,
    required this.vectorsPersisted,
    required this.lastError,
    required this.errorKind,
    required this.vectorsLibraryVersion,
    required this.vectorSegments,
    required this.needsCompaction,
  });
  final SemanticState state;
  final bool enabled, available, modelLoaded, vectorsPersisted;
  final int indexedBookCount, vectorCount, embeddingDim, vectorSegments;
  final String modelId, vectorBackend;
  final String? embeddingBackend, lastError;

  /// Otzaria's `SemanticErrorKind` name (`notConfigured`), set exactly when [lastError] is.
  final String? errorKind;
  final int? vectorsLibraryVersion;
  final bool needsCompaction;

  /// [vectors]: the open set's info ([SemanticIndex.vectorInfo]: library version, segments,
  /// counts and the compaction flag come from the open generation, as Otzaria's
  /// `vector_set_info`); [dirInfo]: its directory's ([readVectorsDirInfo], for the model
  /// family; read here from `vectors.path` when not given); [modelLoaded]: whether the host's
  /// query model is loaded.
  static SemanticStatus of(VectorSetInfo? vectors, {required bool modelLoaded, VectorsDirInfo? dirInfo}) {
    if (vectors == null) {
      return const SemanticStatus(
        state: SemanticState.notConfigured,
        enabled: false,
        available: false,
        modelLoaded: false,
        indexedBookCount: 0,
        vectorCount: 0,
        modelId: '',
        embeddingDim: 0,
        embeddingBackend: null,
        vectorBackend: '',
        vectorsPersisted: false,
        lastError: 'semantic sidecar has not been configured',
        errorKind: 'notConfigured',
        vectorsLibraryVersion: null,
        vectorSegments: 0,
        needsCompaction: false,
      );
    }
    final dir = dirInfo ?? readVectorsDirInfo(vectors.path);
    final vectorCount = vectors.slotsLive > 0xFFFFFFFF ? 0xFFFFFFFF : vectors.slotsLive;
    final available = vectorCount > 0 && modelLoaded;
    return SemanticStatus(
      state: available ? SemanticState.ready : SemanticState.empty,
      enabled: true,
      available: available,
      modelLoaded: modelLoaded,
      indexedBookCount: vectors.books > 0xFFFFFFFF ? 0xFFFFFFFF : vectors.books,
      vectorCount: vectorCount,
      modelId: dir.present ? dir.modelFamilyId : '',
      embeddingDim: vectors.dims,
      embeddingBackend: modelLoaded ? 'onnxruntime-sentence-v1' : null,
      vectorBackend: 'otzaria-oxv',
      vectorsPersisted: true,
      lastError: null,
      errorKind: null,
      vectorsLibraryVersion: vectors.libraryVersion,
      vectorSegments: vectors.segments.length,
      needsCompaction: _compactionWanted(vectors),
    );
  }
}

/// The sidecar's default `CompactionPolicy::wants` on the open generation's info: more than 4
/// segments, deltas over 20 % of the base's bytes, or over 5 % of the slots dead.
bool _compactionWanted(VectorSetInfo v) {
  if (v.segments.length > 4) return true;
  final base = v.segments.isEmpty ? 0 : v.segments.first.size;
  final deltas = v.segments.skip(1).fold<int>(0, (a, s) => a + s.size);
  if (base > 0 && deltas > 0.20 * base) return true;
  final slots = v.slotsLive + v.slotsDead;
  return slots > 0 && v.slotsDead > 0.05 * slots;
}
