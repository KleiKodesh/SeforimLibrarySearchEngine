import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:test/test.dart';

// Otzaria's lexical search expansion (MagicDictionary over lexical.db), on a tiny
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
late LexicalDb dict;

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
    dict = LexicalDb.open(path);
  });
  tearDownAll(() {
    dict.close();
    dir.deleteSync(recursive: true);
  });

  test('a non-database or missing file opens nothing', () {
    expect(LexicalDb.tryOpen('${dir.path}/missing.db'), isNull);
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

  test('expand fills the chosen words, unmarked when blacklisted', () {
    final o = SearchOptions('$halachti~1 $lachtoch~1 $shuv~1 x*~1');
    dict.expand(o, {0: halachti, 1: lachtoch, 3: 'x*'});
    expect(o.lexicalFuzzy, isTrue);
    expect(o.words[0]!.lexicalForms, [hlkFinal, holechFinal]);
    expect(o.words[0]!.unmarkedLexicalForms, isEmpty);
    expect(o.words[0]!.maxExpandedTerms, isNull);
    expect(o.words[1]!.lexicalForms, isEmpty);
    expect(o.words[1]!.unmarkedLexicalForms, [ken, veken]);
    expect(o.words.containsKey(2), isFalse); // not chosen: opt-in per word
    expect(o.words.containsKey(3), isFalse); // a wildcard takes no forms
    final j = o.toJson();
    expect(j['lexical_fuzzy'], true);
  });

  test('expand keeps the word options already set and adds the phrase budget', () {
    final o = SearchOptions('$halachti~1 $shuv~1', words: {0: WordOptions(ignoreQuotes: true, lexicalForms: [hlkFinal])});
    dict.expand(o, {0: halachti, 1: shuv}, phraseWords: {0, 1});
    expect(o.words[0]!.ignoreQuotes, isTrue);
    expect(o.words[0]!.lexicalForms, [hlkFinal, holechFinal]); // no duplicate
    expect(o.words[0]!.maxExpandedTerms, LexicalDb.maxPhraseTermsPerWord - 3);
    expect(o.words[1]!.lexicalForms, [tashuv]);
    expect(o.words[1]!.maxExpandedTerms, LexicalDb.maxPhraseTermsPerWord - 2);
  });

  // The real lexical.db, when Otzaria is installed here (skipped otherwise).
  final real = '${Platform.environment['APPDATA'] ?? ''}/otzaria/books/lexical.db';
  test('the real lexical.db opens and expands a common word', () {
    final db = LexicalDb.open(real);
    try {
      final forms = db.recallForms(halachti);
      expect(forms, isNotEmpty);
      expect(forms.length, lessThanOrEqualTo(LexicalDb.maxLexicalForms));
      expect(forms.toSet().length, forms.length);
    } finally {
      db.close();
    }
  }, skip: File(real).existsSync() ? false : 'no lexical.db at $real');
}
