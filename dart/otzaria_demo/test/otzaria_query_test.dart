import 'package:otzaria_demo/otzaria_query.dart';
import 'package:test/test.dart';

void main() {
  test('a user regex goes to the engine whole, with no options', () {
    const re = '/\u05E9\u05DC\u05D5?\u05DD \u05E2\u05D5\u05DC\u05DD/';
    final s = OtzariaSearch(query: ' $re ', distance: 3, wordMatchMode: WordMatchMode.mostWords).toOptions().toJson();
    expect(s['query'], re);
    expect(s['user_regex'], isTrue);
    expect(s.containsKey('proximity'), isFalse);
    expect(s.containsKey('word_match'), isFalse);
    expect(isUserRegex('/a/ /b/'), isFalse);
    expect(isUserRegex('//'), isFalse);
    expect(isUserRegex('/ ab/'), isTrue);
    // an invalid pattern is searched as words, with the options
    final bad = OtzariaSearch(query: '/a(b c/', distance: 3).toOptions().toJson();
    expect(bad['user_regex'], isNull);
    expect(bad.containsKey('proximity'), isTrue);
  });

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
