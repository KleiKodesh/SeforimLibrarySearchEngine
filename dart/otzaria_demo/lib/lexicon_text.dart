// Text rules shared by the host lexicons ([AcronymLexicon],
// [TranslationLexicon]) and the query adapter.

/// Otzaria `fold_presentation_form` (otzaria_search_engine v5.0.0,
/// `hebrew_query.rs`, generated from its table): Hebrew presentation forms
/// U+FB1D..U+FB4F as their letters (+ marks); U+FB29 is not folded.
const presentationForms = <int, String>{
  0xFB1D: '\u05D9\u05B4',
  0xFB1E: '',
  0xFB1F: '\u05F2\u05B7',
  0xFB20: '\u05E2',
  0xFB21: '\u05D0',
  0xFB22: '\u05D3',
  0xFB23: '\u05D4',
  0xFB24: '\u05DB',
  0xFB25: '\u05DC',
  0xFB26: '\u05DD',
  0xFB27: '\u05E8',
  0xFB28: '\u05EA',
  0xFB2A: '\u05E9\u05C1',
  0xFB2B: '\u05E9\u05C2',
  0xFB2C: '\u05E9\u05BC\u05C1',
  0xFB2D: '\u05E9\u05BC\u05C2',
  0xFB2E: '\u05D0\u05B7',
  0xFB2F: '\u05D0\u05B8',
  0xFB30: '\u05D0\u05BC',
  0xFB31: '\u05D1\u05BC',
  0xFB32: '\u05D2\u05BC',
  0xFB33: '\u05D3\u05BC',
  0xFB34: '\u05D4\u05BC',
  0xFB35: '\u05D5\u05BC',
  0xFB36: '\u05D6\u05BC',
  0xFB38: '\u05D8\u05BC',
  0xFB39: '\u05D9\u05BC',
  0xFB3A: '\u05DA\u05BC',
  0xFB3B: '\u05DB\u05BC',
  0xFB3C: '\u05DC\u05BC',
  0xFB3E: '\u05DE\u05BC',
  0xFB40: '\u05E0\u05BC',
  0xFB41: '\u05E1\u05BC',
  0xFB43: '\u05E3\u05BC',
  0xFB44: '\u05E4\u05BC',
  0xFB46: '\u05E6\u05BC',
  0xFB47: '\u05E7\u05BC',
  0xFB48: '\u05E8\u05BC',
  0xFB49: '\u05E9\u05BC',
  0xFB4A: '\u05EA\u05BC',
  0xFB4B: '\u05D5\u05B9',
  0xFB4C: '\u05D1\u05BF',
  0xFB4D: '\u05DB\u05BF',
  0xFB4E: '\u05E4\u05BF',
  0xFB4F: '\u05D0\u05DC',
};

/// [text] with every presentation form folded (Otzaria
/// `fold_presentation_forms`).
String foldPresentationForms(String text) {
  if (!text.runes.any(presentationForms.containsKey)) return text;
  final b = StringBuffer();
  for (final r in text.runes) {
    final f = presentationForms[r];
    f == null ? b.writeCharCode(r) : b.write(f);
  }
  return b.toString();
}

/// Otzaria `normalize_for_index` for lexicon words: presentation forms
/// folded, attached nikud and te'amim (U+0591..U+05C7 except maqaf, paseq,
/// sof pasuq, nun hafukha) and general combining marks dropped,
/// lower-cased. Plus (ours): gershayim U+05F4 and geresh U+05F3 as the ASCII
/// `"` and `'`, the form the query sanitizer gives every typed word, so a
/// dictionary key written either way is reachable from either.
String normalizeLexiconWord(String text) {
  final b = StringBuffer();
  for (final r in foldPresentationForms(text).runes) {
    final attached = r >= 0x0591 && r <= 0x05C7 && r != 0x05BE && r != 0x05C0 && r != 0x05C3 && r != 0x05C6;
    final combining = r >= 0x0300 && r <= 0x036F;
    if (attached || combining) continue;
    b.writeCharCode(r == 0x05F4 ? 0x22 : (r == 0x05F3 ? 0x27 : r));
  }
  return b.toString().toLowerCase();
}

/// [map]'s keys in Otzaria's load order: serde_json without
/// `preserve_order` reads a JSON object into a BTreeMap, so keys come
/// sorted (by code point), whatever the file's order. The caps and the
/// reverse lists depend on this order, so it must not follow the file.
List<String> sortedKeys(Map<dynamic, dynamic> map) =>
    [for (final k in map.keys) if (k is String) k]..sort(_byCodePoint);

int _byCodePoint(String a, String b) {
  final x = a.runes.toList(), y = b.runes.toList();
  for (var i = 0; i < x.length && i < y.length; i++) {
    if (x[i] != y[i]) return x[i] - y[i];
  }
  return x.length - y.length;
}
