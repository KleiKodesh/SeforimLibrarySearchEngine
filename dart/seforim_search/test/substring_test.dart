import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

String? _nativePath() {
  final env = Platform.environment['SEFORIM_SEARCH_NATIVE'];
  if (env != null) return env;
  var dir = Directory.current;
  for (var i = 0; i < 5; i++) {
    final f = File('${dir.path}/target/release/${SeforimNative.defaultFileName}');
    if (f.existsSync()) return f.path;
    dir = dir.parent;
  }
  return null;
}

/// Issue G-K1: the substring query, position ranges and the decline over the cap.
void main() {
  late Directory tmp;
  late SeforimIndex index;

  setUpAll(() {
    SeforimNative.load(_nativePath());
    tmp = Directory.systemTemp.createTempSync('se_dart_substring_');
    final created = SeforimIndex.create(tmp.path);
    final w = created.openWriter(heapBytes: 30000000, threads: 1);
    const texts = ['alpha beta', 'xxxxxxxxfghyyyyyyyy', 'fgh', 'zzfghzz omega', 'delta qrsfgh', 'omega fghij'];
    for (var i = 0; i < texts.length; i++) {
      w.add(LineDocument(lineId: 10 + i, position: i, bookId: 1 + i ~/ 3, text: texts[i]));
    }
    w.mergeAndClose();
    expect(created.buildFmSidecar(), isTrue);
    created.close();
    index = SeforimIndex.open(tmp.path);
  });

  tearDownAll(() {
    index.close();
    tmp.deleteSync(recursive: true);
  });

  List<int>? ids(SearchOptions o) {
    final s = index.search(o);
    try {
      if (s.info().declined) return null;
      return [for (final h in s.allHits()) h.lineId];
    } finally {
      s.close();
    }
  }

  test('substring: every term containing the fragment, restricted to the ranges', () {
    expect(ids(SearchOptions('fgh', substring: true, declineOverCap: true)), [11, 12, 13, 14, 15]);
    expect(ids(SearchOptions('fgh', substring: true, positionRanges: [(4, 9), (1, 2)])), [11, 12, 14, 15]);
    expect(ids(SearchOptions('omega', positionRanges: [(0, 3)])), [13]);
    expect(ids(SearchOptions('omega', positionRanges: [])), isEmpty);
  });

  test('declined: no indexable term, or over the cap', () {
    expect(ids(SearchOptions('!!!', substring: true)), isNull);
    expect(ids(SearchOptions('fgh', substring: true, declineOverCap: true, maxExpandedTerms: 4)), isNull);
    final s = index.search(SearchOptions('fgh', substring: true, maxExpandedTerms: 4));
    expect(s.info().truncated && !s.info().declined, isTrue);
    s.close();
  });
}
