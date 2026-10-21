import 'package:seforim_search/seforim_search.dart';

import 'acronym_lexicon.dart';
import 'translation_lexicon.dart';

/// Otzaria's search modes (`SearchMode`).
enum SearchMode { exact, advanced, fuzzy }

/// Otzaria's `WordMatchMode`: how many of the query words a result needs.
enum WordMatchMode { all, anyWord, mostWords, atLeast }

/// Otzaria's `SearchScope` for the negative query: within the distance, the
/// whole line (paragraph), or the whole section.
enum NegativeScope { wordDistance, sameParagraph, sameSection }

/// Otzaria's per-word option keys (literal Hebrew strings in Otzaria).
abstract final class OtzariaOption {
  static const grammaticalPrefixes = 'קידומות דקדוקיות';
  static const grammaticalSuffixes = 'סיומות דקדוקיות';
  static const prefixes = 'קידומות';
  static const suffixes = 'סיומות';
  static const fullOrDefective = 'כתיב מלא/חסר';
  static const partial = 'חלק ממילה';
  static const typoTolerance = 'שגיאות כתיב';
  static const aramaicPrefixes = 'קידומות ארמיות';
  static const aramaicSuffixes = 'סיומות ארמיות';
  static const ignoreGershayim = 'התעלם מגרשיים';
  static const aramaicTranslation = 'תרגום ארמי'; // host lexicon (TranslationLexicon)
  static const acronyms = 'ראשי תיבות'; // host lexicon (AcronymLexicon)
  static const nikud = 'ניקוד';
  static const teamim = 'טעמים';

  /// Options exact mode allows (Otzaria `exactWordOptionKeys`).
  static const exactAllowed = {grammaticalPrefixes, grammaticalSuffixes, prefixes, suffixes, fullOrDefective};
  static const unsupported = <String>{};
}

/// Otzaria's query sanitizer: ״/׳ → "/', maqaf and hyphen → space, collapse
/// spaces; `"` at a word edge separates, between letters it stays (רמב"ם);
/// `''` → `"`.
List<String> splitQueryWords(String query) {
  var q = query.replaceAll('״', '"').replaceAll('׳', "'").replaceAll("''", '"').replaceAll(RegExp('[־-]'), ' ');
  final words = <String>[];
  for (var w in q.split(RegExp(r'\s+'))) {
    w = w.replaceAll(RegExp(r'^"+|"+$'), '').replaceAll(RegExp(r"^'+"), '');
    if (w.isNotEmpty) words.add(w);
  }
  return words;
}


/// Everything an Otzaria search request carries that the engine can use.
class OtzariaSearch {
  OtzariaSearch({
    required this.query,
    this.mode = SearchMode.advanced,
    Map<String, bool>? globalOptions,
    Map<int, Set<String>>? wordOptions,
    Map<int, List<String>>? alternativeWords,
    this.distance = 0,
    this.anyOrder = false,
    this.fuzzyDistance = 1,
    this.wordMatchMode = WordMatchMode.all,
    this.wordMatchCount = 1,
    this.prefixMaxLetters,
    this.suffixMaxLetters,
    this.partOfWordMaxLetters,
    this.grammarPrefixMaxLetters,
    this.marksIndex = false,
    this.customSpacing,
    this.negativeQuery = '',
    this.negativeScope = NegativeScope.wordDistance,
    this.translations,
    this.acronymLexicon,
  })  :globalOptions = globalOptions ?? {},
        wordOptions = wordOptions ?? {},
        alternativeWords = alternativeWords ?? {};

  final String query;
  final SearchMode mode;

  /// Options applied to every word (`expandGlobalOptionsToWords`).
  final Map<String, bool> globalOptions;

  /// Per-word options by word index (Otzaria keys them "{word}_{index}").
  final Map<int, Set<String>> wordOptions;
  final Map<int, List<String>> alternativeWords;

  /// Word gap (slop). Otzaria's default scope is "in order, with a gap".
  final int distance;
  final bool anyOrder;

  /// Fuzzy mode edit distance (Otzaria kMaxFuzzyDistance = 2).
  final int fuzzyDistance;

  /// Otzaria's wordMatchMode / wordMatchCount (count is for atLeast). Only
  /// the advanced mode honours it (exact and fuzzy run "all").
  final WordMatchMode wordMatchMode;
  final int wordMatchCount;

  /// Otzaria `WordMatch::min_words`: all = n, anyWord = 1, mostWords =
  /// n/2 + 1, atLeast = count (at least 1); never more than n.
  int minWords(int n) {
    final min = switch (wordMatchMode) {
      WordMatchMode.all => n,
      WordMatchMode.anyWord => 1,
      WordMatchMode.mostWords => n ~/ 2 + 1,
      WordMatchMode.atLeast => wordMatchCount < 1 ? 1 : wordMatchCount,
    };
    return min < n ? min : n;
  }

