import 'package:seforim_search/seforim_search.dart';

import 'acronym_lexicon.dart';
import 'translation_lexicon.dart';

/// Otzaria's search modes (`SearchMode`).
enum SearchMode { exact, advanced, fuzzy }

/// Otzaria's `WordMatchMode`: how many of the query words a result needs.
enum WordMatchMode { all, anyWord, mostWords, atLeast }

/// Otzaria's `UserRegexQuery._isWrapped`: longer than 2 chars, starts and
/// ends with `/`, and no slash next to a space inside (`/a/ /b/` is words).
bool isUserRegex(String query) {
  final q = query.trim();
  return q.length > 2 &&
      q.startsWith('/') &&
      q.endsWith('/') &&
      !RegExp(r'\s/|/\s').hasMatch(q.substring(1, q.length - 1));
}

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

/// Otzaria's query sanitizer: ״/׳ → "/', maqaf, hyphen and `|` → space
/// (Otzaria: `|` separates; the engine cuts typed words there too), collapse
/// spaces; `"` at a word edge separates, between letters it stays (רמב"ם);
/// `''` → `"`.
///
/// Paired readings `(X) [Y]` keep only Y first, by the engine's own rule
/// (Otzaria's `keep_second_of_paired_readings`).
List<String> splitQueryWords(String query) {
  var q = SeforimIndex.reducePairedReadings(query).replaceAll('״', '"').replaceAll('׳', "'").replaceAll("''", '"').replaceAll(RegExp('[־|-]'), ' ');
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
    this.negativeWordOptions,
    Map<int, List<String>>? negativeAlternativeWords,
    this.translations,
    this.acronymLexicon,
  })  :globalOptions = globalOptions ?? {},
        wordOptions = wordOptions ?? {},
        alternativeWords = alternativeWords ?? {},
        negativeAlternativeWords = negativeAlternativeWords ?? {};

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

  /// The mode as the engine's [WordMatch]; the engine counts the words
  /// itself (Otzaria `WordMatch::min_words` over the slots it searches).
  WordMatch get wordMatch => switch (wordMatchMode) {
        WordMatchMode.all => const WordMatch.all(),
        WordMatchMode.anyWord => const WordMatch.anyWord(),
        WordMatchMode.mostWords => const WordMatch.mostWords(),
        WordMatchMode.atLeast => WordMatch.atLeast(wordMatchCount),
      };

  final int? prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters, grammarPrefixMaxLetters;

  /// The index was built with keepMarks (nikud/te'amim kept in terms).
  final bool marksIndex;

  /// Otzaria's customSpacing: gap per consecutive word pair (index i = between
  /// word i and i+1). As Otzaria's `resolve_gaps`: a missing pair takes the
  /// largest custom value, and any order is ignored when it is set.
  final Map<int, int>? customSpacing;

  /// Otzaria's negativeQuery: words that must not appear. It runs with the
  /// same distance / order as the positive query (Otzaria passes
  /// `negativeDistance: state.distance`, `negativeScope: proximityScope`),
  /// with options per word ([negativeWordOptions]). Empty = none.
  final String negativeQuery;

  /// Otzaria's negativeSearchOptions as the app sends them
  /// (`effectiveNegativeSearchOptions`: its negative global set expanded onto
  /// every word, or the per-word map), by negative word index: each word's
  /// FULL option set; a word absent here has none. Null = every negative
  /// word takes the positive global options (the engine's default).
  final Map<int, Set<String>>? negativeWordOptions;

  /// Otzaria's negativeAlternativeWords: extra words per negative word.
  final Map<int, List<String>> negativeAlternativeWords;

  /// Otzaria's Aramaic↔Hebrew dictionary for "תרגום ארמי"; null = not loaded
  /// (the option then warns and does nothing, as Otzaria without the asset).
  final TranslationLexicon? translations;

  /// Otzaria's acronym dictionary for "ראשי תיבות"; null = not loaded.
  final AcronymLexicon? acronymLexicon;

  /// Otzaria acronym_alternatives: when any word has the option, the whole
  /// query's phrase alternatives (advanced mode only).
  List<PhraseOptions> _phraseAlternatives(List<String> words, Set<String> Function(int) optionsOf) {
    if (mode != SearchMode.advanced) return const [];
    // Otzaria: no acronym alternatives in vocalized (nikud/te'amim) search
    if (globalOptions[OtzariaOption.nikud] == true || globalOptions[OtzariaOption.teamim] == true) return const [];
    var enabled = false;
    for (var i = 0; i < words.length; i++) {
      final o = optionsOf(i);
      // a word's own nikud/te'amim option makes the build vocalized too
      // (Otzaria: voc.or(options_vocalized_flags(search_options)))
      if (o.contains(OtzariaOption.nikud) || o.contains(OtzariaOption.teamim)) return const [];
      if (o.contains(OtzariaOption.acronyms)) enabled = true;
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
    return [for (final p in lex.alternativesFor(words)) PhraseOptions(p.join(' '), ignoreQuotes: p.length == 1)];
  }

  /// Otzaria's negativeScope (its proximityScope).
  final NegativeScope negativeScope;

  /// Warnings about options the engine cannot honour.
  final List<String> warnings = [];

  /// The options every word gets (`expandGlobalOptionsToWords`), filtered by mode.
  Set<String> _globalSet() {
    final s = {...globalOptions.entries.where((e) => e.value).map((e) => e.key)};
    if (mode == SearchMode.exact) s.retainAll(OtzariaOption.exactAllowed);
    if (mode == SearchMode.fuzzy) s.clear();
    return s;
  }

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

  /// A word as the engine's query syntax: the fuzzy mode is `word~N`
  /// (wildcards stay wildcards).
  String _queryWord(String w) {
    if (w.contains('*') || w.contains('?')) return w;
    if (mode == SearchMode.fuzzy) return '$w~$fuzzyDistance';
    return w;
  }

  /// [o] (Otzaria option keys) as the engine's global word options.
  SearchOptions _withOptions(SearchOptions s, Set<String> o) {
    // Otzaria's 3-letter root floor for grammatical affixes (acronyms exempt)
    // is the engine's walk rule; nothing to do here.
    s
      ..grammarPrefixes = o.contains(OtzariaOption.grammaticalPrefixes)
      ..grammarSuffixes = o.contains(OtzariaOption.grammaticalSuffixes)
      ..advancedPrefixes = o.contains(OtzariaOption.prefixes)
      ..advancedSuffixes = o.contains(OtzariaOption.suffixes)
      ..ktivMaleHaser = o.contains(OtzariaOption.fullOrDefective)
      ..partOfWord = o.contains(OtzariaOption.partial)
      ..spellingErrors = o.contains(OtzariaOption.typoTolerance)
      ..aramaicPrefixes = o.contains(OtzariaOption.aramaicPrefixes)
      ..aramaicSuffixes = o.contains(OtzariaOption.aramaicSuffixes)
      ..ignoreQuotes = o.contains(OtzariaOption.ignoreGershayim)
      // Otzaria's nikud / te'amim options: typed marks of the class required,
      // untyped marks free, only lines that carry a mark (the engine's
      // match_nikud / match_teamim; they need a keep-marks index).
      ..matchNikud = o.contains(OtzariaOption.nikud)
      ..matchTeamim = o.contains(OtzariaOption.teamim)
      ..prefixMaxLetters = prefixMaxLetters
      ..suffixMaxLetters = suffixMaxLetters
      ..partOfWordMaxLetters = partOfWordMaxLetters
      ..grammarPrefixMaxLetters = grammarPrefixMaxLetters;
    return s;
  }

  /// Word [i]'s overrides of the global options ([own] vs [global]) and its
  /// alternatives; null when it has neither.
  WordOptions? _wordOverride(Set<String> own, Set<String> global, List<String> alternatives) {
    bool? d(String k) => own.contains(k) == global.contains(k) ? null : own.contains(k);
    final w = WordOptions(
      grammarPrefixes: d(OtzariaOption.grammaticalPrefixes),
      grammarSuffixes: d(OtzariaOption.grammaticalSuffixes),
      advancedPrefixes: d(OtzariaOption.prefixes),
      advancedSuffixes: d(OtzariaOption.suffixes),
      ktivMaleHaser: d(OtzariaOption.fullOrDefective),
      partOfWord: d(OtzariaOption.partial),
      spellingErrors: d(OtzariaOption.typoTolerance),
      aramaicPrefixes: d(OtzariaOption.aramaicPrefixes),
      aramaicSuffixes: d(OtzariaOption.aramaicSuffixes),
      ignoreQuotes: d(OtzariaOption.ignoreGershayim),
      matchNikud: d(OtzariaOption.nikud),
      matchTeamim: d(OtzariaOption.teamim),
      alternatives: [for (final a in alternatives) _queryWord(a)],
    );
    return w.toJson().isEmpty ? null : w;
  }

  /// Negative word [i]'s options (Otzaria keys): its own set from
  /// [negativeWordOptions] (none when absent there, as Otzaria), or the
  /// positive global set when [negativeWordOptions] is null.
  Set<String> _negativeOptionsFor(int i) {
    final own = negativeWordOptions;
    if (own == null) return _globalSet();
    final s = {...?own[i]};
    for (final u in s.intersection(OtzariaOption.unsupported)) {
      warnings.add('option "$u" is not supported by the engine; ignored');
    }
    return s;
  }

  /// Otzaria's apply_advanced_negative_query: the negative words as a query
  /// of their own (built like the positive one), always "all words", with
  /// the positive distance and order. Options are per word: each negative
  /// word sends only what differs from the global options, plus its
  /// alternatives (negativeAlternativeWords, Aramaic translations); the
  /// acronym option ORs the negative query's acronym phrases into it.
  ExcludeOptions? _exclude() {
    if (negativeScope == NegativeScope.sameSection) return null; // host-side, see sectionNegativeSearches
    final words = _negativeWords();
    if (words.isEmpty) return null;
    final global = _globalSet();
    final perWord = <int, WordOptions>{};
    final options = [for (var i = 0; i < words.length; i++) _negativeOptionsFor(i)];
    for (var i = 0; i < words.length; i++) {
      final o = options[i];
      final translate = o.contains(OtzariaOption.aramaicTranslation);
      if (translate && translations == null) {
        warnings.add('option "${OtzariaOption.aramaicTranslation}" needs --dictionary (Otzaria assets/dictionary.json)');
      }
      final alts = <String>[
        ...?negativeAlternativeWords[i],
        if (translate) ...?translations?.expansions(words[i]),
      ];
      final w = _wordOverride(o, global, alts);
      if (w != null) perWord[i] = w;
    }
    // proximityScope: wordDistance → distance/order; sameParagraph → the
    // whole line, no limits.
    final limits = negativeScope == NegativeScope.wordDistance && words.length > 1;
    // the demo has no separate negative spacing (Otzaria's
    // negativeCustomSpacing): the positive distance and order
    return ExcludeOptions(words.join(' '),
        words: perWord,
        phraseAlternatives: _phraseAlternatives(words, (i) => options[i]),
        proximity: limits ? _proximity(words.length, null) : null);
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

  /// The negative words (advanced mode only). As for the positive words,
  /// `|` and `%` (FtsLib syntax) are dropped from them, and a word of only
  /// those is dropped, so the word indexes stay those of the engine.
  List<String> _negativeWords() => mode == SearchMode.advanced
      ? [
          for (final w in splitQueryWords(negativeQuery))
            if (w.replaceAll(RegExp('[|%]'), '').isNotEmpty) _queryWord(w.replaceAll(RegExp('[|%]'), ''))
        ]
      : const [];

  /// Negative scope sameSection (Otzaria's scoped_words_query) is the HOST's
  /// job, since the index holds no sections: run each of these searches (one
  /// per negative word, global options) for its line ids, map them to
  /// sections with SQLite (`line_toc`), intersect the per-word section sets
  /// ([blockedSections]) and drop the positive hits in them. Empty unless the
  /// scope is sameSection.
  List<SearchOptions> sectionNegativeSearches({List<int>? books}) => negativeScope == NegativeScope.sameSection
      ? [for (final w in _negativeWords()) _withOptions(SearchOptions(w, books: books), _globalSet())]
      : const [];

  /// The engine search: the query string with the global options, and per
  /// word only what differs (Otzaria's wordOptions) plus its alternatives.
  SearchOptions toOptions({int contextWords = 8}) {
    // A user regex `/.../` (Otzaria's UserRegexQuery): a valid pattern goes
    // to the engine whole, on its own (no word options, distance, negative
    // query or grouping); an invalid one is searched as words with every
    // option, as Otzaria's app does.
    if (isUserRegex(query) && SeforimIndex.userRegexKind(query) != 'invalid') {
      return SearchOptions(query.trim(), contextWords: contextWords, userRegex: true);
    }
    // `|` and `%` are FtsLib syntax in the engine's query string: an Otzaria
    // word is literal, so they are dropped from it.
    final words = [
      for (final w in splitQueryWords(query))
        if (w.replaceAll(RegExp('[|%]'), '').isNotEmpty) w.replaceAll(RegExp('[|%]'), '')
    ];
    final global = _globalSet();
    final perWord = <int, WordOptions>{};
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
      final w = _wordOverride(o, global, alts);
      if (w != null) perWord[i] = w;
    }
    final s = _withOptions(
      SearchOptions(
        [for (final w in words) _queryWord(w)].join(' '),
        words: perWord,
        contextWords: contextWords,
        exclude: _exclude(),
        phraseAlternatives: _phraseAlternatives(words, _optionsFor),
      ),
      global,
    );
    // The user's settings as they are: the engine counts the words for
    // partial matching (which wins over proximity: the match is at line
    // resolution) and cuts the words it drops (no letters or digits) out of
    // the proximity, merging their gaps. A one-letter word is a word.
    if (mode == SearchMode.advanced) s.wordMatch = wordMatch;
    if (words.length > 1) s.proximity = _proximity(words.length, customSpacing);
    return s;
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
