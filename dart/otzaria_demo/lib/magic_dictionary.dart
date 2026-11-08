import 'dart:collection';

import 'package:sqlite3/sqlite3.dart';

import 'lexicon_text.dart' show normalizeLexiconWord;
import 'magic_blacklist.dart';

/// Port of Otzaria's `MagicDictionary` (otzaria_search_engine 0d7a33f,
/// `rust/src/magic/{mod,dictionary,normalize,blacklist}.rs`): morphological
/// forms of a query word from the offline-built `lexical.db` (tables `base`,
/// `surface`, `variant`, `surface_variant`), read-only.
///
/// Host-side on purpose: lexical.db is an Otzaria app asset, not engine
/// logic. The forms reach the engine as word alternatives: Otzaria searches
/// every form but highlights only those outside the hallucination blacklist,
/// so the blacklisted ones go as `unmarkedAlternatives` (see [formsFor]).
/// Otzaria's downloader stays app code: this class takes a path.
class MagicDictionary {
  MagicDictionary._(this._db);

  /// Otzaria `MAX_LEXICAL_FORMS`: forms per query word.
  static const maxLexicalForms = 32;
  static const _primaryFamilyCap = 24;
  static const _secondaryFamilyCap = 8;
  static const _variantRouteCap = 4;
  static const _minFormLetters = 2;
  static const _minVariantSurfaceLetters = 3;
  static const _surfaceRouteRank = 10;

  static const _baseByValueSql = 'SELECT id, value FROM base WHERE value = ?1';
  static const _baseBySurfaceSql =
      'SELECT b.id, b.value FROM surface s JOIN base b ON b.id = s.base_id WHERE s.value = ?1';
  static const _variantSurfacesSql = 'SELECT s.base_id, s.id, s.value FROM variant v '
      'JOIN surface_variant sv ON sv.variant_id = v.id JOIN surface s ON s.id = sv.surface_id '
      'WHERE v.value = ?1';
  static const _baseValueSql = 'SELECT value FROM base WHERE id = ?1';
  static const _surfacesSql = 'SELECT value FROM surface WHERE base_id = ?1 ORDER BY id';
  static const _variantsSql = 'SELECT v.value FROM surface s '
      'JOIN surface_variant sv ON sv.surface_id = s.id JOIN variant v ON v.id = sv.variant_id '
      'WHERE s.base_id = ?1 ORDER BY s.id, sv.variant_id';

  final Database _db;
  final Map<String, PreparedStatement> _stmts = {};

  /// Otzaria's LRU of selected families (512 on desktop).
  static const _cacheSize = 512;
  final LinkedHashMap<String, List<_Family>> _cache = LinkedHashMap();

  /// Opens [path] read-only; throws when it is not a lexical.db (Otzaria's
  /// `MagicDictionary::open`: every statement must prepare).
  static MagicDictionary open(String path) {
    final db = sqlite3.open(path, mode: OpenMode.readOnly);
    try {
      db.execute('PRAGMA query_only = ON;');
      final dict = MagicDictionary._(db);
      for (final sql in [
        _baseByValueSql,
        _baseBySurfaceSql,
        _variantSurfacesSql,
        _baseValueSql,
        _surfacesSql,
        _variantsSql
      ]) {
        dict._stmt(sql);
      }
      return dict;
    } catch (_) {
      db.close();
      rethrow;
    }
  }

  /// [open], or null when the file is missing or not a lexical.db (Otzaria's
  /// `set_magic_dictionary_path` returning false: no dictionary, no forms).
  static MagicDictionary? tryOpen(String path) {
    try {
      return open(path);
    } catch (_) {
      return null;
    }
  }

  PreparedStatement _stmt(String sql) => _stmts[sql] ??= _db.prepare(sql);

  void close() {
    for (final s in _stmts.values) {
      s.close();
    }
    _stmts.clear();
    _db.close();
  }

  /// Every form of [token] (Otzaria `recall_forms`): searched.
  List<String> recallForms(String token, [int cap = maxLexicalForms]) => _collect(token, cap, false);

  /// The forms outside blacklisted families (Otzaria `highlight_forms`): painted.
  List<String> highlightForms(String token, [int cap = maxLexicalForms]) => _collect(token, cap, true);

