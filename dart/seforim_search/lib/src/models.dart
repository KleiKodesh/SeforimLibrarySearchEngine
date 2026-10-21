import 'dart:typed_data';

/// How an index is built. Fixed at creation.
class IndexOptions {
  const IndexOptions({this.positions = true, this.keepMarks = false});

  /// Store word positions. Off: a smaller index; word distance is still
  /// validated from the line text by the snippet locator.
  final bool positions;

  /// Keep nikud/te'amim in indexed terms (canonical order) and flag the lines
  /// that carry a mark. Searches ignore the marks unless
  /// [WalkOptions.matchNikud] / [WalkOptions.matchTeamim] ask for them, so
  /// default results equal the mark-stripped index's.
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

/// One line to index. Bare minimum: the index works in tandem with
/// seforim.db, so it keeps only the line id, catalog position and book id
/// (for filtered search); line index, sections etc. come from SQLite by id.
class LineDocument {
  const LineDocument({
    required this.lineId,
    required this.position,
    required this.bookId,
    required this.text,
  });
  final int lineId;

  /// Rank in the library's catalog order: results are returned in it by default.
  final int position;
  final int bookId;

  /// The raw line text (HTML allowed).
  final String text;
}

/// How the typo option (שגיאות כתיב) expands a word.
enum TypoMode {
  /// Exactly Otzaria: edit distance 1 (its Levenshtein-1 scan) for a lone
  /// word of 4+ letters with typo as its only option; otherwise Otzaria's
  /// sampled list (confusions, transpositions, deletions, insertions of 13
  /// letters; 3 letters: confusions only).
  otzaria,

  /// Edit distance 1 for every 4+-letter word in every combination
  /// (confusions only for 3 letters, nothing for 2 or fewer).
  fullEdit1,
}

/// The order of the hits (Otzaria's ResultsOrder).
enum ResultsOrder {
  /// Catalog order, as FtsLib (the default; costs nothing).
  catalog,

  /// Otzaria's relevance: best score first, ties in catalog order. Phrase,
  /// proximity and multi-word fuzzy queries need an index with positions;
  /// without, they come back in catalog order with the notice
  /// [SearchInfo.relevanceNeedsPositions] ([SearchSession.info]).
  relevance,

  /// Otzaria's generation order: by each book's generation order
  /// ([SearchOptions.bookGenerations]), then catalog order.
  generation,
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
    this.exactQuotes = false,
    this.spellingErrors = false,
    this.typoMode = TypoMode.otzaria,
    this.matchNikud = false,
    this.matchTeamim = false,
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

  /// Index terms must match as written. Off (default, Otzaria): a term also
  /// matches by its quote-free form, so רמבם finds רמב"ם.
  final bool exactQuotes;

  /// שגיאות כתיב, expanded per [typoMode].
  final bool spellingErrors;

  /// Which typo behaviour [spellingErrors] uses. Default [TypoMode.otzaria].
  final TypoMode typoMode;

  /// Otzaria's match_nikud: typed nikud required on its letter, untyped marks
  /// free, only lines that carry a mark (needs a keepMarks index).
  final bool matchNikud;

  /// Otzaria's match_taamim: the same for te'amim.
  final bool matchTeamim;

  /// Max letters before the word (advanced prefixes). Null = engine default
  /// (3, or 5 for a word of ≤3 letters).
  final int? prefixMaxLetters;

  /// Max letters after the word (advanced suffixes). Null = default (5, or 7).
  final int? suffixMaxLetters;

  /// Part of word: max letters per side. Null = default (3, or 2 for 4+ letters).
  final int? partOfWordMaxLetters;

  /// Grammar prefixes: max total letters of a stacked prefix. Null = no cap.
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
    flag('exact_quotes', exactQuotes);
    flag('spelling_errors', spellingErrors);
    if (typoMode == TypoMode.fullEdit1) m['typo_mode'] = 'full_edit1';
    flag('match_nikud', matchNikud);
    flag('match_teamim', matchTeamim);
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

