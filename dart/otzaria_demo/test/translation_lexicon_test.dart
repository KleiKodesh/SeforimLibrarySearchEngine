import 'package:otzaria_demo/otzaria_query.dart';
import 'package:otzaria_demo/translation_lexicon.dart';
import 'package:test/test.dart';

// Otzaria's own lexicon tests (lexicons.rs), ported.
const sample = '''{
  "מילון פשיטא": [
    { "אִיתָא": "יש" },
    { "אבא": "יער" },
    { "אאגורייהו": "{אַאָגוּרַיְיהוּ} על שוכריהם" },
    { "אברא": "{אַבָּרָא} אמת. *** {אֲבָרָא } אבר, עופרת." }
  ]
}''';

void main() {
  final lex = TranslationLexicon.parse(sample);

  test('both directions', () {
    expect(lex.expansions('איתא'), ['יש']);
    expect(lex.expansions('יש'), ['איתא']);
    expect(lex.expansions('יער'), ['אבא']);
    expect(lex.expansions('אברא'), ['אמת', 'אבר', 'עופרת']);
    expect(lex.expansions('עופרת'), ['אברא']);
  });

  test('multi-word glosses skipped', () {
    expect(lex.expansions('אאגורייהו'), isEmpty);
    expect(lex.expansions('שוכריהם'), isEmpty);
  });

  test('cap and unknown', () {
    expect(lex.expansions('אברא', 1), ['אמת']);
    expect(lex.expansions('מילה־שאיננה'), isEmpty);
  });

  test('all dictionaries merged', () {
    final l = TranslationLexicon.parse('''{
      "מילון פשיטא": [ { "הכא": "כאן" } ],
      "מילון שיח ישראל": [ { "התם": "שם" } ],
      "מושגים ואישים": [ { "אביי": "אמורא בדור הרביעי" } ]
    }''');
    expect(l.expansions('הכא'), ['כאן']);
    expect(l.expansions('כאן'), ['הכא']);
    expect(l.expansions('שם'), ['התם']);
    expect(l.expansions('אביי'), isEmpty);
  });

  test('garbage rejected', () {
    expect(() => TranslationLexicon.parse('not json'), throwsFormatException);
  });

  test('option adds the translations as word alternatives', () {
    final req = OtzariaSearch(query: 'הכא', globalOptions: {OtzariaOption.aramaicTranslation: true},
            translations: TranslationLexicon.parse('{"d": [ { "הכא": "כאן" } ]}'))
        .toRequest();
    final json = req.toJson().toString();
    expect(json, contains('any'));
    expect(json, contains('כאן'));
  });
}
