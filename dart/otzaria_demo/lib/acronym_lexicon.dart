import 'dart:convert';
import 'dart:io';

import 'otzaria_query.dart' show splitQueryWords;

/// Port of Otzaria's `AcronymLexicon` (otzaria_search_engine v5.0.0,
/// `rust/src/lexicons.rs`) over the app's `assets/Acronyms.json`
/// (`{"רמב\"ם": ["רבי משה בן מיימון"], ...}`), for the "ראשי תיבות" option.
/// Keys are kept quote-free (רמבם); single-word expansions are skipped.
///
/// Host-side on purpose: the dictionary is an Otzaria app asset. The engine
/// only gets the result, as `SearchRequest.phraseAlternatives`.
class AcronymLexicon {
  AcronymLexicon._(this._forward, this._reverse);

  /// Otzaria `MAX_ACRONYM_EXPANSIONS`.
  static const maxExpansions = 16;

  final Map<String, List<List<String>>> _forward;
  final Map<String, List<String>> _reverse;

  static AcronymLexicon load(String path) => parse(File(path).readAsStringSync());

  static AcronymLexicon parse(String json) {
    final top = jsonDecode(json);
    if (top is! Map<String, dynamic>) {
      throw const FormatException('acronyms dictionary: expected a top-level {acronym: [expansions]} object');
    }
    final forward = <String, List<List<String>>>{}, reverse = <String, List<String>>{};
    top.forEach((acronym, expansions) {
      final key = _stripQuotes(_normalize(acronym));
      if (key.isEmpty || expansions is! List) return;
      for (final e in expansions) {
        if (e is! String) continue;
        final words = splitQueryWords(_normalize(e));
        if (words.length < 2) continue; // a one-word expansion adds nothing
        final list = forward.putIfAbsent(key, () => []);
        if (list.length < maxExpansions && !list.any((w) => _same(w, words))) list.add(words);
        final joined = words.join(' ');
        if (joined != key) {
          final r = reverse.putIfAbsent(joined, () => []);
          if (r.length < maxExpansions && !r.contains(key)) r.add(key);
        }
      }
    });
    return AcronymLexicon._(forward, reverse);
  }

  /// Acronym → its expansions, each a word list, up to [cap].
  List<List<String>> expand(String token, [int cap = maxExpansions]) =>
      (_forward[_stripQuotes(token)] ?? const []).take(cap).toList();

  /// Expansion words → the (quote-free) acronyms they open, up to [cap].
  List<String> acronymsFor(List<String> words, [int cap = maxExpansions]) =>
      words.length < 2 ? const [] : (_reverse[words.join(' ')] ?? const []).take(cap).toList();

  int get length => _forward.length;

  /// Otzaria `acronym_alternatives`: only when the WHOLE query is one
  /// semantic unit. A lone acronym → its expansions; a query that is a known
  /// expansion → its acronyms (one-word phrases).
  List<List<String>> alternativesFor(List<String> queryWords) {
    final words = [for (final w in queryWords) _normalize(w)];
    return [
      if (words.length == 1) ...expand(words.first),
      for (final a in acronymsFor(words)) [a],
    ];
  }

  static bool _same(List<String> a, List<String> b) =>
      a.length == b.length && Iterable.generate(a.length).every((i) => a[i] == b[i]);

  static String _stripQuotes(String s) => s.replaceAll(RegExp('["\']'), '');

  /// Otzaria `normalize_for_index`: attached marks dropped, lower-cased.
  static String _normalize(String text) {
    final b = StringBuffer();
    for (final r in text.runes) {
      final attached = r >= 0x0591 && r <= 0x05C7 && r != 0x05BE && r != 0x05C0 && r != 0x05C3 && r != 0x05C6;
      final combining = r >= 0x0300 && r <= 0x036F;
      if (!attached && !combining && r != 0xFB1E) b.writeCharCode(r);
    }
    return b.toString().toLowerCase();
  }
}
