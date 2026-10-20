import 'dart:typed_data';

/// How an index is built. Fixed at creation.
class IndexOptions {
  const IndexOptions({this.positions = true, this.keepMarks = false});

  /// Store word positions. Off: a smaller index; word distance is still
  /// validated from the line text by the snippet locator.
  final bool positions;

  /// Keep nikud/te'amim in indexed terms (for [WalkOptions.ignoreNikud] /
  /// [WalkOptions.ignoreTeamim]).
  final bool keepMarks;

  Map<String, Object?> toJson() => {'positions': positions, 'keep_marks': keepMarks};
}

/// The state of an open index.
class IndexInfo {
  const IndexInfo({required this.numLines, required this.positions, required this.keepMarks, this.payload});
  final int numLines;
  final bool positions;
  final bool keepMarks;

  /// The payload committed with the last commit, or null.
  final String? payload;

  factory IndexInfo.fromJson(Map<String, dynamic> j) => IndexInfo(
        numLines: (j['num_lines'] as num).toInt(),
        positions: j['positions'] as bool,
        keepMarks: j['keep_marks'] as bool,
        payload: j['payload'] as String?,
      );
}

/// One line to index.
class LineDocument {
  const LineDocument({
    required this.lineId,
    required this.position,
    required this.bookId,
    required this.lineIndex,
    this.tocEntryId = 0,
    required this.text,
  });
  final int lineId;

  /// Rank in the library's catalog order: results are returned in it.
  final int position;
  final int bookId;
  final int lineIndex;
  final int tocEntryId;

  /// The raw line text (HTML allowed).
  final String text;
}

/// Hebrew walk options for a word (Otzaria's search options).
class WalkOptions {
  const WalkOptions({
    this.grammarPrefixes = false,
    this.grammarSuffixes = false,
    this.aramaicPrefixes = false,
    this.aramaicSuffixes = false,
    this.ktivMaleHaser = false,
    this.partOfWord = false,
    this.advancedPrefixes = false,
    this.advancedSuffixes = false,
    this.ignoreQuotes = false,
    this.spellingErrors = false,
    this.ignoreNikud = false,
    this.ignoreTeamim = false,
    this.prefixMaxLetters,
    this.suffixMaxLetters,
    this.partOfWordMaxLetters,
    this.grammarPrefixMaxLetters,
  });

  /// קידומות דקדוקיות.
  final bool grammarPrefixes;

  /// סיומות דקדוקיות.
  final bool grammarSuffixes;

  /// קידומות ארמיות.
  final bool aramaicPrefixes;

  /// סיומות ארמיות.
  final bool aramaicSuffixes;

  /// כתיב מלא/חסר.
  final bool ktivMaleHaser;

  /// חלק ממילה.
  final bool partOfWord;

  /// קידומות (מתקדם): any letters before.
  final bool advancedPrefixes;

  /// סיומות (מתקדם): any letters after.
  final bool advancedSuffixes;

  /// התעלם מגרשיים.
  final bool ignoreQuotes;

  /// שגיאות כתיב / OCR מצויות.
  final bool spellingErrors;

  /// Ignore nikud (matters on a keepMarks index).
  final bool ignoreNikud;

  /// Ignore te'amim (matters on a keepMarks index).
  final bool ignoreTeamim;

  /// Max letters before the word (advanced prefixes). Null = engine default
  /// (3, or 5 for a word of ≤3 letters).
  final int? prefixMaxLetters;

  /// Max letters after the word (advanced suffixes). Null = default (5, or 7).
  final int? suffixMaxLetters;

  /// Part of word: max letters per side. Null = default (3, or 2 for 4+ letters).
  final int? partOfWordMaxLetters;

  /// Grammar prefixes: max total letters of a stacked prefix. Null = 3.
  final int? grammarPrefixMaxLetters;

  Map<String, Object?> toJson() {
    final m = <String, Object?>{};
    void limit(String k, int? v) {
      if (v != null) m[k] = v < 0 ? 0 : v;
    }

    void flag(String k, bool v) {
      if (v) m[k] = true;
    }

    limit('prefix_max_letters', prefixMaxLetters);
    limit('suffix_max_letters', suffixMaxLetters);
    limit('part_of_word_max_letters', partOfWordMaxLetters);
    limit('grammar_prefix_max_letters', grammarPrefixMaxLetters);
    flag('grammar_prefixes', grammarPrefixes);
    flag('grammar_suffixes', grammarSuffixes);
    flag('aramaic_prefixes', aramaicPrefixes);
    flag('aramaic_suffixes', aramaicSuffixes);
    flag('ktiv_male_haser', ktivMaleHaser);
    flag('part_of_word', partOfWord);
    flag('advanced_prefixes', advancedPrefixes);
    flag('advanced_suffixes', advancedSuffixes);
    flag('ignore_quotes', ignoreQuotes);
    flag('spelling_errors', spellingErrors);
    flag('ignore_nikud', ignoreNikud);
    flag('ignore_teamim', ignoreTeamim);
    return m;
  }
}

/// One AND slot of a query.
sealed class QueryGroup {
  const QueryGroup();

  /// A word with walk options.
  const factory QueryGroup.word(String word, [WalkOptions options]) = WordGroup;

