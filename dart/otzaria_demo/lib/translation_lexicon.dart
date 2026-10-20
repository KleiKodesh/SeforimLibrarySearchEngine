import 'dart:convert';
import 'dart:io';

/// Port of Otzaria's `TranslationLexicon` (otzaria_search_engine v5.0.0,
/// `rust/src/lexicons.rs`): Aramaic↔Hebrew single-word translations from the
/// app's `assets/dictionary.json`, both directions, for the "תרגום ארמי"
/// option. The expansions become word alternatives (an OR group), exactly as
/// Otzaria injects them into `alternative_words`.
///
/// This is host-side on purpose: the dictionary is an Otzaria app asset, not
/// engine logic.
class TranslationLexicon {
  TranslationLexicon._(this._forward, this._reverse);

  /// Otzaria `MAX_TRANSLATION_EXPANSIONS`: per word, both directions together.
  static const maxExpansions = 16;

  final Map<String, List<String>> _forward;
  final Map<String, List<String>> _reverse;

  /// Loads `dictionary.json`: `{dictionaryName: [{headword: gloss}, ...]}`.
  /// Every dictionary is merged (Otzaria's regression: not only the first).
  static TranslationLexicon load(String path) => parse(File(path).readAsStringSync());

  static TranslationLexicon parse(String json) {
    final top = jsonDecode(json);
    if (top is! Map<String, dynamic>) {
      throw const FormatException('translation dictionary: expected a top-level {name: [entries]} object');
    }
    final forward = <String, List<String>>{}, reverse = <String, List<String>>{};
    for (final list in top.values) {
      if (list is! List) continue;
      for (final entry in list) {
        if (entry is! Map) continue;
        entry.forEach((headword, gloss) {
          if (headword is! String || gloss is! String) return;
          final head = _normalize(headword.trim());
          if (head.isEmpty) return;
          for (final word in _glossWords(gloss)) {
            _pushCapped(forward, head, word);
            _pushCapped(reverse, word, head);
          }
        });
      }
    }
    return TranslationLexicon._(forward, reverse);
  }

  /// The word's translations in both directions (Aramaic→Hebrew, then
  /// Hebrew→Aramaic), dictionary order, without the word itself, up to [cap].
  List<String> expansions(String token, [int cap = maxExpansions]) {
    final key = _normalize(token.trim());
    final out = <String>[];
    for (final list in [_forward[key], _reverse[key]]) {
      if (list == null) continue;
      for (final w in list) {
        if (w != key && !out.contains(w)) {
          out.add(w);
          if (out.length >= cap) return out;
        }
      }
    }
    return out;
  }

  /// Head words loaded (for logs/tests).
  int get length => _forward.length;

  static void _pushCapped(Map<String, List<String>> map, String key, String value) {
    if (key == value) return;
    final list = map.putIfAbsent(key, () => []);
    if (list.length < maxExpansions && !list.contains(value)) list.add(value);
  }

  /// Otzaria `gloss_translation_words`: drop `{...}` and `(...)` blocks, split
  /// on `***` and `, ; . :`, keep single-word items only.
  static List<String> _glossWords(String gloss) {
    final cleaned = StringBuffer();
    var curly = 0, paren = 0;
    for (final c in gloss.runes.map(String.fromCharCode)) {
      switch (c) {
        case '{':
          curly++;
        case '}':
          if (curly > 0) curly--;
        case '(':
          paren++;
        case ')':
          if (paren > 0) paren--;
        default:
          if (curly == 0 && paren == 0) cleaned.write(c);
      }
    }
    final out = <String>[];
    for (final meaning in cleaned.toString().split('***')) {
      for (final piece in meaning.split(RegExp('[,;.:]'))) {
        final word = piece.trim();
        if (word.isEmpty || word.split(RegExp(r'\s+')).length != 1) continue;
        final n = _normalize(word);
        if (n.isNotEmpty && !out.contains(n)) out.add(n);
      }
    }
    return out;
  }

  /// Otzaria `normalize_for_index` for dictionary words: attached nikud and
  /// te'amim (U+0591–U+05C7 except maqaf, paseq, sof pasuq, nun hafukha) and
  /// general combining marks dropped, lower-cased.
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
