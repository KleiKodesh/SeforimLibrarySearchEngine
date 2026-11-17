// Otzaria's text and query helpers (otzaria_search_engine 7.2.0, ported into the
// engine byte for byte): the same names and shapes as the old package's API, so an
// app switches by changing its import.
import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'native.dart';

SeforimNative get _n => SeforimNative.instance;

dynamic _text(Map<String, Object?> request) => using((a) {
      final (r, rl) = utf8Native(jsonEncode(request), a);
      return jsonDecode(_n.takeString(_n.textHelper(r, rl)));
    });

/// Otzaria's `sanitizeQuery`: the query normalised exactly as the engine reads it.
String sanitizeQuery(String query) => _text({'op': 'sanitize_query', 'query': query}) as String;

/// Otzaria's `splitQueryWords`: the query's words as the engine tokenizes them (the
/// words per-word options are keyed by, `"{word}_{index}"`).
List<String> splitQueryWords(String query) =>
    (_text({'op': 'split_query_words', 'query': query}) as List).cast<String>();

/// One query word and its source range (Otzaria's `QueryWordSpan`, UTF-16 units).
final class QueryWordSpan {
  const QueryWordSpan(this.word, this.start, this.end);
  final String word;
  final int start;
  final int end;
}

/// Otzaria's `queryWordSpans`: [splitQueryWords] with each word's source selection; a
/// paired reading `(X) [Y]` contributes only Y.
List<QueryWordSpan> queryWordSpans(String query) => [
      for (final s in _text({'op': 'query_word_spans', 'query': query}) as List)
        QueryWordSpan(s[0] as String, s[1] as int, s[2] as int)
    ];

/// Otzaria's `generateLiteralHighlightPattern`: the regex of a literal in-book search
/// (compile with `RegExp(pattern, caseSensitive: false, unicode: true)`); null for a
/// whitespace-only query.
String? generateLiteralHighlightPattern(String query) =>
    _text({'op': 'literal_highlight_pattern', 'query': query}) as String?;

/// Otzaria's `UserRegexKind`.
enum UserRegexKind { invalid, singleWord, wordSequence, crossSpace }

/// Otzaria's `UserRegexPlan` of a `/.../` query.
final class UserRegexPlan {
  const UserRegexPlan(this.kind, this.pattern, this.words, this.reason);
  final UserRegexKind kind;

  /// The whole folded pattern; an edge `^` / `$` reads `\A` / `\z`.
  final String pattern;
  final List<String> words;
  final String? reason;
}

/// Otzaria's `parseUserRegex`.
UserRegexPlan parseUserRegex(String raw) {
  final j = _text({'op': 'user_regex_plan', 'raw': raw}) as Map<String, dynamic>;
  final kind = switch (j['kind'] as String) {
    'single_word' => UserRegexKind.singleWord,
    'word_sequence' => UserRegexKind.wordSequence,
    'cross_space' => UserRegexKind.crossSpace,
    _ => UserRegexKind.invalid,
  };
  return UserRegexPlan(kind, j['pattern'] as String, (j['words'] as List).cast<String>(), j['reason'] as String?);
}

/// A UTF-16 range (Otzaria's `HighlightRange`).
final class HighlightRange {
  const HighlightRange(this.start, this.end);
  final int start;
  final int end;
}

/// One match (Otzaria's `HighlightMatch`): word ranges relative to [start].
final class HighlightMatch {
  const HighlightMatch(this.start, this.end, this.wordRanges);
  final int start;
  final int end;
  final List<HighlightRange> wordRanges;
}

/// A prepared native display matcher: Otzaria's `HighlightMatcher` and `UserRegexMatcher`.
/// Freed with the object (a native finalizer).
final class HighlightMatcher implements Finalizable {
  HighlightMatcher._(this._handle, this._crossLine) {
    _n.matcherFinalizer.attach(this, _handle);
  }

  final Pointer<Void> _handle;
  final bool _crossLine;

  /// Whether a display caller may join adjacent lines to complete this pattern.
  bool allowsCrossLine() => _crossLine;

  List<HighlightMatch> _find(String data, List<bool> bounds, int words) => using((a) {
        final (d, dl) = utf8Native(data, a);
        final b = a<Uint8>(bounds.isEmpty ? 1 : bounds.length);
        for (var i = 0; i < bounds.length; i++) {
          b[i] = bounds[i] ? 1 : 0;
        }
        final j = jsonDecode(_n.takeString(_n.matcherFind(_handle, d, dl, words, b, bounds.length))) as List;
        return [
          for (final m in j)
            HighlightMatch(m[0] as int, m[1] as int,
                [for (final r in m[2] as List) HighlightRange(r[0] as int, r[1] as int)])
        ];
      });

  /// Phrase matches in displayed text (markup included), UTF-16 offsets.
  List<HighlightMatch> findMatches(String data, {List<bool> requireTokenBoundaries = const []}) =>
      _find(data, requireTokenBoundaries, 0);

