import 'package:seforim_search/seforim_search.dart';

/// Otzaria's search modes (`SearchMode`).
enum SearchMode { exact, advanced, fuzzy }

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
  static const aramaicTranslation = 'תרגום ארמי'; // not supported by the engine
  static const acronyms = 'ראשי תיבות'; // not supported by the engine
  static const nikud = 'ניקוד';
  static const teamim = 'טעמים';

  /// Options exact mode allows (Otzaria `exactWordOptionKeys`).
  static const exactAllowed = {grammaticalPrefixes, grammaticalSuffixes, prefixes, suffixes, fullOrDefective};
  static const unsupported = {aramaicTranslation, acronyms};
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

bool _isAcronym(String w) => w.contains('"');
int _letters(String w) => w.replaceAll(RegExp('["\']'), '').length;

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
    this.anyWord = false,
    this.prefixMaxLetters,
    this.suffixMaxLetters,
    this.partOfWordMaxLetters,
    this.grammarPrefixMaxLetters,
    this.marksIndex = false,
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

  /// wordMatchMode = anyWord: one OR slot of all words.
  final bool anyWord;

  final int? prefixMaxLetters, suffixMaxLetters, partOfWordMaxLetters, grammarPrefixMaxLetters;

  /// The index was built with keepMarks (nikud/te'amim kept in terms).
  final bool marksIndex;

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
    // Otzaria: grammatical affixes skip words under 3 letters, except acronyms.
    final grammarOk = _letters(w) >= 3 || _isAcronym(w);
    return QueryGroup.word(
      w,
      WalkOptions(
        grammarPrefixes: grammarOk && o.contains(OtzariaOption.grammaticalPrefixes),
        grammarSuffixes: grammarOk && o.contains(OtzariaOption.grammaticalSuffixes),
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

  /// The engine request.
  SearchRequest toRequest({int contextWords = 8}) {
    final words = splitQueryWords(query);
    final groups = <QueryGroup>[];
    for (var i = 0; i < words.length; i++) {
      final o = _optionsFor(i);
      final alts = mode == SearchMode.advanced ? (alternativeWords[i] ?? const []) : const <String>[];
      final main = _wordGroup(words[i], o);
      groups.add(alts.isEmpty ? main : QueryGroup.any([main, for (final a in alts) _wordGroup(a, o)]));
    }
    if (anyWord && groups.length > 1) {
      return SearchRequest(groups: [QueryGroup.any(groups)], contextWords: contextWords);
    }
    return SearchRequest(
      groups: groups,
      maxWordDistance: groups.length > 1 ? distance : null,
      requireOrdered: !anyOrder && groups.length > 1,
      contextWords: contextWords,
    );
  }
}