  final int? prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters, grammarPrefixMaxLetters;

  /// The index was built with keepMarks (nikud/te'amim kept in terms).
  final bool marksIndex;

  /// Otzaria's customSpacing: gap per consecutive word pair (index i = between
  /// word i and i+1). As Otzaria's `resolve_gaps`: a missing pair takes the
  /// largest custom value, and any order is ignored when it is set.
  final Map<int, int>? customSpacing;

  /// Otzaria's negativeQuery: words that must not appear. It runs with the
  /// same distance / order as the positive query (Otzaria passes
  /// `negativeDistance: state.distance`, `negativeScope: proximityScope`) and,
  /// in this demo, the same global options. Empty = none.
  final String negativeQuery;

  /// Otzaria's Aramaic↔Hebrew dictionary for "תרגום ארמי"; null = not loaded
  /// (the option then warns and does nothing, as Otzaria without the asset).
  final TranslationLexicon? translations;

  /// Otzaria's acronym dictionary for "ראשי תיבות"; null = not loaded.
  final AcronymLexicon? acronymLexicon;

  /// Otzaria acronym_alternatives: when any word has the option, the whole
  /// query's phrase alternatives (advanced mode only).
  List<List<QueryGroup>> _phraseAlternatives(List<String> words) {
    if (mode != SearchMode.advanced) return const [];
    // Otzaria: no acronym alternatives in vocalized (nikud/te'amim) search
    if (globalOptions[OtzariaOption.nikud] == true || globalOptions[OtzariaOption.teamim] == true) return const [];
    var enabled = false;
    for (var i = 0; i < words.length; i++) {
      if (_optionsFor(i).contains(OtzariaOption.acronyms)) enabled = true;
    }
    if (!enabled) return const [];
    final lex = acronymLexicon;
    if (lex == null) {
      warnings.add('option "${OtzariaOption.acronyms}" needs --acronyms (Otzaria assets/Acronyms.json)');
      return const [];
    }
    // A decoded phrase is literal: exact words, no options. A one-word
    // alternative is a quote-free acronym (רמבן): Otzaria's index carries a
    // quote-free twin token, ours keeps רמב"ן, so the quote is made
    // transparent instead.
    return [
      for (final p in lex.alternativesFor(words))
        [for (final w in p) p.length == 1 ? QueryGroup.word(w, const WalkOptions(ignoreQuotes: true)) : QueryGroup.word(w)]
    ];
  }

  /// Otzaria's negativeScope (its proximityScope).
  final NegativeScope negativeScope;

  /// Warnings about options the engine cannot honour.
  final List<String> warnings = [];

  Set<String> _optionsFor(int i) {
    final s = {
      ...globalOptions.entries.where((e) => e.value).map((e) => e.key),
      ...?wordOptions[i],
    };
    if (mode == SearchMode.exact) s.retainAll(OtzariaOption.exactAllowed);
    if (mode == SearchMode.fuzzy) s.clear();
    for (final u in s.intersection(OtzariaOption.unsupported)) {
      warnings.add('option "$u" is not supported by the engine; ignored');
    }
    return s;
  }

  QueryGroup _wordGroup(String w, Set<String> o) {
    if (w.contains('*') || w.contains('?')) return QueryGroup.wildcard(w);
    if (mode == SearchMode.fuzzy) return QueryGroup.fuzzy(w, fuzzyDistance);
    // Otzaria's 3-letter root floor for grammatical affixes (acronyms exempt)
    // is the engine's walk rule; nothing to do here.
    return QueryGroup.word(
      w,
      WalkOptions(
        grammarPrefixes: o.contains(OtzariaOption.grammaticalPrefixes),
        grammarSuffixes: o.contains(OtzariaOption.grammaticalSuffixes),
        advancedPrefixes: o.contains(OtzariaOption.prefixes),
        advancedSuffixes: o.contains(OtzariaOption.suffixes),
        ktivMaleHaser: o.contains(OtzariaOption.fullOrDefective),
        partOfWord: o.contains(OtzariaOption.partial),
        spellingErrors: o.contains(OtzariaOption.typoTolerance),
        aramaicPrefixes: o.contains(OtzariaOption.aramaicPrefixes),
        aramaicSuffixes: o.contains(OtzariaOption.aramaicSuffixes),
        ignoreQuotes: o.contains(OtzariaOption.ignoreGershayim),
        // Otzaria's ניקוד/טעמים REQUIRE the typed marks; on a keep-marks index
        // that is the default (no ignore flags). Without them, ignore marks.
        ignoreNikud: marksIndex && !o.contains(OtzariaOption.nikud),
        ignoreTeamim: marksIndex && !o.contains(OtzariaOption.teamim),
        prefixMaxLetters: prefixMaxLetters,
        suffixMaxLetters: suffixMaxLetters,
        partOfWordMaxLetters: partOfWordMaxLetters,
        grammarPrefixMaxLetters: grammarPrefixMaxLetters,
      ),
    );
  }

