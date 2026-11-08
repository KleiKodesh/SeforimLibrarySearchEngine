import 'dart:io';

import 'package:otzaria_demo/magic_dictionary.dart';
import 'package:otzaria_demo/otzaria_query.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:test/test.dart';

// Issue #10: Otzaria's MagicDictionary as host lexicon code, on a tiny
// synthetic lexical.db (Otzaria's own dictionary.rs fixture, in part).
const schema = '''
CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
''';

// he lamed kaf family
const hlk = 'הלכ', hlkFinal = 'הלך';
const halachti = 'הלכתי';
const holech = 'הולכ', holechFinal = 'הולך';
// a blacklisted pair of the shipped list: token "lachtoch", base "ken"
const ken = 'כן', veken = 'וכן';
const lachtoch = 'לחתוך';
// rambam with gershayim and without
const rambamQ = 'רמב"ם', rambam = 'רמבם';
const haRambamQ = 'ה$rambamQ', leRambamQ = 'ל$rambamQ';
const haRambam = 'ה$rambam', haRambamG = 'הרמב״ם';
// a one-letter "form" (dropped) in the shin-vav-bet family
const shuv = 'שוב', tashuv = 'תשוב', tav = 'ת\'';

late Directory dir;
late MagicDictionary dict;

void main() {
  setUpAll(() {
    dir = Directory.systemTemp.createTempSync('lexical');
    final path = '${dir.path}/lexical.db';
    final db = sqlite3.open(path);
    db.execute(schema);
    final base = db.prepare('INSERT INTO base (id, value) VALUES (?, ?)');
    final surface = db.prepare('INSERT INTO surface (id, value, base_id) VALUES (?, ?, ?)');
    for (final (id, v) in [(1, hlk), (5, ken), (6, shuv), (9, rambamQ), (10, rambam)]) {
      base.execute([id, v]);
    }
    for (final (id, v, b) in [
      (1, halachti, 1), (2, holech, 1), //
      (40, veken, 5), (41, lachtoch, 5),
      (50, tashuv, 6), (51, tav, 6),
      (80, haRambamQ, 9), (81, leRambamQ, 9),
      (90, haRambam, 10), (91, haRambamG, 10),
    ]) {
      surface.execute([id, v, b]);
    }
    base.close();
    surface.close();
    db.close();
    dict = MagicDictionary.open(path);
  });
  tearDownAll(() {
    dict.close();
    dir.deleteSync(recursive: true);
  });

  test('a non-database or missing file opens nothing', () {
    expect(MagicDictionary.tryOpen('${dir.path}/missing.db'), isNull);
  });

  test('a surface token reaches its family as index terms', () {
    expect(dict.recallForms(halachti), [hlkFinal, halachti, holechFinal]);
    expect(dict.recallForms('אבג'), isEmpty);
    expect(dict.recallForms('  '), isEmpty);
  });

  test('forms under two letters are dropped', () {
    expect(dict.recallForms(shuv), [shuv, tashuv]);
  });

  test('gershayim reach both spellings and fold to ASCII', () {
    for (final token in [rambamQ, 'רמב״ם']) {
      final forms = dict.recallForms(token);
      expect(forms.first, rambamQ, reason: token);
      expect(forms, containsAll([haRambamQ, leRambamQ, rambam, haRambam]));
      expect(forms.any((f) => f.contains('״')), isFalse);
    }
  });

  test('the blacklist hides a family from the highlight only', () {
    expect(dict.recallForms(lachtoch), [ken, veken, lachtoch]);
    expect(dict.highlightForms(lachtoch), isEmpty);
    expect(isBlacklisted(lachtoch, ken), isTrue);
    expect(isBlacklisted(veken, ken), isFalse);
    final f = dict.formsFor(lachtoch);
    expect(f.marked, isEmpty);
    expect(f.unmarked, [ken, veken]);
  });

  test('fuzzy mode feeds the forms as alternatives, unmarked when blacklisted', () {
    OtzariaSearch s(String q, SearchMode mode) =>
        OtzariaSearch(query: q, mode: mode, magicDictionary: dict);
    var w = s(halachti, SearchMode.fuzzy).toOptions().words[0]!;
    expect(w.alternatives, [hlkFinal, holechFinal]); // exact words, no ~N
    expect(w.unmarkedAlternatives, isEmpty);
    w = s(lachtoch, SearchMode.fuzzy).toOptions().words[0]!;
    expect(w.alternatives, isEmpty);
    expect(w.unmarkedAlternatives, [ken, veken]);
    // only the fuzzy mode, as Otzaria
    expect(s(halachti, SearchMode.advanced).toOptions().words[0], isNull);
    expect(s(halachti, SearchMode.exact).toOptions().words[0], isNull);
    // the whole query quoted: Otzaria's lexical fuzzy phrase, every word
    // fuzzy with its forms, in order within 1 word
    final o = s('"$halachti $lachtoch"', SearchMode.fuzzy).toOptions();
    expect(o.query, '$halachti~1 $lachtoch~1');
    expect(o.words[0]!.alternatives, [hlkFinal, holechFinal]);
    expect(o.words[1]!.unmarkedAlternatives, [ken, veken]);
    expect(o.proximity!.toJson(), {'mode': 'ordered', 'gaps': [1]});
    // a quoted part of a longer query is searched as written
    expect(s('"$halachti $holech" $shuv', SearchMode.fuzzy).toOptions().words.keys, [2]);
  });
}