  /// Each word's matches on its own.
  List<HighlightMatch> findWordMatches(String data, {List<bool> requireTokenBoundaries = const []}) =>
      _find(data, requireTokenBoundaries, 1);
}

/// Otzaria's `HighlightPattern`.
final class HighlightPattern {
  const HighlightPattern(this.combinedPattern, this.wordPatterns, this.wordBoundaryEligible, this.matcher);

  /// A single word's regex, or the never-match `(?!)` for several words (use [matcher]).
  final String combinedPattern;
  final List<String> wordPatterns;
  final List<bool> wordBoundaryEligible;
  final HighlightMatcher matcher;
}

(Pointer<Void>, Map<String, dynamic>?) _open(Map<String, Object?> request) => using((a) {
      final (r, rl) = utf8Native(jsonEncode(request), a);
      final info = a<Pointer<Utf8>>();
      info.value = nullptr;
      final h = _n.matcherOpen(r, rl, info);
      if (info.value == nullptr) throw _n.error();
      final j = jsonDecode(_n.takeString(info.value)) as Map<String, dynamic>?;
      return (h, j);
    });

HighlightPattern? _pattern(Map<String, Object?> request) {
  final (h, j) = _open(request);
  if (j == null || h == nullptr) return null;
  return HighlightPattern(j['combined_pattern'] as String, (j['word_patterns'] as List).cast<String>(),
      (j['word_boundary_eligible'] as List).cast<bool>(), HighlightMatcher._(h, j['allows_cross_line'] as bool));
}

/// Otzaria's `generateHighlightPattern`: highlights an advanced search's matches in an
/// opened book exactly as the engine matched them. [customSpacing] is keyed `"i-(i+1)"`,
/// [alternativeWords] by word position, [searchOptions] `"{word}_{index}"` with
/// Otzaria's option names. Null when the query has no highlightable word.
HighlightPattern? generateHighlightPattern({
  required String query,
  required int distance,
  Map<String, String> customSpacing = const {},
  Map<int, List<String>> alternativeWords = const {},
  Map<String, Map<String, bool>> searchOptions = const {},
  bool anyOrder = false,
}) =>
    _pattern({
      'kind': 'highlight',
      'query': query,
      'distance': distance,
      'custom_spacing': customSpacing,
      'alternative_words': {for (final e in alternativeWords.entries) '${e.key}': e.value},
      'search_options': searchOptions,
      'any_order': anyOrder,
    });

/// Otzaria's `generateSmartHighlightPattern`; [exact] = `SemanticLexicalMode.exact`.
HighlightPattern? generateSmartHighlightPattern({required String query, required bool exact}) =>
    _pattern({'kind': 'smart', 'query': query, 'exact': exact});

/// Otzaria's `prepareUserRegexMatcher`: the book-view matcher of a `/.../` query, null
/// when it is invalid. Use [HighlightMatcher.findMatches] (no boundaries).
HighlightMatcher? prepareUserRegexMatcher(String raw) {
  final (h, j) = _open({'kind': 'user_regex', 'raw': raw});
  if (j == null || h == nullptr) return null;
  return HighlightMatcher._(h, false);
}

/// One `line_note` row for [libraryBookTextFingerprint].
typedef BookNoteRow = ({int lineIndex, int charPos, bool hasMarker, String body});

/// Otzaria's `textHash` of a library book (`libraryBookContentFingerprint` after the
/// read), byte-identical to otzaria_search_engine 7.2.0: [rows] are the stored rows in
/// `lineIndex, id` order as read (zstd frames decoded, a TEXT value's own bytes, NULL
/// empty), [notes] its `line_note` rows in `lineIndex, noteIndex` order. Null when the
/// book has no text. The DB reading is the host's (`SeforimSqlDb.bookContentFingerprint`).
int? libraryBookTextFingerprint(List<Uint8List> rows, List<BookNoteRow> notes) => using((a) {
      var total = 0;
      for (final r in rows) {
        total += r.length;
      }
      final data = a<Uint8>(total == 0 ? 1 : total);
      final lens = a<Uint64>(rows.isEmpty ? 1 : rows.length);
      var at = 0;
      for (var i = 0; i < rows.length; i++) {
        data.asTypedList(total == 0 ? 1 : total).setAll(at, rows[i]);
        lens[i] = rows[i].length;
        at += rows[i].length;
      }
      final (nj, njl) = utf8Native(
          notes.isEmpty ? '' : jsonEncode([for (final n in notes) [n.lineIndex, n.charPos, n.hasMarker, n.body]]), a);
      final out = a<Uint64>();
      final status = _n.bookContentFingerprint(data, lens, rows.length, nj, njl, out);
      if (status < 0) throw _n.error();
      return status == 0 ? null : out.value;
    });
