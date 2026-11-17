import 'dart:convert';
import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

/// A tiny Unigram tokenizer.json in the shape of the Meivin model's.
Map<String, dynamic> _tiny() => {
      'added_tokens': [
        for (final (i, t) in ['[PAD]', '[UNK]', '[CLS]', '[SEP]', '[QUERY]'].indexed)
          {'id': i, 'content': t, 'single_word': false, 'lstrip': false, 'rstrip': false, 'normalized': false}
      ],
      'normalizer': {
        'type': 'Sequence',
        'normalizers': [
          {'type': 'NFKC'},
          {
            'type': 'Replace',
            'pattern': {'Regex': '[\\u200B-\\u200F]'},
            'content': ''
          },
          {
            'type': 'Replace',
            'pattern': {'String': '`'},
            'content': "'"
          },
        ]
      },
      'pre_tokenizer': {'type': 'Metaspace', 'replacement': '▁', 'prepend_scheme': 'always', 'split': true},
      'post_processor': {
        'type': 'TemplateProcessing',
        'special_tokens': {
          '[CLS]': {
            'ids': [2]
          },
          '[SEP]': {
            'ids': [3]
          }
        }
      },
      'model': {
        'type': 'Unigram',
        'unk_id': 1,
        'byte_fallback': false,
        'vocab': [
          ['[PAD]', 0.0], ['[UNK]', 0.0], ['[CLS]', 0.0], ['[SEP]', 0.0], ['[QUERY]', 0.0], //
          ['▁', -2.0], ['▁hello', -1.0], ['▁he', -3.0], ['llo', -3.0], ['▁world', -1.5],
          ['a', -4.0], ['b', -4.0], ['ab', -2.0], ["'", -3.0], ['fi', -2.5],
        ],
      },
    };

void main() {
  final t = MeivinTokenizer.fromJson(_tiny());

  test('template, metaspace and the best path', () {
    expect(t.encode('hello world'), [2, 6, 9, 3]);
    expect(t.encode(' ab'), [2, 5, 12, 3]); // a leading space is the prepended mark
    expect(t.encode('hello  world'), [2, 6, 5, 9, 3]); // a double space keeps a lone mark
  });

  test('added tokens are matched literally, before the normalizer', () {
    expect(t.encode('[QUERY] hello'), [2, 4, 6, 3]);
    expect(t.encode('[QUERY]hello'), [2, 4, 6, 3]); // the piece after it gets the mark too
  });

  test('normalizer: NFKC, regex and string replaces', () {
    expect(t.encode('ﬁ'), [2, 5, 14, 3]); // the fi ligature folds to f + i
    expect(t.encode('a​b'), [2, 5, 12, 3]);
    expect(t.encode('`'), [2, 5, 13, 3]);
  });

  test('unknown characters fuse into one [UNK]', () {
    expect(t.encode('hello xyz'), [2, 6, 5, 1, 3]);
    expect(t.encode('xyab'), [2, 5, 1, 12, 3]);
  });

  test('truncation keeps the specials', () {
    expect(t.encode('hello world hello', maxTokens: 4), [2, 6, 9, 3]);
    expect(t.encode('hello world hello', maxTokens: 3), [2, 6, 3]);
  });

  // The real tokenizer against Otzaria's reference ids (HF tokenizers 0.23.2):
  // runs when the installed model and the otzaria-semantic-search checkout exist.
  final model = '${Platform.environment['APPDATA']}/otzaria/books/meivin-qa491-onnx/tokenizer.json';
  final golden = Platform.environment['OTZARIA_GOLDEN'] ??
      '${Platform.environment['USERPROFILE']}/.cargo/git/checkouts/otzaria-semantic-search-934c7c4f97476962/'
          'd2a782a/tests/data/onnx_golden_vectors.json';
  test('real tokenizer equals the reference token ids', () {
    final real = MeivinTokenizer.load(model);
    final cases = (jsonDecode(File(golden).readAsStringSync()) as Map<String, dynamic>)['cases'] as List;
    final max = 256;
    final bad = <String>[];
    for (final c in cases.cast<Map<String, dynamic>>()) {
      final ids = real.encode(c['input'] as String, maxTokens: max);
      if (jsonEncode(ids) != jsonEncode(c['token_ids'])) bad.add(c['name'] as String);
    }
    expect(bad, isEmpty, reason: '${bad.length} of ${cases.length} cases differ');
  }, skip: File(model).existsSync() && File(golden).existsSync() ? false : 'model or golden file not installed');
}