  /// Levenshtein neighbours of [word], distance 1..3 (3 needs the FM-index
  /// sidecar; the walk path clamps to 2), closest first. With [ignoreQuotes]
  /// distances are measured on the quote-free forms.
  const factory QueryGroup.fuzzy(String word, int distance, {bool ignoreQuotes}) = FuzzyGroup;

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
  const FuzzyGroup(this.word, this.distance, {this.ignoreQuotes = false});
  final String word;
  final int distance;
  final bool ignoreQuotes;
  @override
  Map<String, Object?> toJson() =>
      {'kind': 'fuzzy', 'word': word, 'distance': distance.clamp(1, 3), 'ignore_quotes': ignoreQuotes};
}

class AnyGroup extends QueryGroup {
  const AnyGroup(this.alternatives);
  final List<QueryGroup> alternatives;
  @override
  Map<String, Object?> toJson() => {'kind': 'any', 'alternatives': [for (final a in alternatives) a.toJson()]};
}

/// Otzaria's word proximity, an engine rule of its own (the engine's
/// `ProximitySpec`). With positions it runs in the query; without, the
/// snippet locator validates it on the line text: both return the same lines.
class Proximity {
  /// Groups in query order, at most gaps[i] words between group i and group
  /// i+1 (0 = adjacent); one entry per consecutive pair.
  const Proximity.ordered(List<int> this.gaps) : distance = 0;

  /// Groups in any order, each at its own word, all inside one window of
  /// (k - 1) * (distance + 1) positions for k groups.
  const Proximity.anyOrder(this.distance) : gaps = null;

  final List<int>? gaps;
  final int distance;

  Map<String, Object?> toJson() => gaps == null
      ? {'mode': 'any_order', 'distance': distance < 0 ? 0 : distance}
      : {
          'mode': 'ordered',
          'gaps': [for (final g in gaps!) g < 0 ? 0 : g]
        };
}

/// Otzaria's `WordMatchMode` for [SearchOptions.wordMatch]: how many slots a
/// line needs, counted by the engine over the slots it searches (a dropped
/// word is not counted). all = every slot, anyWord = 1, mostWords = n/2 + 1,
/// atLeast = [count] (at least 1).
class WordMatch {
  const WordMatch.all() : mode = 'all', count = 0;
  const WordMatch.anyWord() : mode = 'any_word', count = 0;
  const WordMatch.mostWords() : mode = 'most_words', count = 0;
  const WordMatch.atLeast(this.count) : mode = 'at_least';

  final String mode;
  final int count;

  Map<String, Object?> toJson() => {'mode': mode, if (mode == 'at_least') 'count': count < 0 ? 0 : count};
}

/// A search.
class SearchRequest {
  SearchRequest(
      {List<QueryGroup>? groups,
      this.maxWordDistance,
      this.requireOrdered = false,
      this.contextWords = 8,
      this.gaps,
      this.minGroups,
      this.proximity,
      this.exclude,
      List<List<QueryGroup>>? phraseAlternatives,
      this.books})
      : groups = groups ?? [],
        phraseAlternatives = phraseAlternatives ?? [];

  /// Filtered search by book: only lines whose book id is in this list come
  /// back (empty list = no hits). Null = every book.
  List<int>? books;

  /// Whole-phrase alternatives ORed with [groups] (Otzaria's acronym
  /// expansion: רמב"ם ↔ רבי משה בן מיימון). Each phrase's words are adjacent,
  /// in order and all required, whatever the main query's settings.
  final List<List<QueryGroup>> phraseAlternatives;

  /// Otzaria's negative query: lines carrying these words are dropped.
  /// Null = none.
  ExcludeSpec? exclude;

  /// Partial matching (Otzaria's wordMatchMode): a line needs only this many
  /// of the groups (anyWord = 1, mostWords = n/2 + 1, atLeast = N; clamped to
  /// 1..n). Null = all groups. Below the group count, [maxWordDistance],
  /// [requireOrdered] and [gaps] are not applied; the snippet is the tightest
  /// window holding that many groups.
  int? minGroups;

