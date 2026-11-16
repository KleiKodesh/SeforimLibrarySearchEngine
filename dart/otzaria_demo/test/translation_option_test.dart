import 'package:otzaria_demo/otzaria_query.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

// The demo maps Otzaria's Aramaic translation option onto the package's
// TranslationLexicon (its own tests live in the package).
void main() {
  test('option adds the translations as word alternatives', () {
    final req = OtzariaSearch(query: 'הכא', globalOptions: {OtzariaOption.aramaicTranslation: true},
            translations: TranslationLexicon.parse('{"d": [ { "הכא": "כאן" } ]}'))
        .toOptions();
    final json = req.toJson().toString();
    expect(json, contains('alternatives'));
    expect(json, contains('כאן'));
  });
}
