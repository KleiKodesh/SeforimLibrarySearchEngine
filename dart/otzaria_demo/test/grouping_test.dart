import 'package:otzaria_demo/grouping.dart';
import 'package:otzaria_demo/otzaria_query.dart';
import 'package:test/test.dart';

void main() {
  const line = 'אמר רבי יוחנן משום רבי שמעון בן יוחי';

  test('signature ignores spaces, punctuation, quotes, marks and tags', () {
    final h = lineDedupHash(line);
    expect(h, isNot(0));
    expect(lineDedupHash('אָמַר  רַבִּי, יוֹחָנָן: מִשּׁוּם "רבי" <b>שמעון</b> בן־יוחי.'), h);
  });

  test('digits and spelling count', () {
    expect(lineDedupHash('לשלם 100 שקלים לכל אחד ואחד מהם'), isNot(lineDedupHash('לשלם 200 שקלים לכל אחד ואחד מהם')));
    expect(lineDedupHash(line), isNot(lineDedupHash('אמר ר יוחנן משום רבי שמעון בן יוחי')));
  });

  test('under 12 Hebrew letters: no signature', () {
    expect(lineDedupHash('אמר רבי יוחנן'), 0); // 11 letters
    expect(lineDedupHash('אמר רבי יוחנןא'), isNot(0)); // 12
  });

  test('keys: section 0 and unsigned lines stay alone', () {
    expect(groupKey(Grouping.sameSection, tocEntryId: 5, lineId: 1), (0, 5));
    expect(groupKey(Grouping.sameSection, tocEntryId: 0, lineId: 7), (2, 7));
    expect(groupKey(Grouping.identicalText, tocEntryId: 5, lineId: 9, text: 'קצר'), (2, 9));
  });

  test('groups keep catalog order of their first member', () {
    final g = groupInOrder<(int, int)>([(1, 10), (2, 20), (3, 10), (4, 0), (5, 0)],
        (h) => groupKey(Grouping.sameSection, tocEntryId: h.$2, lineId: h.$1));
    expect([for (final x in g) x.members.map((m) => m.$1).toList()], [
      [1, 3],
      [2],
      [4],
      [5],
    ]);
  });

  test('sameSection exclusion blocks sections holding every negative word', () {
    expect(blockedSections([]), isEmpty);
    expect(blockedSections([{1, 2, 3}]), {1, 2, 3});
    expect(blockedSections([{1, 2, 3}, {2, 3, 4}, {3, 2}]), {2, 3});
    expect(blockedSections([{1}, {2}]), isEmpty);
  });
}