  /// A wildcard pattern (`*`, `?`). A rejected pattern skips its slot.
  const factory QueryGroup.wildcard(String pattern) = WildcardGroup;

  /// Levenshtein neighbours of [word], distance 1..2, closest first.
  const factory QueryGroup.fuzzy(String word, int distance) = FuzzyGroup;

  /// A slot any of the alternatives satisfies (a | b).
  const factory QueryGroup.any(List<QueryGroup> alternatives) = AnyGroup;

  Map<String, Object?> toJson();
}

class WordGroup extends QueryGroup {
  const WordGroup(this.word, [this.options = const WalkOptions()]);
  final String word;
  final WalkOptions options;
  @override
  Map<String, Object?> toJson() => {'kind': 'word', 'word': word, 'options': options.toJson()};
}

class WildcardGroup extends QueryGroup {
  const WildcardGroup(this.pattern);
  final String pattern;
  @override
  Map<String, Object?> toJson() => {'kind': 'wildcard', 'pattern': pattern};
}

class FuzzyGroup extends QueryGroup {
  const FuzzyGroup(this.word, this.distance);
  final String word;
  final int distance;
  @override
  Map<String, Object?> toJson() => {'kind': 'fuzzy', 'word': word, 'distance': distance.clamp(1, 2)};
}

class AnyGroup extends QueryGroup {
  const AnyGroup(this.alternatives);
  final List<QueryGroup> alternatives;
  @override
  Map<String, Object?> toJson() => {'kind': 'any', 'alternatives': [for (final a in alternatives) a.toJson()]};
}

/// A search.
class SearchRequest {
  SearchRequest(
      {List<QueryGroup>? groups, this.maxWordDistance, this.requireOrdered = false, this.contextWords = 8, this.gaps})
      : groups = groups ?? [];

  /// Per-pair spacing (Otzaria's customSpacing): gaps[i] = max words between
  /// group i and group i+1 (0 = adjacent). Needs an index with positions; one
  /// entry per consecutive pair. Null = not used.
  List<int>? gaps;

  /// AND across groups, OR within each group's expansion.
  final List<QueryGroup> groups;

  /// Drop hits with more words than this between matched slots. Null = none.
  int? maxWordDistance;

  /// Only windows where the groups appear in query order.
  bool requireOrdered;

  /// Words of context per side of the match.
  int contextWords;

  Map<String, Object?> toJson() => {
        'groups': [for (final g in groups) g.toJson()],
        'max_word_distance': maxWordDistance,
        'require_ordered': requireOrdered,
        'context_words': contextWords,
        if (gaps != null) 'gaps': [for (final g in gaps!) g < 0 ? 0 : g],
      };
}

/// A matching line, in catalog order.
class PlacedHit {
  const PlacedHit(this.position, this.lineId, this.bookId, this.lineIndex, this.tocEntryId);
  final int position;
  final int lineId;
  final int bookId;
  final int lineIndex;
  final int tocEntryId;
  @override
  String toString() => 'PlacedHit(pos $position, line $lineId, book $bookId:$lineIndex)';
}

/// One highlighted word inside a snippet: UTF-16 offsets (Dart string indices).
class SnippetMark {
  const SnippetMark(this.start, this.length, this.termId);
  final int start;
  final int length;

  /// Index into the session's term table (see [SearchSession.takeNewTerms]).
  final int termId;
}

/// A located snippet. Offsets are UTF-16 units into the located text, so
/// they index Dart strings directly.
class Snippet {
  Snippet({
    required this.score,
    required this.wordDistance,
    required this.windowWordCount,
    required this.start,
    required this.end,
    required this.flags,
    required this.marks,
  });

  static const int flagLeadingEllipsis = 1;
  static const int flagTrailingEllipsis = 2;

  /// Raw span of the tightest window (smaller = closer).
  final int score;

  /// Words between the matched slots in the tightest window.
  final int wordDistance;

  /// Words the window spans after context expansion.
  final int windowWordCount;
  final int start;
  final int end;
  final int flags;
  final List<SnippetMark> marks;

  bool get leadingEllipsis => flags & flagLeadingEllipsis != 0;
  bool get trailingEllipsis => flags & flagTrailingEllipsis != 0;

  /// Fewer words than the per-side context: widen over neighbour lines.
  bool isShort(int contextWords) => windowWordCount < contextWords;

  static const int _maxInt32 = 0x7fffffff;

  /// Reads one record of the native flat format at [at]; returns it and the next index.
  static (Snippet, int) read(Uint32List d, int at) {
    final markCount = d[at + 6];
    final marks = <SnippetMark>[
      for (var m = 0; m < markCount; m++)
        SnippetMark(d[at + 7 + m * 3], d[at + 8 + m * 3], d[at + 9 + m * 3]),
    ];
    final s = Snippet(
      score: d[at] > _maxInt32 ? _maxInt32 : d[at],
      wordDistance: d[at + 1] > _maxInt32 ? _maxInt32 : d[at + 1],
      windowWordCount: d[at + 2],
      start: d[at + 3],
      end: d[at + 4],
      flags: d[at + 5],
      marks: marks,
    );
    return (s, at + 7 + markCount * 3);
  }
}