  /// The forms the engine gets for a query [token], as Otzaria's lexical
  /// fuzzy query injects them: every recall form but the token itself and
  /// its quote-free spelling; `marked` the highlighted ones, `unmarked` the
  /// rest (searched, not highlighted).
  ({List<String> marked, List<String> unmarked}) formsFor(String token) {
    final t = indexToken(token);
    final clean = t.replaceAll(RegExp('[\'"]'), '');
    final recall = recallForms(t).where((f) => f != t && f != clean).toList();
    final highlight = highlightForms(t).toSet();
    return (
      marked: [for (final f in recall) if (highlight.contains(f)) f],
      unmarked: [for (final f in recall) if (!highlight.contains(f)) f],
    );
  }

  List<String> _collect(String token, int cap, bool applyBlacklist) {
    if (cap > maxLexicalForms) cap = maxLexicalForms;
    final out = <String>[];
    for (final f in _familiesFor(token)) {
      if (applyBlacklist && isBlacklisted(token, f.base)) continue;
      for (final t in f.terms) {
        if (out.length >= cap) return out;
        out.add(t);
      }
    }
    return out;
  }

  List<_Family> _familiesFor(String token) {
    final key = canonicalKey(token);
    if (key.isEmpty) return const [];
    final hit = _cache.remove(key);
    if (hit != null) {
      _cache[key] = hit;
      return hit;
    }
    final keys = lookupKeys(token);
    List<_Family> families;
    try {
      // one read snapshot (Otzaria: unchecked_transaction + commit)
      _db.execute('BEGIN');
      try {
        families = _selectFamilies(token, keys);
      } finally {
        _db.execute('COMMIT');
      }
    } catch (_) {
      return const []; // an error: no forms, not cached
    }
    _cache[key] = families;
    if (_cache.length > _cacheSize) _cache.remove(_cache.keys.first);
    return families;
  }

  List<_Family> _selectFamilies(String token, List<String> keys) {
    final lemmas = _matchLemmas(keys);
    final picked = _Picked();
    final families = <_Family>[];
    for (final (rank, baseId, base) in lemmas) {
      final familyCap = rank < _surfaceRouteRank ? _primaryFamilyCap : _secondaryFamilyCap;
      final cap = familyCap < picked.room ? familyCap : picked.room;
      if (cap == 0) break;
      final terms = <String>[];
      if (!picked.offer(terms, base, cap) && !_streamInto(_surfacesSql, baseId, picked, terms, cap)) {
        _streamInto(_variantsSql, baseId, picked, terms, cap);
      }
      if (terms.isNotEmpty) families.add(_Family(base, terms));
    }
    if (picked.room > 0) {
      final reached = {for (final l in lemmas) l.$2};
      _selectVariantRoute(token, keys, reached, picked, families);
    }
    return families;
  }

  /// (route rank + key position, base id, base value), one per base, best rank first.
  List<(int, int, String)> _matchLemmas(List<String> keys) {
    final lemmas = <(int, int, String)>[];
    for (final (routeRank, sql) in [(0, _baseByValueSql), (_surfaceRouteRank, _baseBySurfaceSql)]) {
      final stmt = _stmt(sql);
      for (var position = 0; position < keys.length; position++) {
        for (final row in stmt.select([keys[position]])) {
          lemmas.add((routeRank + position, row.columnAt(0) as int, row.columnAt(1) as String));
        }
      }
    }
    // keep each base's best rank, then order by (rank, id)
    lemmas.sort((a, b) => a.$2 != b.$2 ? a.$2.compareTo(b.$2) : a.$1.compareTo(b.$1));
    final dedup = <(int, int, String)>[];
    for (final l in lemmas) {
      if (dedup.isEmpty || dedup.last.$2 != l.$2) dedup.add(l);
    }
    dedup.sort((a, b) => a.$1 != b.$1 ? a.$1.compareTo(b.$1) : a.$2.compareTo(b.$2));
    return dedup;
  }

  bool _streamInto(String sql, int baseId, _Picked picked, List<String> terms, int cap) {
    for (final row in _stmt(sql).select([baseId])) {
      if (picked.offer(terms, row.columnAt(0) as String, cap)) return true;
    }
    return false;
  }

