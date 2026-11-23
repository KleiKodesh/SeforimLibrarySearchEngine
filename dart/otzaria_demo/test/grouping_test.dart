import 'package:otzaria_demo/grouping.dart';
import 'package:test/test.dart';

void main() {
  test('keys: section 0 and unsigned lines stay alone', () {
    expect(groupKey(Grouping.sameSection, tocEntryId: 5, lineId: 1), (0, 5));
    expect(groupKey(Grouping.sameSection, tocEntryId: 0, lineId: 7), (2, 7));
    expect(groupKey(Grouping.identicalText, tocEntryId: 5, lineId: 9), (2, 9));
    expect(groupKey(Grouping.identicalText, tocEntryId: 5, lineId: 9, lineHash: 42), (1, 42));
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
}