  /// Per-pair spacing (Otzaria's customSpacing): gaps[i] = max words between
  /// group i and group i+1 (0 = adjacent). Needs an index with positions; one
  /// entry per consecutive pair. Null = not used.
  List<int>? gaps;

  /// Otzaria's word proximity. When set it is the main query's distance rule:
  /// [maxWordDistance], [requireOrdered] and [gaps] are ignored for it (they
  /// are FtsLib's rule). [minGroups] below the group count wins over it.
  Proximity? proximity;

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
        if (minGroups != null) 'min_groups': minGroups,
        if (proximity != null) 'proximity': proximity!.toJson(),
        if (exclude != null) 'exclude': exclude!.toJson(),
        if (books != null) 'books': books,
        if (phraseAlternatives.isNotEmpty)
          'phrase_alternatives': [
            for (final p in phraseAlternatives) [for (final g in p) g.toJson()]
          ],
      };
}

/// Otzaria's negative query: the words to exclude, run as a query of their own
/// with its own distance, order and spacing, always with every word required.
/// A line matching it is dropped. Several words bound by
/// [maxWordDistance] / [requireOrdered] (without [gaps]) are checked on the
/// line text when a snippet is located, so such lines are still among the hits
/// and locate as null. Section-scoped exclusion is the host's job: search
/// each negative word on its own, map the returned line ids to sections with
/// SQLite (`line_toc`), intersect, and drop the positive hits there.
class ExcludeSpec {
  ExcludeSpec(
      {List<QueryGroup>? groups,
      this.maxWordDistance,
      this.requireOrdered = false,
      this.gaps,
      this.proximity,
      List<List<QueryGroup>>? phraseAlternatives})
      : groups = groups ?? [],
        phraseAlternatives = phraseAlternatives ?? [];

  /// AND across groups, OR within each group's expansion.
  final List<QueryGroup> groups;

  /// Whole phrases ORed with the negative query (Otzaria's acronym
  /// expansion), each as [SearchRequest.phraseAlternatives]: adjacent, in
  /// order, all words. A line carrying one is dropped.
  final List<List<QueryGroup>> phraseAlternatives;

  /// Words allowed between the excluded words. Null = anywhere in the line.
  int? maxWordDistance;

  /// The excluded words must appear in this order.
  bool requireOrdered;

  /// Per-pair spacing for the excluded words (needs positions).
  List<int>? gaps;

  /// Otzaria's proximity for the excluded words; when set, [maxWordDistance],
  /// [requireOrdered] and [gaps] are ignored.
  Proximity? proximity;

  Map<String, Object?> toJson() => {
        'groups': [for (final g in groups) g.toJson()],
        'max_word_distance': maxWordDistance,
        'require_ordered': requireOrdered,
        if (gaps != null) 'gaps': [for (final g in gaps!) g < 0 ? 0 : g],
        if (proximity != null) 'proximity': proximity!.toJson(),
        if (phraseAlternatives.isNotEmpty)
          'phrase_alternatives': [
            for (final p in phraseAlternatives) [for (final g in p) g.toJson()]
          ],
      };
}

/// Exact totals of a search (Otzaria's count / countByBook): the lines that
/// pass every filter, in all and per book id.
class HitCounts {
  const HitCounts(this.total, this.byBook, this.validated, this.exact);

  /// Lines that passed, among the hits counted so far.
  final int total;

  /// Lines per book id (books with none absent).
  final Map<int, int> byBook;

  /// Hits counted so far, from the first in hit order.
  final int validated;

  /// True when every hit is counted: the numbers are final.
  final bool exact;

  static HitCounts fromJson(Map<String, dynamic> j) => HitCounts(
        j['total'] as int,
        {for (final p in (j['books'] as List)) (p as List)[0] as int: p[1] as int},
        j['validated'] as int,
        j['exact'] as bool,
      );
}

