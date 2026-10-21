import 'package:otzaria_demo/otzaria_query.dart';
import 'package:test/test.dart';

void main() {
  test('settings pass through: one-letter words are words', () {
    // the one-letter word is a slot of its own, with its own gaps, and the
    // engine counts it for wordMatch
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

  test('a paired reading keeps its second reading (engine rule)', () {
    final s = OtzariaSearch(query: '(אב) [גד] הו', distance: 0).toOptions().toJson();
    expect(s['query'], 'גד הו');
    expect(s['proximity'], {
      'mode': 'ordered',
      'gaps': [0]
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
