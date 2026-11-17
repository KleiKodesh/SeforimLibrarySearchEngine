// A small HF `tokenizers` Unigram tokenizer for Otzaria's Meivin model
// (issue #18): reads the model folder's tokenizer.json and encodes exactly as
// `tokenizer.encode(text, add_special_tokens = true)` does for that file:
// added tokens split first (matched literally in the raw text), then each
// remaining piece through the normalizer (Sequence of NFKC / Replace), the
// Metaspace pre-tokenizer, Viterbi over the piece scores (unk = min score - 10,
// consecutive unknowns fused) and the [CLS] A [SEP] template.
//
// Host-side code: the model and its tokenizer stay in the host, never in the
// engine.

import 'dart:convert';
import 'dart:io';

import 'package:unorm_dart/unorm_dart.dart' as unorm;

class MeivinTokenizer {
  MeivinTokenizer._(this._vocab, this._scores, this._unkId, this._unkScore, this._maxPieceRunes, this._added,
      this._normalizers, this._replacement, this._prependAlways, this._split, this._clsId, this._sepId);

  final Map<String, int> _vocab;
  final List<double> _scores;
  final int _unkId;
  final double _unkScore;
  final int _maxPieceRunes;

  /// Added tokens matched in the raw text, longest first.
  final List<(String, int)> _added;
  final List<String Function(String)> _normalizers;
  final String _replacement;
  final bool _prependAlways, _split;
  final int? _clsId, _sepId;

  static MeivinTokenizer load(String path) =>
      fromJson(jsonDecode(File(path).readAsStringSync()) as Map<String, dynamic>);

  static MeivinTokenizer fromJson(Map<String, dynamic> j) {
    final model = j['model'] as Map<String, dynamic>;
    if (model['type'] != 'Unigram') throw FormatException('not a Unigram model: ${model['type']}');
    if (model['byte_fallback'] == true) throw UnsupportedError('byte_fallback is not supported');
    final vocab = <String, int>{};
    final scores = <double>[];
    var maxRunes = 1;
    var minScore = double.infinity;
    for (final e in model['vocab'] as List) {
      final piece = (e as List)[0] as String;
      final score = (e[1] as num).toDouble();
      vocab.putIfAbsent(piece, () => scores.length);
      scores.add(score);
      if (score < minScore) minScore = score;
      final n = piece.runes.length;
      if (n > maxRunes) maxRunes = n;
    }
    final added = <(String, int)>[
      for (final t in (j['added_tokens'] as List? ?? const []))
        ((t as Map<String, dynamic>)['content'] as String, t['id'] as int)
    ]..sort((a, b) => b.$1.length.compareTo(a.$1.length));
    for (final t in (j['added_tokens'] as List? ?? const [])) {
      final m = t as Map<String, dynamic>;
      if (m['normalized'] == true || m['single_word'] == true || m['lstrip'] == true || m['rstrip'] == true) {
        throw UnsupportedError('added token options other than the plain literal match: ${m['id']}');
      }
    }
    final normalizers = <String Function(String)>[];
    void addNormalizer(Map<String, dynamic> n) {
      switch (n['type']) {
        case 'Sequence':
          for (final s in n['normalizers'] as List) {
            addNormalizer(s as Map<String, dynamic>);
          }
        case 'NFKC':
          normalizers.add(unorm.nfkc);
        case 'NFC':
          normalizers.add(unorm.nfc);
        case 'NFKD':
          normalizers.add(unorm.nfkd);
        case 'NFD':
          normalizers.add(unorm.nfd);
        case 'Replace':
          final p = n['pattern'] as Map<String, dynamic>;
          final content = n['content'] as String;
          if (p.containsKey('Regex')) {
            final re = RegExp(p['Regex'] as String, unicode: true);
            normalizers.add((s) => s.replaceAll(re, content));
          } else {
            final lit = p['String'] as String;
            normalizers.add((s) => s.replaceAll(lit, content));
          }
        case null:
          break;
        default:
          throw UnsupportedError('normalizer ${n['type']}');
      }
    }

    if (j['normalizer'] != null) addNormalizer(j['normalizer'] as Map<String, dynamic>);
    final pre = j['pre_tokenizer'] as Map<String, dynamic>?;
    if (pre == null || pre['type'] != 'Metaspace') throw UnsupportedError('pre_tokenizer ${pre?['type']}');
    final scheme = pre['prepend_scheme'] as String? ?? 'always';
    if (scheme != 'always' && scheme != 'never') throw UnsupportedError('prepend_scheme $scheme');
    int? special(String name) {
      final st = ((j['post_processor'] as Map<String, dynamic>?)?['special_tokens'] as Map<String, dynamic>?)?[name]
          as Map<String, dynamic>?;
      return st == null ? null : (st['ids'] as List).first as int;
    }

    return MeivinTokenizer._(vocab, scores, model['unk_id'] as int, minScore - 10.0, maxRunes, added, normalizers,
        pre['replacement'] as String, scheme == 'always', pre['split'] as bool? ?? true, special('[CLS]'),
        special('[SEP]'));
  }