/// A matching line (in the search's order, catalog by default): only the ids the index keeps. Any
/// other placement (line index, section) comes from SQLite by line id.
class PlacedHit {
  const PlacedHit(this.position, this.lineId, this.bookId);
  final int position;
  final int lineId;
  final int bookId;
  @override
  String toString() => 'PlacedHit(pos $position, line $lineId, book $bookId)';
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

  /// Fewer words than the per-side context ("short"): the host may locate
  /// again over a longer text, e.g. the line joined with its neighbours.
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

/// THE search API: the query string (FtsLib syntax: words AND-ed, `a | b`
/// one slot, `*` `?` wildcard, `word~N` fuzzy, `%word` / `word%` FtsLib's
/// grammar tables) and the word options, which apply to every word unless
/// [words] overrides them for one word (Otzaria's wordOptions /
/// alternativeWords). The engine builds one walk per word with all its
/// options. [SearchRequest] is the low-level form it lowers to.
class SearchOptions {
  SearchOptions(
    this.query, {
    this.grammarPrefixes = false,
    this.grammarSuffixes = false,
    this.aramaicPrefixes = false,
    this.aramaicSuffixes = false,
    this.ktivMaleHaser = false,
    this.partOfWord = false,
    this.advancedPrefixes = false,
    this.advancedSuffixes = false,
    this.ignoreQuotes = false,
    this.exactQuotes = false,
    this.spellingErrors = false,
    this.matchNikud = false,
    this.matchTeamim = false,
    this.typoMode = TypoMode.otzaria,
    this.prefixMaxLetters,
    this.suffixMaxLetters,
    this.partOfWordMaxLetters,
    this.grammarPrefixMaxLetters,
    Map<int, WordOptions>? words,
    this.maxWordDistance,
    this.requireOrdered = false,
    this.minGroups,
    this.wordMatch,
    this.contextWords = 8,
    this.proximity,
    this.exclude,
    this.books,
    List<PhraseOptions>? phraseAlternatives,
    this.userRegex = false,
    this.order = ResultsOrder.catalog,
    Map<int, int>? bookGenerations,
  })  : words = words ?? {},
        phraseAlternatives = phraseAlternatives ?? [],
        bookGenerations = bookGenerations ?? {};

  String query;

  /// Otzaria's user regex: a query wrapped in `/.../` is a regex. Off (the
  /// default, FtsLib's behaviour): the query is plain text.
  bool userRegex;

  // Word options: every word, unless [words] overrides (see [WalkOptions]
  // for each one's meaning).
  bool grammarPrefixes, grammarSuffixes, aramaicPrefixes, aramaicSuffixes;
  bool ktivMaleHaser, partOfWord, advancedPrefixes, advancedSuffixes;

  /// התעלם מגרשיים (also for wildcard / fuzzy / `%` words).
  bool ignoreQuotes;
  bool exactQuotes, spellingErrors, matchNikud, matchTeamim;
  TypoMode typoMode;
  int? prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters, grammarPrefixMaxLetters;

  /// Per-word overrides by word index: the place, from 0, of the word as
  /// typed ([query] cut on whitespace and `|`; `|` is not a word; a word the
  /// engine drops keeps its place). A typed word the separators split
  /// (maqaf, punctuation, Hebrew/Latin) gives its override to every part.
  final Map<int, WordOptions> words;

  /// FtsLib's word distance. Null = no limit.
  int? maxWordDistance;

  /// FtsLib: the words in query order.
  bool requireOrdered;

  /// Partial matching as a slot count: slots a line needs. Null = all.
  /// [wordMatch] wins when set.
  int? minGroups;

  /// Partial matching (Otzaria's wordMatchMode), counted by the engine over
  /// the slots it searches.
  WordMatch? wordMatch;

  /// Words of context per side of the snippet.
  int contextWords;

  /// Otzaria's proximity, per TYPED word (gaps[i] between typed words i and
  /// i+1, words the engine drops included: it cuts them out and merges the
  /// gaps around them). When set, [maxWordDistance] / [requireOrdered] are
  /// not used.
  Proximity? proximity;

  /// Otzaria's negative query: its words take the word options above as
  /// their default and have their own per-word overrides
  /// ([ExcludeOptions.words]).
  ExcludeOptions? exclude;

