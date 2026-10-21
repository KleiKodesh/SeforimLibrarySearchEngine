import 'package:otzaria_demo/otzaria_query.dart';
import 'package:test/test.dart';

void main() {
  test('settings pass through: the engine handles dropped words', () {
    // the one-letter word stays in the query and in the gaps; the engine
    // drops it and merges its gaps, and counts the words for wordMatch
    final s = OtzariaSearch(
      query: 'משה ו תורה',
      distance: 2,
      wordMatchMode: WordMatchMode.mostWords,
      customSpacing: {0: 1},
    ).toOptions().toJson();
    expect(s['query'], 'משה ו תורה');
    expect(s['word_match'], {'mode': 'most_words'});
    expect(s['proximity'], {
      'mode': 'ordered',
      'gaps': [1, 1]
    });
  });

  test('exact mode sends no word match', () {
    final s = OtzariaSearch(query: 'משה תורה', mode: SearchMode.exact).toOptions().toJson();
    expect(s.containsKey('word_match'), isFalse);
    expect(s['proximity'], {
      'mode': 'ordered',
      'gaps': [0]
    });
  });
}
