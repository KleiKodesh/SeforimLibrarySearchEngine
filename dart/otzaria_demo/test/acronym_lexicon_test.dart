import 'package:otzaria_demo/acronym_lexicon.dart';
import 'package:otzaria_demo/otzaria_query.dart';
import 'package:test/test.dart';

// Otzaria's own AcronymLexicon tests (lexicons.rs), ported.
const sample = r'''{
  "א\"ב": ["אין בו", "איכא בנייהו", "איסורי ביאה"],
  "רמב\"ם": ["רבי משה בן מיימון"],
  "וכו'": ["וכולי"]
}''';

void main() {
  final lex = AcronymLexicon.parse(sample);

  test('forward', () {
    expect(lex.expand('אב'), [
      ['אין', 'בו'],
      ['איכא', 'בנייהו'],
      ['איסורי', 'ביאה'],
    ]);
    expect(lex.expand('א"ב'), lex.expand('אב'));
    expect(lex.expand('רמבם'), [
      ['רבי', 'משה', 'בן', 'מיימון']
    ]);
  });

  test('reverse', () {
    expect(lex.acronymsFor(['איכא', 'בנייהו']), ['אב']);
    expect(lex.acronymsFor(['רבי', 'משה', 'בן', 'מיימון']), ['רמבם']);
  });

  test('single-word expansion skipped', () {
    expect(lex.expand('וכו'), isEmpty);
    expect(lex.acronymsFor(['וכולי']), isEmpty);
  });

  test('cap and unknown', () {
    expect(lex.expand('אב', 1), [
      ['אין', 'בו']
    ]);
    expect(lex.expand('שאיננו'), isEmpty);
    expect(lex.acronymsFor(['מילה', 'אקראית']), isEmpty);
  });

  test('garbage rejected', () {
    expect(() => AcronymLexicon.parse('not json'), throwsFormatException);
  });

  test('option sends phrase alternatives both ways', () {
    final json = OtzariaSearch(query: 'רמב"ם', globalOptions: {OtzariaOption.acronyms: true}, acronymLexicon: lex)
        .toOptions()
        .toJson();
    expect(json['phrase_alternatives'].toString(), contains('מיימון'));
    final back = OtzariaSearch(
            query: 'רבי משה בן מיימון', globalOptions: {OtzariaOption.acronyms: true}, acronymLexicon: lex)
        .toOptions()
        .toJson();
    expect(back['phrase_alternatives'].toString(), contains('רמבם'));
  });
}
