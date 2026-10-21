/// Otzaria's result grouping (`ResultGrouping`, otzaria_search_engine v5.0.0,
/// `api/search_engine.rs`), done by the host over the engine's hits.
///
/// - sameSection: hits under the same heading (`tocEntryId`) form one card
///   with a count. A hit without a section (0) is its own group.
/// - identicalText: lines whose text signature ([lineDedupHash]) is equal
///   merge, across books. A line too short to sign never merges.
///
/// As in Otzaria, groups are ordered by their best member (hit
/// order here), a page counts groups, and a card lists at most [mergedSiblingsCap]
/// siblings besides its representative.
enum Grouping { sameSection, identicalText }

/// Otzaria `MERGED_SIBLINGS_CAP`.
const mergedSiblingsCap = 10;

/// Otzaria `LINE_DEDUP_MIN_LETTERS`.
const lineDedupMinLetters = 12;

/// Otzaria's group key: (kind, value). Section: (0, toc or the line id when
/// there is none, made unique with kind 2). Identical text: (1, hash), or
/// (2, line id) when the line has no signature.
(int, int) groupKey(Grouping g, {required int tocEntryId, required int lineId, String? text}) {
  switch (g) {
    case Grouping.sameSection:
      return tocEntryId != 0 ? (0, tocEntryId) : (2, lineId);
    case Grouping.identicalText:
      final h = lineDedupHash(text ?? '');
      return h == 0 ? (2, lineId) : (1, h);
  }
}

/// Otzaria `line_dedup_hash`: FNV-1a 64 over the letters and digits of the
/// line (case-folded), ignoring spaces, punctuation, quotes and marks, so
/// editions differing only there sign alike; digits and foreign letters do
/// count. 0 = no signature: fewer than [lineDedupMinLetters] Hebrew letters.
///
/// Otzaria hashes its index-normalised text; here the raw line is reduced
/// the same way first: HTML tags dropped, nikud/te'amim dropped (they are
/// not alphanumeric to Otzaria's check either, after normalisation).
int lineDedupHash(String rawLine) {
  final text = rawLine.replaceAll(RegExp(r'<[^>]*>'), ' ');
  var hash = 0xcbf29ce484222325;
  const prime = 0x100000001b3;
  var letters = 0;
  void feed(List<int> bytes) {
    for (final b in bytes) {
      hash ^= b;
      hash = (hash * prime) & 0xFFFFFFFFFFFFFFFF;
    }
  }

  for (final r in text.runes) {
    if (r >= 0x05D0 && r <= 0x05EA) {
      letters++;
      feed(_utf8(r));
    } else if (r < 0x80) {
      final c = String.fromCharCode(r);
      if (RegExp(r'[A-Za-z0-9]').hasMatch(c)) feed([c.toLowerCase().codeUnitAt(0)]);
    } else if (_isOtherAlphanumeric(r)) {
      for (final l in String.fromCharCode(r).toLowerCase().runes) {
        feed(_utf8(l));
      }
    }
  }
  return letters < lineDedupMinLetters ? 0 : (hash == 0 ? 1 : hash);
}

/// Letters and digits outside Hebrew/ASCII (Unicode classes, as Rust's
/// `is_alphanumeric`); Hebrew marks and punctuation are not.
bool _isOtherAlphanumeric(int r) {
  if (r >= 0x0591 && r <= 0x05CF) return false; // marks and Hebrew punctuation
  return RegExp(r'[\p{L}\p{N}]', unicode: true).hasMatch(String.fromCharCode(r));
}

List<int> _utf8(int r) {
  if (r < 0x80) return [r];
  if (r < 0x800) return [0xC0 | (r >> 6), 0x80 | (r & 0x3F)];
  if (r < 0x10000) return [0xE0 | (r >> 12), 0x80 | ((r >> 6) & 0x3F), 0x80 | (r & 0x3F)];
  return [0xF0 | (r >> 18), 0x80 | ((r >> 12) & 0x3F), 0x80 | ((r >> 6) & 0x3F), 0x80 | (r & 0x3F)];
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