  /// Otzaria's apply_advanced_negative_query: the negative words as a query
  /// of their own, always "all words", with the positive distance and order.
  ExcludeSpec? _exclude() {
    if (negativeScope == NegativeScope.sameSection) return null; // host-side, see sectionNegativeWords
    final groups = _negativeGroups();
    if (groups.isEmpty) return null;
    // proximityScope: wordDistance → distance/order; sameParagraph → the
    // whole line, no limits.
    final limits = negativeScope == NegativeScope.wordDistance && groups.length > 1;
    // the demo has no separate negative spacing (Otzaria's
    // negativeCustomSpacing): the positive distance and order
    return ExcludeSpec(groups: groups, proximity: limits ? _proximity(groups.length, null) : null);
  }

  /// Otzaria's `resolve_gaps` + `resolve_proximity` (hebrew_query.rs) for
  /// [wordCount] words, sent as the engine's own proximity rule: with
  /// [spacing], each pair its value (a missing pair takes the largest one),
  /// in order; without, [distance] for every pair, in order or (anyOrder)
  /// all words inside one window of (k-1)(distance+1) positions.
  Proximity _proximity(int wordCount, Map<int, int>? spacing) {
    if (spacing == null || spacing.isEmpty) {
      return anyOrder
          ? Proximity.anyOrder(distance)
          : Proximity.ordered(List.filled(wordCount - 1, distance));
    }
    final maxCustom = spacing.values.fold<int>(0, (m, v) => v > m ? v : m);
    return Proximity.ordered([for (var i = 0; i < wordCount - 1; i++) spacing[i] ?? maxCustom]);
  }

  /// The negative words as engine groups (advanced mode only).
  List<QueryGroup> _negativeGroups() {
    if (mode != SearchMode.advanced) return const [];
    final words = splitQueryWords(negativeQuery);
    // the global options only (Otzaria keeps separate negative options; the
    // CLI has none), without re-reporting the unsupported-option warnings
    final o = {...globalOptions.entries.where((e) => e.value).map((e) => e.key)}
      ..removeAll(OtzariaOption.unsupported);
    return [for (final w in words) _wordGroup(w, o)];
  }

  /// Negative scope sameSection (Otzaria's scoped_words_query) is the HOST's
  /// job, since the index holds no sections: run each of these words as its
  /// own search (line ids), map them to sections with SQLite (`line_toc`),
  /// intersect the per-word section sets ([blockedSections]) and drop the
  /// positive hits in them. Empty unless the scope is sameSection.
  List<QueryGroup> sectionNegativeWords() =>
      negativeScope == NegativeScope.sameSection ? _negativeGroups() : const [];

  /// The engine request.
  SearchRequest toRequest({int contextWords = 8}) {
    final words = splitQueryWords(query);
    final groups = <QueryGroup>[];
    for (var i = 0; i < words.length; i++) {
      final o = _optionsFor(i);
      final alts = <String>[
        if (mode == SearchMode.advanced) ...?alternativeWords[i],
        // Otzaria translation_alternatives: appended to the word's alternatives
        if (mode == SearchMode.advanced && o.contains(OtzariaOption.aramaicTranslation))
          ...?translations?.expansions(words[i]),
      ];
      if (mode == SearchMode.advanced && o.contains(OtzariaOption.aramaicTranslation) && translations == null) {
        warnings.add('option "${OtzariaOption.aramaicTranslation}" needs --dictionary (Otzaria assets/dictionary.json)');
      }
      final main = _wordGroup(words[i], o);
      groups.add(alts.isEmpty ? main : QueryGroup.any([main, for (final a in alts) _wordGroup(a, o)]));
    }
    final exclude = _exclude();
    final phrases = _phraseAlternatives(words);
    // Partial matching: at least minWords of the words; order, distance and
    // spacing do not apply (Otzaria: the match is at line resolution).
    final min = mode == SearchMode.advanced ? minWords(groups.length) : groups.length;
    if (min < groups.length) {
      return SearchRequest(
          groups: groups, minGroups: min, contextWords: contextWords, exclude: exclude, phraseAlternatives: phrases);
    }
    return SearchRequest(
      groups: groups,
      proximity: groups.length > 1 ? _proximity(groups.length, customSpacing) : null,
      contextWords: contextWords,
      exclude: exclude,
      phraseAlternatives: phrases,
    );
  }
}

/// Otzaria's SameSection negative scope: a section is blocked when it holds
/// EVERY negative word, each anywhere in it. [perWord] = the sections each
/// word's lines fall in. No words = nothing blocked.
Set<int> blockedSections(List<Set<int>> perWord) {
  if (perWord.isEmpty) return {};
  var blocked = perWord.first;
  for (final s in perWord.skip(1)) {
    blocked = blocked.intersection(s);
  }
  return blocked;
}
