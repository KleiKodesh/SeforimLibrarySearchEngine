import 'package:seforim_search/seforim_search.dart';

import 'acronym_lexicon.dart';
import 'lexicon_text.dart' show foldPresentationForms;
import 'magic_dictionary.dart';
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
  static const exactAllowed = {grammaticalPrefixes, grammaticalSuffixes, fullOrDefective, partial, typoTolerance};
  static const unsupported = <String>{};
}

/// HTML entities in a typed query. A numeric entity (decimal / hex, any case,
/// `;` optional, valid code point, not a surrogate) decodes to its char, which
/// then splits or joins like any typed char (a space or comma separates, a
/// letter is a letter). A named entity is a word separator, as in the engine's
/// scanner, which treats every entity as one: the space ones (nbsp, ensp,
/// emsp, thinsp) also without `;`, any other name with its `;`.
final _entity = RegExp(
    r'&(?:#(?:x([0-9a-f]+)|([0-9]+));?|(nbsp|ensp|emsp|thinsp);?|[a-z][a-z0-9]*;)',
    caseSensitive: false);

String _decodeEntity(Match m) {
  if (m[1] == null && m[2] == null) return ' ';
  final cp = m[1] != null ? int.tryParse(m[1]!, radix: 16) : int.tryParse(m[2]!);
  if (cp == null || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return m[0]!;
  return String.fromCharCode(cp);
}

/// Otzaria's query sanitizer: numeric entities decode first and named ones
/// separate (see [_entity]); every char the engine's scanner does not keep in a word become spaces,
/// presentation forms fold to letters + marks; ״/׳ → "/', maqaf, hyphen and `|` → space
/// (Otzaria: `|` separates; the engine cuts typed words there too), collapse
/// spaces; `"` at a word edge separates, between letters it stays (רמב"ם);
/// `''` → `"`.
///
/// Paired readings `(X) [Y]` keep only Y first, by the engine's own rule
/// (Otzaria's `keep_second_of_paired_readings`).
List<String> splitQueryWords(String query) => [for (final w in splitQueryWordsWithPhrases(query)) w.$1];

/// [splitQueryWords], each word with the index of the quoted phrase it is in
/// (null: outside every phrase). Otzaria's `smartSearchQuotedPhrases` rule,
/// as the engine reads it (`hebrew_fts::quoted_phrases`): a quote (`"`,
/// gershayim, typographic double quotes) opens a phrase and the next closes
/// it, unless its previous char (nikud / te'amim skipped) and its next char
/// are both Hebrew letters (an acronym's gershayim). The typographic double
/// quotes are folded to `"` with gershayim.
List<(String, int?)> splitQueryWordsWithPhrases(String query) {
  // one char for one char, so offsets hold for the phrase spans
  final q = SeforimIndex.reducePairedReadings(foldPresentationForms(query.replaceAllMapped(_entity, _decodeEntity)))
      .replaceAll(RegExp('[״“”„]'), '"')
      .replaceAll('׳', "'")
      .replaceAll(RegExp('[־|-]'), ' ')
      // every other char the engine's scanner does not keep in a word
      // (letters, marks, ASCII digits, quotes) separates, as it does there
      .replaceAll(RegExp(r'''[^\p{L}\p{M}0-9"']''', unicode: true), ' ');
  bool letter(int? c) => c != null && c >= 0x05D0 && c <= 0x05EA;
  final spans = <(int, int)>[];
  int? start;
  int? previous;
  for (var i = 0; i < q.length; i++) {
    final c = q.codeUnitAt(i);
    final next = i + 1 < q.length ? q.codeUnitAt(i + 1) : null;
    if (c == 0x22 && !(letter(previous) && letter(next))) {
      if (start == null) {
        start = i + 1;
      } else {
        if (q.substring(start, i).trim().isNotEmpty) spans.add((start, i));
        start = null;
      }
    }
    if (!(c >= 0x0591 && c <= 0x05BD || c >= 0x05BF && c <= 0x05C7)) previous = c;
  }
  final words = <(String, int?)>[];
  for (final m in RegExp(r'\S+').allMatches(q)) {
    final w = m[0]!.replaceAll("''", '"').replaceAll(RegExp(r'^"+|"+$'), '').replaceAll(RegExp(r"^'+"), '');
    if (w.isEmpty) continue;
    int? phrase;
    for (var k = 0; k < spans.length && phrase == null; k++) {
      if (m.start < spans[k].$2 && m.end > spans[k].$1) phrase = k;
    }
    words.add((w, phrase));
  }
  return words;
}

/// [words] joined into the engine's query, each quoted phrase's run of
/// words wrapped in `"` (the engine searches it as a literal phrase; in
/// lexical fuzzy mode as Otzaria's lexical fuzzy phrase).
String joinWithPhrases(List<String> words, List<int?> phrases) {
  final out = StringBuffer();
  for (var i = 0; i < words.length; i++) {
    final p = phrases[i];
    final open = p != null && (i == 0 || phrases[i - 1] != p);
    final close = p != null && (i == words.length - 1 || phrases[i + 1] != p);
    if (i > 0) out.write(' ');
    out.write('${open ? '"' : ''}${words[i]}${close ? '"' : ''}');
  }
  return out.toString();
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
    this.magicDictionary,
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

  /// Otzaria's MagicDictionary over lexical.db; null = not loaded. As in
  /// Otzaria, only the fuzzy mode uses it: each word's dictionary forms
  /// join its slot as exact words (Otzaria's lexical TermSetQuery), the
  /// forms of a blacklisted lemma searched but not marked.
  final MagicDictionary? magicDictionary;

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
  WordOptions? _wordOverride(Set<String> own, Set<String> global, List<String> alternatives,
      [({List<String> marked, List<String> unmarked})? forms, int? cap]) {
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
      lexicalForms: forms?.marked,
      unmarkedLexicalForms: forms?.unmarked,
      maxExpandedTerms: cap,
    );
    return w.toJson().isEmpty ? null : w;
  }

  /// Word [word]'s dictionary forms (fuzzy mode with a [magicDictionary]),
  /// exact words: Otzaria's `build_lexical_fuzzy_query` recall forms
  /// minus the token and its quote-free spelling, split by its highlight
  /// forms. None for a wildcard word. A quoted word takes them too: the
  /// engine's lexical fuzzy phrase keeps each word's slot.
  ({List<String> marked, List<String> unmarked})? _magicForms(String word) {
    final dict = magicDictionary;
    if (dict == null || mode != SearchMode.fuzzy) return null;
    if (word.contains('*') || word.contains('?')) return null;
    return dict.formsFor(word);
  }

  /// Otzaria's `MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN`.
  static const _lexicalPhraseTerms = 256;

  /// A lexical fuzzy phrase word's edit-distance budget (Otzaria's
  /// `lexical_fuzzy_phrase_terms`): 256 terms per word, the word, its
  /// quote-free spelling and its dictionary forms taken first.
  int _lexicalPhraseCap(String word, ({List<String> marked, List<String> unmarked})? forms) {
    final t = indexToken(word);
    final taken = {t, t.replaceAll(RegExp('[\'"]'), ''), ...?forms?.marked, ...?forms?.unmarked};
    final left = _lexicalPhraseTerms - taken.length;
    return left > 0 ? left : 1;
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
    // sameSection: the engine drops every line of a section holding every
    // negative word (the index keeps each line's section id)
    return ExcludeOptions(words.join(' '),
        words: perWord,
        phraseAlternatives: _phraseAlternatives(words, (i) => options[i]),
        proximity: limits ? _proximity(words.length, null) : null,
        sameSection: negativeScope == NegativeScope.sameSection);
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
    final split = [
      for (final w in splitQueryWordsWithPhrases(query))
        if (w.$1.replaceAll(RegExp('[|%]'), '').isNotEmpty) (w.$1.replaceAll(RegExp('[|%]'), ''), w.$2)
    ];
    // Partial matching (any / most / at least n) is its own rule, as
    // Otzaria's `scoped_words_query`: each distinct typed word counts once
    // (a repeated word merges into its first place: the union of its
    // options and alternatives), and no distance or order applies, even
    // when n reaches the word count.
    final partial = mode == SearchMode.advanced && wordMatchMode != WordMatchMode.all;
    // typed word index -> the word indexes merged into it
    final merged = <List<int>>[];
    final keep = <(String, int?)>[];
    for (var i = 0; i < split.length; i++) {
      final at = partial && split[i].$2 == null
          ? keep.indexWhere((k) => k.$2 == null && k.$1 == split[i].$1)
          : -1;
      if (at >= 0) {
        merged[at].add(i);
      } else {
        keep.add(split[i]);
        merged.add([i]);
      }
    }
    final words = [for (final w in keep) w.$1];
    // Quoted phrases stay quoted: the engine searches each as a literal
    // phrase, or in lexical fuzzy mode as Otzaria's lexical fuzzy phrase
    // (each word's slot, in order, at most 1 word between neighbours).
    final phrases = [for (final w in keep) w.$2];
    final lexicalFuzzy = mode == SearchMode.fuzzy && magicDictionary != null;
    // Otzaria's lexical fuzzy phrase (fuzzy mode with a magic dictionary,
    // the whole query one quoted phrase): every word with its edit-distance
    // matches and dictionary forms, in order, at most 1 word between
    // neighbours (LEXICAL_FUZZY_PHRASE_SLOP), not the literal phrase.
    final lexicalPhrase = mode == SearchMode.fuzzy &&
        magicDictionary != null &&
        words.length >= 2 &&
        phrases.every((p) => p != null && p == phrases.first);
    // Otzaria's plain fuzzy query has no phrases (its builder ignores them):
    // the words are searched anywhere, quoted or not.
    final sentPhrases = lexicalPhrase || (mode == SearchMode.fuzzy && magicDictionary == null)
        ? List<int?>.filled(words.length, null)
        : phrases;
    final global = _globalSet();
    final perWord = <int, WordOptions>{};
    Set<String> optionsOf(int k) => {for (final i in merged[k]) ..._optionsFor(i)};
    for (var k = 0; k < words.length; k++) {
      final o = optionsOf(k);
      final alts = <String>[];
      void add(Iterable<String>? xs) {
        for (final x in xs ?? const <String>[]) {
          if (!alts.contains(x)) alts.add(x);
        }
      }
      for (final i in merged[k]) {
        if (mode == SearchMode.advanced) add(alternativeWords[i]);
      }
      // Otzaria translation_alternatives: appended to the word's alternatives
      if (mode == SearchMode.advanced && o.contains(OtzariaOption.aramaicTranslation)) {
        add(translations?.expansions(words[k]));
      }
      if (mode == SearchMode.advanced && o.contains(OtzariaOption.aramaicTranslation) && translations == null) {
        warnings.add('option "${OtzariaOption.aramaicTranslation}" needs --dictionary (Otzaria assets/dictionary.json)');
      }
      final forms = _magicForms(words[k]);
      // a lexical fuzzy phrase word (the whole query, or a quoted part the
      // engine requires within 1 word): Otzaria's 256-term budget
      final phraseWord = lexicalPhrase || (lexicalFuzzy && sentPhrases[k] != null);
      final w = _wordOverride(o, global, alts, forms, phraseWord ? _lexicalPhraseCap(words[k], forms) : null);
      if (w != null) perWord[k] = w;
    }
    final s = _withOptions(
      SearchOptions(
        joinWithPhrases([for (final w in words) _queryWord(w)], sentPhrases),
        words: perWord,
        contextWords: contextWords,
        exclude: _exclude(),
        phraseAlternatives: _phraseAlternatives(words, optionsOf),
      ),
      global,
    );
    // The engine counts the words for partial matching and cuts the words
    // it drops (no letters or digits) out of the proximity, merging their
    // gaps. A one-letter word is a word. A partial mode sends no proximity.
    if (mode == SearchMode.advanced) s.wordMatch = wordMatch;
    // Otzaria's fuzzy query has no distance: its words anywhere in the
    // line (its lexical phrase: in order, at most 1 word between).
    if (lexicalPhrase) {
      s.proximity = Proximity.ordered(List.filled(words.length - 1, 1));
    } else if (words.length > 1 && !partial && mode != SearchMode.fuzzy) {
      s.proximity = _proximity(words.length, customSpacing);
    }
    // the fuzzy words ranked as Otzaria's lexical fuzzy query
    s.lexicalFuzzy = lexicalFuzzy;
    // the negative query's default: Otzaria's distance and any order (its
    // negative custom spacing defaults to none)
    if (mode == SearchMode.advanced) s.proximityBase = (distance: distance, anyOrder: anyOrder);
    return s;
  }
}
