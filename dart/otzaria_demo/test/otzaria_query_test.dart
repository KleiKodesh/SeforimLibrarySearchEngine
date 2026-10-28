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

  test('quoted phrases stay quoted; an acronym quotes nothing', () {
    const a = 'אב', b = 'גד', c = 'הו', d = 'זח';
    const acronym = 'רמב"ם';
    String q(String query, [SearchMode mode = SearchMode.advanced]) =>
        OtzariaSearch(query: query, mode: mode).toOptions().toJson()['query'] as String;
    expect(q('"$a $b"'), '"$a $b"');
    expect(q('$a ״$b $c״ $d'), '$a "$b $c" $d');
    expect(q('„$a“ $b “$c $d”'), '"$a" $b "$c $d"');
    expect(q('$acronym $a'), '$acronym $a');
    expect(q('"$acronym $a" $b'), '"$acronym $a" $b');
    expect(q('"$a $b'), '$a $b');
    // fuzzy mode writes ~N on every word; the engine drops it inside a phrase
    expect(q('"$a $b" $c', SearchMode.fuzzy), '"$a~1 $b~1" $c~1');
  });

  test('a paired reading keeps its second reading (engine rule)', () {
    final s = OtzariaSearch(query: '(אב) [גד] הו', distance: 0).toOptions().toJson();
    expect(s['query'], 'גד הו');
    expect(s['proximity'], {
      'mode': 'ordered',
      'gaps': [0]
    });
  });

  test('negative words have their own options and alternatives', () {
    const torah = 'תורה';
    const beracha = 'ברכה';
    const shalom = 'שלום';
    // no negative options given: every negative word takes the global set
    final inherit = OtzariaSearch(
      query: torah,
      globalOptions: {OtzariaOption.grammaticalPrefixes: true},
      negativeQuery: '$beracha $shalom',
    ).toOptions().toJson();
    expect(inherit['grammar_prefixes'], isTrue);
    final inheritEx = inherit['exclude'] as Map<String, Object?>;
    expect(inheritEx['query'], '$beracha $shalom');
    expect(inheritEx.containsKey('words'), isFalse);
    // Otzaria's negativeSearchOptions: each word its full set (word 0 none,
    // so prefixes off for it); negativeAlternativeWords join word 0's slot
    final own = OtzariaSearch(
      query: torah,
      globalOptions: {OtzariaOption.grammaticalPrefixes: true},
      negativeQuery: '$beracha $shalom',
      negativeWordOptions: {
        1: {OtzariaOption.grammaticalPrefixes, OtzariaOption.fullOrDefective}
      },
      negativeAlternativeWords: {
        0: [torah]
      },
    ).toOptions().toJson();
    expect(own['grammar_prefixes'], isTrue);
    expect((own['exclude'] as Map<String, Object?>)['words'], {
      '0': {
        'alternatives': [torah],
        'grammar_prefixes': false
      },
      '1': {'ktiv_male_haser': true},
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
