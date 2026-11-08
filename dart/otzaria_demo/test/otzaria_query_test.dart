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
    // a partial mode has no distance or order
    expect(s.containsKey('proximity'), isFalse);
    final all = OtzariaSearch(
      query: 'משה ו תורה',
      distance: 2,
      customSpacing: {0: 1},
    ).toOptions().toJson();
    expect(all['proximity'], {
      'mode': 'ordered',
      'gaps': [1, 1]
    });
  });

  test('partial modes: a repeated word counts once, its options and alternatives merged', () {
    const a = 'אב', b = 'גד', x = 'הו', y = 'זח';
    final s = OtzariaSearch(
      query: '$a $b $a',
      wordMatchMode: WordMatchMode.atLeast,
      wordMatchCount: 2,
      wordOptions: {
        2: {OtzariaOption.prefixes}
      },
      alternativeWords: {
        0: [x],
        2: [y, x]
      },
    ).toOptions().toJson();
    expect(s['query'], '$a $b');
    expect(s['word_match'], {'mode': 'at_least', 'count': 2});
    expect(s.containsKey('proximity'), isFalse);
    expect(s['words'], {
      '0': {
        'advanced_prefixes': true,
        'alternatives': [x, y]
      }
    });
    // all words: the repeated word stays (ordered phrase)
    expect(OtzariaSearch(query: '$a $b $a').toOptions().toJson()['query'], '$a $b $a');
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

  test('exact mode allows Otzaria exactWordOptionKeys only (issue #9 CB04)', () {
    expect(OtzariaOption.exactAllowed, {
      OtzariaOption.grammaticalPrefixes,
      OtzariaOption.grammaticalSuffixes,
      OtzariaOption.fullOrDefective,
      OtzariaOption.partial,
      OtzariaOption.typoTolerance,
    });
  });

  // Engine scanner word counts (FtsLib / Otzaria word model: runs of letters,
  // marks and ASCII digits; quotes stay only between letters).
  test('words split as the engine scanner does (issue #9 CB05)', () {
    const a = '\u05D0\u05D1', b = '\u05D2\u05D3', c = '\u05D4\u05D5';
    final cases = <String, List<String>>{
      '$a,$b': [a, b],
      '$a.$b;$c': [a, b, c],
      '$a:$b!$c?': [a, b, c],
      '$a($b)': [a, b],
      '$a\u05C3 $b\u05C0$c': [a, b, c], // sof pasuq, paseq
      '$a\u2013$b \u00A0$c': [a, b, c], // en dash, nbsp
      '${a}5,$b': ['${a}5', b],
      '\u05D0"\u05D1, $b': ['\u05D0"\u05D1', b],
      '$a/$b+$c': [a, b, c],
    };
    cases.forEach((q, want) => expect(splitQueryWords(q), want, reason: q.codeUnits.toString()));
  });

  test('sanitizer folds space entities and presentation forms (issue #9 CB06)', () {
    const a = '\u05D0\u05D1', b = '\u05D2\u05D3';
    expect(splitQueryWords('$a&nbsp;$b'), [a, b]);
    expect(splitQueryWords('$a&NBSP;$b&#160;$a&ensp;$b&#xa0;$a'), [a, b, a, b, a]);
    // U+FB2E (alef + patah) -> U+05D0 U+05B7; U+FB31 (bet + dagesh) -> U+05D1 U+05BC
    expect(splitQueryWords('\uFB2E\uFB31'), ['\u05D0\u05B7\u05D1\u05BC']);
  });
}