  void _selectVariantRoute(
      String token, List<String> keys, Set<int> reached, _Picked picked, List<_Family> families) {
    final probe = StemProbe(token);
    final matched = <(int, int, String)>[];
    final stmt = _stmt(_variantSurfacesSql);
    for (final key in keys) {
      for (final row in stmt.select([key])) {
        final baseId = row.columnAt(0) as int;
        if (reached.contains(baseId)) continue;
        final surface = row.columnAt(2) as String;
        if (hebrewLetterCount(surface) >= _minVariantSurfaceLetters && probe.matches(surface)) {
          matched.add((baseId, row.columnAt(1) as int, surface));
        }
      }
    }
    matched.sort((a, b) => a.$1 != b.$1 ? a.$1.compareTo(b.$1) : a.$2.compareTo(b.$2));
    final dedup = <(int, int, String)>[];
    for (final m in matched) {
      if (dedup.isEmpty || dedup.last.$2 != m.$2) dedup.add(m);
    }
    var i = 0;
    while (i < dedup.length) {
      var j = i;
      while (j < dedup.length && dedup[j].$1 == dedup[i].$1) {
        j++;
      }
      final cap = _variantRouteCap < picked.room ? _variantRouteCap : picked.room;
      if (cap == 0) break;
      final terms = <String>[];
      for (var k = i; k < j; k++) {
        if (picked.offer(terms, dedup[k].$3, cap)) break;
      }
      if (terms.isNotEmpty) {
        final base = _stmt(_baseValueSql).select([dedup[i].$1]).first.columnAt(0) as String;
        families.add(_Family(base, terms));
      }
      i = j;
    }
  }
}

class _Family {
  _Family(this.base, this.terms);
  final String base;
  final List<String> terms;
}

/// Otzaria `Picked`: the forms taken so far across families (deduplicated
/// index terms), under the per-word total.
class _Picked {
  final Set<String> seen = {};

  int get room {
    final r = MagicDictionary.maxLexicalForms - seen.length;
    return r < 0 ? 0 : r;
  }

  /// Takes [form] into [terms] while under [cap]; true once [terms] is full.
  bool offer(List<String> terms, String form, int cap) {
    if (terms.length < cap && hebrewLetterCount(form) >= MagicDictionary._minFormLetters) {
      final term = toIndexTerm(form);
      if (term != null && seen.add(term)) terms.add(term);
    }
    return terms.length >= cap;
  }
}

// ── normalize.rs ────────────────────────────────────────────────────────────

const _finals = {0x05DA: 0x05DB, 0x05DD: 0x05DE, 0x05DF: 0x05E0, 0x05E3: 0x05E4, 0x05E5: 0x05E6};
final _toFinal = {for (final e in _finals.entries) e.value: e.key};

/// Hebrew prefix letters (vav bet kaf lamed mem shin he dalet).
const _prefixLetters = {0x05D5, 0x05D1, 0x05DB, 0x05DC, 0x05DE, 0x05E9, 0x05D4, 0x05D3};
const _maxPrefixLetters = 3;
const _minStemLetters = 3;

/// The points SeforimMagicIndexer removes: te'amim, vowels + meteg, shin /
/// sin dots, qamatz qatan (rafe U+05BF kept).
bool _isRemovedPoint(int c) =>
    (c >= 0x0591 && c <= 0x05AF) || (c >= 0x05B0 && c <= 0x05BD) || c == 0x05C1 || c == 0x05C2 || c == 0x05C7;

String _collapse(String s) => s.split(RegExp(r'\s+')).where((w) => w.isNotEmpty).join(' ');

String _finalizeWord(String word) {
  if (word.isEmpty) return word;
  final runes = word.runes.toList();
  final f = _toFinal[runes.last];
  if (f != null) runes[runes.length - 1] = f;
  return String.fromCharCodes(runes);
}

String _finalizeWords(String s) => s.split(' ').map(_finalizeWord).join(' ');

/// Otzaria `canonical_key`: points stripped, maqaf as a space, finals and
/// quotes as written, whitespace collapsed.
String canonicalKey(String s) {
  final out = StringBuffer();
  for (final c in s.trim().runes) {
    if (_isRemovedPoint(c)) continue;
    out.writeCharCode(c == 0x05BE ? 0x20 : c);
  }
  return _collapse(out.toString());
}