  /// Filtered search: only these book ids. Null = every book.
  List<int>? books;

  /// Whole phrases ORed with the query (Otzaria's acronym expansion).
  final List<PhraseOptions> phraseAlternatives;

  /// The order of the hits. Catalog by default.
  ResultsOrder order;

  /// For [ResultsOrder.generation]: book id to its generation order
  /// (Otzaria's generationOrder, from the host). Books absent come last;
  /// negative ids (no such book) are not sent.
  final Map<int, int> bookGenerations;

  Map<String, Object?> toJson() => {
        'query': query,
        if (userRegex) 'user_regex': true,
        ..._wordFields(grammarPrefixes, grammarSuffixes, aramaicPrefixes, aramaicSuffixes, ktivMaleHaser,
            partOfWord, advancedPrefixes, advancedSuffixes, ignoreQuotes, exactQuotes, spellingErrors,
            matchNikud, matchTeamim, typoMode, prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters,
            grammarPrefixMaxLetters),
        if (words.isNotEmpty) 'words': {for (final e in words.entries) '${e.key}': e.value.toJson()},
        if (maxWordDistance != null) 'max_word_distance': maxWordDistance,
        if (requireOrdered) 'require_ordered': true,
        if (minGroups != null) 'min_groups': minGroups,
        if (wordMatch != null) 'word_match': wordMatch!.toJson(),
        'context_words': contextWords,
        if (proximity != null) 'proximity': proximity!.toJson(),
        if (exclude != null) 'exclude': exclude!.toJson(),
        if (books != null) 'books': books,
        if (phraseAlternatives.isNotEmpty) 'phrase_alternatives': [for (final p in phraseAlternatives) p.toJson()],
        if (order != ResultsOrder.catalog) 'order': order.name,
        if (bookGenerations.isNotEmpty)
          'book_generations': [
            for (final e in bookGenerations.entries)
              if (e.key >= 0) [e.key, e.value.clamp(0, 0xFFFFFFFF)]
          ],
      };
}

Map<String, Object?> _wordFields(
    bool? grammarPrefixes,
    bool? grammarSuffixes,
    bool? aramaicPrefixes,
    bool? aramaicSuffixes,
    bool? ktivMaleHaser,
    bool? partOfWord,
    bool? advancedPrefixes,
    bool? advancedSuffixes,
    bool? ignoreQuotes,
    bool? exactQuotes,
    bool? spellingErrors,
    bool? matchNikud,
    bool? matchTeamim,
    TypoMode? typoMode,
    int? prefixMaxLetters,
    int? suffixMaxLetters,
    int? partOfWordMaxLetters,
    int? grammarPrefixMaxLetters) {
  final m = <String, Object?>{};
  void flag(String k, bool? v) {
    if (v != null) m[k] = v;
  }

  void limit(String k, int? v) {
    if (v != null) m[k] = v < 0 ? 0 : v;
  }

  flag('grammar_prefixes', grammarPrefixes);
  flag('grammar_suffixes', grammarSuffixes);
  flag('aramaic_prefixes', aramaicPrefixes);
  flag('aramaic_suffixes', aramaicSuffixes);
  flag('ktiv_male_haser', ktivMaleHaser);
  flag('part_of_word', partOfWord);
  flag('advanced_prefixes', advancedPrefixes);
  flag('advanced_suffixes', advancedSuffixes);
  flag('ignore_quotes', ignoreQuotes);
  flag('exact_quotes', exactQuotes);
  flag('spelling_errors', spellingErrors);
  flag('match_nikud', matchNikud);
  flag('match_teamim', matchTeamim);
  if (typoMode != null) m['typo_mode'] = typoMode == TypoMode.fullEdit1 ? 'full_edit1' : 'otzaria';
  limit('prefix_max_letters', prefixMaxLetters);
  limit('suffix_max_letters', suffixMaxLetters);
  limit('part_of_word_max_letters', partOfWordMaxLetters);
  limit('grammar_prefix_max_letters', grammarPrefixMaxLetters);
  return m;
}

/// One word's overrides: the same fields as [SearchOptions]' word options,
/// null = inherit, plus extra words for the word's slot.
class WordOptions {
  WordOptions({
    this.grammarPrefixes,
    this.grammarSuffixes,
    this.aramaicPrefixes,
    this.aramaicSuffixes,
    this.ktivMaleHaser,
    this.partOfWord,
    this.advancedPrefixes,
    this.advancedSuffixes,
    this.ignoreQuotes,
    this.exactQuotes,
    this.spellingErrors,
    this.matchNikud,
    this.matchTeamim,
    this.typoMode,
    this.prefixMaxLetters,
    this.suffixMaxLetters,
    this.partOfWordMaxLetters,
    this.grammarPrefixMaxLetters,
    List<String>? alternatives,
  }) : alternatives = alternatives ?? [];

