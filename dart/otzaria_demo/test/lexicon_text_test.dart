import 'dart:convert';

import 'package:otzaria_demo/acronym_lexicon.dart';
import 'package:otzaria_demo/lexicon_text.dart';
import 'package:otzaria_demo/translation_lexicon.dart';
import 'package:test/test.dart';

// Hebrew written as \u escapes only (network filter).
const alef = '\u05D0', bet = '\u05D1', gimel = '\u05D2', dalet = '\u05D3';
const he = '\u05D4', vav = '\u05D5', zayin = '\u05D6', het = '\u05D7';

void main() {
  group('normalizeLexiconWord', () {
    test('Hebrew gershayim and geresh become ASCII', () {
      expect(normalizeLexiconWord('$alef\u05F4$bet'), '$alef"$bet');
      expect(normalizeLexiconWord('$alef\u05F3'), "$alef'");
    });
    test('presentation forms fold', () {
      expect(normalizeLexiconWord('\uFB31'), bet); // bet + dagesh, mark dropped
      expect(normalizeLexiconWord('\uFB21$bet'), '$alef$bet');
    });
  });

  test('sortedKeys orders by code point, not file order', () {
    expect(sortedKeys(jsonDecode('{"$gimel":1,"$alef":2,"$bet":3}') as Map), [alef, bet, gimel]);
  });

  group('AcronymLexicon', () {
    test('gershayim key reached by an ASCII-quoted query', () {
      final lex = AcronymLexicon.parse(jsonEncode({
        '$alef$bet\u05F4$gimel': ['$alef$alef $bet$bet $gimel$gimel']
      }));
      expect(lex.alternativesFor(['$alef$bet"$gimel']), [
        ['$alef$alef', '$bet$bet', '$gimel$gimel']
      ]);
      expect(lex.alternativesFor(['$alef$bet$gimel']).length, 1);
    });
    test('geresh key reached by an ASCII apostrophe', () {
      final lex = AcronymLexicon.parse(jsonEncode({
        '$alef\u05F3': ['$alef$bet $gimel$dalet']
      }));
      expect(lex.alternativesFor(["$alef'"]), [
        ['$alef$bet', '$gimel$dalet']
      ]);
    });
    test('presentation-form key', () {
      final lex = AcronymLexicon.parse(jsonEncode({
        '\uFB21\uFB31': ['$he$he $vav$vav']
      }));
      expect(lex.expand('$alef$bet'), [
        ['$he$he', '$vav$vav']
      ]);
    });
    test('reverse lookup in sorted key order, whatever the file order', () {
      const phrase = '$zayin$zayin $het$het';
      final shuffled = AcronymLexicon.parse('{"$gimel$alef":["$phrase"],"$alef$bet":["$phrase"],"$bet$gimel":["$phrase"]}');
      expect(shuffled.alternativesFor(['$zayin$zayin', '$het$het']), [
        ['$alef$bet'],
        ['$bet$gimel'],
        ['$gimel$alef']
      ]);
    });
    test('cap 16 expansions', () {
      final exps = [for (var i = 0; i < 20; i++) '$alef$bet ${gimel * (i + 1)}'];
      final lex = AcronymLexicon.parse(jsonEncode({'$alef$bet': exps}));
      expect(lex.expand('$alef$bet').length, AcronymLexicon.maxExpansions);
      expect(lex.expand('$alef$bet').first, ['$alef$bet', gimel]);
    });
  });

  group('TranslationLexicon', () {
    test('gershayim and presentation forms in keys and values', () {
      final lex = TranslationLexicon.parse(jsonEncode({
        'd': [
          {'$alef\u05F4$bet': '\uFB31$gimel'}
        ]
      }));
      expect(lex.expansions('$alef"$bet'), ['$bet$gimel']);
      expect(lex.expansions('$bet$gimel'), ['$alef"$bet']);
    });
    test('shuffled head words give sorted reverse lists; cap 16', () {
      final lex = TranslationLexicon.parse('{"d":[{"$gimel":"$het","$alef":"$het","$bet":"$het"}]}');
      expect(lex.expansions(het), [alef, bet, gimel]);
      final many = {for (var i = 0; i < 20; i++) '$alef${bet * (i + 1)}': het};
      final capped = TranslationLexicon.parse(jsonEncode({
        'd': [many]
      }));
      expect(capped.expansions(het).length, TranslationLexicon.maxExpansions);
    });
  });
}
