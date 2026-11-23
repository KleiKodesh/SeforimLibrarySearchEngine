/// Otzaria's result grouping (`ResultGrouping`, otzaria_search_engine v5.0.0,
/// `api/search_engine.rs`), done by the host over the engine's hits.
///
/// - sameSection: hits under the same heading (`tocEntryId`) form one card
///   with a count. A hit without a section (0) is its own group.
/// - identicalText: lines whose text signature (the index's `line_hash`,
///   Otzaria's `lineHash`, [SeforimIndex.lineHashes]) is equal merge, across
///   books. A line too short to sign never merges. (User's note: this grouping
///   is pointless functionality that shouldn't exist; the same text in
///   different commentaries is fine to show. Kept for Otzaria's behaviour.)
///
/// As in Otzaria, groups are ordered by their best member (hit
/// order here), a page counts groups, and a card lists at most [mergedSiblingsCap]
/// siblings besides its representative.
enum Grouping { sameSection, identicalText }

/// Otzaria `MERGED_SIBLINGS_CAP`.
const mergedSiblingsCap = 10;

/// Otzaria's group key: (kind, value). Section: (0, toc or the line id when
/// there is none, made unique with kind 2). Identical text: (1, the line's
/// `lineHash`), or (2, line id) when the line has no signature.
(int, int) groupKey(Grouping g, {required int tocEntryId, required int lineId, int lineHash = 0}) {
  switch (g) {
    case Grouping.sameSection:
      return tocEntryId != 0 ? (0, tocEntryId) : (2, lineId);
    case Grouping.identicalText:
      return lineHash == 0 ? (2, lineId) : (1, lineHash);
  }
}

/// One card: the hits of a group, in hit order.
class HitGroup<T> {
  HitGroup(this.key);
  final (int, int) key;
  final List<T> members = [];
}

/// Groups `items` (already in hit order) by `keyOf`, keeping first-seen
/// order: a group's first member is its best one, so groups come out sorted
/// by their representative, as Otzaria's `finalize_grouped`.
List<HitGroup<T>> groupInOrder<T>(Iterable<T> items, (int, int) Function(T) keyOf) {
  final byKey = <(int, int), HitGroup<T>>{};
  final order = <HitGroup<T>>[];
  for (final item in items) {
    final k = keyOf(item);
    final g = byKey.putIfAbsent(k, () {
      final n = HitGroup<T>(k);
      order.add(n);
      return n;
    });
    g.members.add(item);
  }
  return order;
}