  bool? grammarPrefixes, grammarSuffixes, aramaicPrefixes, aramaicSuffixes;
  bool? ktivMaleHaser, partOfWord, advancedPrefixes, advancedSuffixes;
  bool? ignoreQuotes, exactQuotes, spellingErrors, matchNikud, matchTeamim;
  TypoMode? typoMode;
  int? prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters, grammarPrefixMaxLetters;

  /// Extra words for this word's slot (Otzaria's alternativeWords), in query
  /// syntax, searched with this word's options.
  final List<String> alternatives;

  Map<String, Object?> toJson() => {
        if (alternatives.isNotEmpty) 'alternatives': alternatives,
        ..._wordFields(grammarPrefixes, grammarSuffixes, aramaicPrefixes, aramaicSuffixes, ktivMaleHaser,
            partOfWord, advancedPrefixes, advancedSuffixes, ignoreQuotes, exactQuotes, spellingErrors,
            matchNikud, matchTeamim, typoMode, prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters,
            grammarPrefixMaxLetters),
      };
}

/// The negative query of [SearchOptions] (Otzaria's negativeQuery): its
/// words, all required, together. Options are per word: a negative word takes
/// the global word options of [SearchOptions] as its default and has its own
/// overrides and alternatives in [words] (Otzaria's negativeSearchOptions /
/// negativeAlternativeWords). There is no separate global set for the
/// negative query: a host with one writes it onto every negative word.
class ExcludeOptions {
  ExcludeOptions(this.query,
      {Map<int, WordOptions>? words,
      List<PhraseOptions>? phraseAlternatives,
      this.maxWordDistance,
      this.requireOrdered = false,
      this.proximity})
      : words = words ?? {},
        phraseAlternatives = phraseAlternatives ?? [];
  String query;

  /// Per-word overrides of [query]'s words, by typed-word index, with the
  /// same rule and fields as [SearchOptions.words].
  final Map<int, WordOptions> words;

  /// Whole phrases ORed with the negative query (Otzaria's acronym expansion
  /// of the negative words): a line carrying one, as a phrase, is dropped too.
  final List<PhraseOptions> phraseAlternatives;
  int? maxWordDistance;
  bool requireOrdered;
  Proximity? proximity;

  Map<String, Object?> toJson() => {
        'query': query,
        if (words.isNotEmpty) 'words': {for (final e in words.entries) '${e.key}': e.value.toJson()},
        if (phraseAlternatives.isNotEmpty) 'phrase_alternatives': [for (final p in phraseAlternatives) p.toJson()],
        if (maxWordDistance != null) 'max_word_distance': maxWordDistance,
        if (requireOrdered) 'require_ordered': true,
        if (proximity != null) 'proximity': proximity!.toJson(),
      };
}

/// A whole-phrase alternative: words adjacent, in order, all required,
/// matched as written (plus the quote option).
class PhraseOptions {
  const PhraseOptions(this.query, {this.ignoreQuotes = false});
  final String query;
  final bool ignoreQuotes;

  Map<String, Object?> toJson() => {'query': query, 'ignore_quotes': ignoreQuotes};
}