  /// Token ids of [text] with the template's specials; [maxTokens] truncates
  /// on the right keeping the specials (the total counts them).
  List<int> encode(String text, {int? maxTokens}) {
    final content = <int>[];
    for (final (piece, addedId) in _splitAdded(text)) {
      if (addedId != null) {
        content.add(addedId);
      } else {
        content.addAll(_encodePiece(piece));
      }
    }
    final specials = (_clsId == null ? 0 : 1) + (_sepId == null ? 0 : 1);
    if (maxTokens != null && content.length + specials > maxTokens) {
      content.length = (maxTokens - specials).clamp(0, content.length);
    }
    return [if (_clsId != null) _clsId, ...content, if (_sepId != null) _sepId];
  }

  /// The raw text cut at added tokens (leftmost, longest match).
  List<(String, int?)> _splitAdded(String text) {
    final out = <(String, int?)>[];
    var start = 0, i = 0;
    while (i < text.length) {
      (String, int)? hit;
      for (final a in _added) {
        if (text.startsWith(a.$1, i)) {
          hit = a;
          break;
        }
      }
      if (hit == null) {
        i++;
        continue;
      }
      if (i > start) out.add((text.substring(start, i), null));
      out.add((hit.$1, hit.$2));
      i += hit.$1.length;
      start = i;
    }
    if (start < text.length) out.add((text.substring(start), null));
    return out;
  }

  List<int> _encodePiece(String raw) {
    var s = raw;
    for (final n in _normalizers) {
      s = n(s);
    }
    if (s.isEmpty) return const [];
    s = s.replaceAll(' ', _replacement);
    if (_prependAlways && !s.startsWith(_replacement)) s = '$_replacement$s';
    final words = <String>[];
    if (_split) {
      // MergedWithNext: each replacement starts a new word
      var b = 0;
      for (var k = 1; k <= s.length; k++) {
        if (k == s.length || s.startsWith(_replacement, k)) {
          words.add(s.substring(b, k));
          b = k;
        }
      }
    } else {
      words.add(s);
    }
    return [for (final w in words) ..._viterbi(w)];
  }

  List<int> _viterbi(String word) {
    final runes = word.runes.toList();
    final n = runes.length;
    final best = List<double>.filled(n + 1, double.negativeInfinity);
    final from = List<int>.filled(n + 1, -1);
    final ids = List<int>.filled(n + 1, -1);
    best[0] = 0;
    for (var i = 0; i < n; i++) {
      if (best[i] == double.negativeInfinity) continue;
      var single = false;
      final sb = StringBuffer();
      for (var len = 1; len <= _maxPieceRunes && i + len <= n; len++) {
        sb.writeCharCode(runes[i + len - 1]);
        final id = _vocab[sb.toString()];
        if (id == null) continue;
        if (len == 1) single = true;
        final sc = best[i] + _scores[id];
        if (from[i + len] < 0 || sc > best[i + len]) {
          best[i + len] = sc;
          from[i + len] = i;
          ids[i + len] = id;
        }
      }
      if (!single) {
        final sc = best[i] + _unkScore;
        if (from[i + 1] < 0 || sc > best[i + 1]) {
          best[i + 1] = sc;
          from[i + 1] = i;
          ids[i + 1] = _unkId;
        }
      }
    }
    final out = <int>[];
    for (var e = n; e > 0; e = from[e]) {
      out.add(ids[e]);
    }
    // fuse consecutive unknowns into one token
    final fused = <int>[];
    for (final id in out.reversed) {
      if (id == _unkId && fused.isNotEmpty && fused.last == _unkId) continue;
      fused.add(id);
    }
    return fused;
  }
}