/// Otzaria `lookup_keys`: up to five distinct keys, most literal first.
List<String> lookupKeys(String token) {
  final asWritten = canonicalKey(token);
  final finalized = _finalizeWords(asWritten);
  String? otherQuotes;
  if (finalized.contains('"')) {
    otherQuotes = finalized.replaceAll('"', '״');
  } else if (finalized.contains('״')) {
    otherQuotes = finalized.replaceAll('״', '"');
  }
  final unquoted = finalized.replaceAll(RegExp('["\'״׳]'), '');
  final keys = <String>[];
  for (final k in [asWritten, finalized, otherQuotes, _finalizeWords(unquoted), normalizeHebrew(token)]) {
    if (k != null && k.isNotEmpty && !keys.contains(k)) keys.add(k);
  }
  return keys;
}

/// Hebrew letters (alef..tav, finals included) in [s].
int hebrewLetterCount(String s) => s.runes.where((c) => c >= 0x05D0 && c <= 0x05EA).length;

/// Otzaria `normalize_hebrew`: points and quotes dropped, maqaf a space,
/// finals folded, whitespace collapsed.
String normalizeHebrew(String s) {
  final out = StringBuffer();
  for (final c in s.trim().runes) {
    if (_isRemovedPoint(c) || c == 0x05F4 || c == 0x05F3 || c == 0x22 || c == 0x27) continue;
    out.writeCharCode(c == 0x05BE ? 0x20 : (_finals[c] ?? c));
  }
  return _collapse(out.toString());
}

/// The prefix-stripped readings of a folded word (Otzaria `stems`).
Iterable<List<int>> _stems(List<int> word) sync* {
  var deepest = 0;
  while (deepest < _maxPrefixLetters &&
      word.length > deepest + _minStemLetters &&
      _prefixLetters.contains(word[deepest])) {
    deepest++;
  }
  for (var d = 0; d <= deepest; d++) {
    yield word.sublist(d);
  }
}

bool _startsWith(List<int> s, List<int> head) {
  if (s.length < head.length) return false;
  for (var i = 0; i < head.length; i++) {
    if (s[i] != head[i]) return false;
  }
  return true;
}

bool _containsRun(List<int> s, List<int> w) {
  if (w.isEmpty) return true; // Rust `windows(0)` panics; never reached (non-empty word)
  for (var i = 0; i + w.length <= s.length; i++) {
    var ok = true;
    for (var k = 0; k < w.length; k++) {
      if (s[i + k] != w[k]) {
        ok = false;
        break;
      }
    }
    if (ok) return true;
  }
  return false;
}

/// Otzaria `StemProbe`: the shared-stem check of the variant route.
class StemProbe {
  StemProbe(String word) : _word = normalizeHebrew(word).runes.toList();
  final List<int> _word;

  bool matches(String surface) {
    final s = normalizeHebrew(surface).runes.toList();
    if (_word.isEmpty || s.isEmpty) return false;
    for (final w in _stems(_word)) {
      final head = w.sublist(0, w.length < 2 ? w.length : 2);
      if (_containsRun(s, w) || _stems(s).any((st) => _startsWith(st, head))) return true;
    }
    return false;
  }
}

/// Otzaria `to_index_term`: a DB form as the index spells it (normalized,
/// quotes ASCII, last letter final), or null when empty or multi-word.
String? toIndexTerm(String form) {
  final words = normalizeLexiconWord(form).split(RegExp(r'\s+')).where((w) => w.isNotEmpty).toList();
  if (words.length != 1) return null;
  final term = _finalizeWord(words.first.replaceAll('״', '"').replaceAll('׳', "'"));
  return term.isEmpty ? null : term;
}

/// A typed word as Otzaria's index token (the lookup key it passes):
/// normalized, quotes ASCII.
String indexToken(String word) => normalizeLexiconWord(word.trim());

// ── blacklist.rs ────────────────────────────────────────────────────────────

final Set<String> _blacklist = () {
  final set = <String>{};
  for (var line in magicBlacklistTsv.split('\n')) {
    line = line.trim();
    if (line.isEmpty || line.startsWith('#')) continue;
    final cols = line.split('\t');
    if (cols.length < 2) continue;
    final token = normalizeHebrew(cols[0]), base = normalizeHebrew(cols[1]);
    if (token.isNotEmpty && base.isNotEmpty) set.add('$token\t$base');
  }
  return set;
}();

/// Otzaria `is_blacklisted`: a known hallucinated (token, base) pair.
bool isBlacklisted(String token, String base) => _blacklist.contains('${normalizeHebrew(token)}\t${normalizeHebrew(base)}');
