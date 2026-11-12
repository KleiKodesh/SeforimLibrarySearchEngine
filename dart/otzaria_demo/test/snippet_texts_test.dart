// The snippet-text cache (KitveiHakodesh's page model): requests of at most
// 200 ids, short rows with context in the same request, in-flight sharing,
// LRU eviction, seeds, lookahead.
import 'dart:async';

import 'package:otzaria_demo/seforim_db.dart';
import 'package:otzaria_demo/snippet_texts.dart';
import 'package:test/test.dart';

void main() {
  final calls = <(List<int>, List<int>)>[];
  Map<int, SnippetText> fake(List<int> ids, List<int> shorts) {
    calls.add((ids, shorts));
    return {
      for (final id in ids) if (id != 999) id: SnippetText(id, 't$id', '', ''),
      for (final id in shorts) id: SnippetText(id, 't$id', 'b$id', 'a$id'),
    };
  }

  setUp(calls.clear);

  test('requests split at 200 ids; short rows ask for context in the same request', () async {
    final c = SnippetTextCache(fake);
    await c.ensure([for (var i = 0; i < 450; i++) (lineId: i, short: i % 2 == 1)]);
    expect(calls.length, 3);
    expect(calls[0].$1.length + calls[0].$2.length, 200);
    expect(calls[2].$1.length + calls[2].$2.length, 50);
    expect(c.textOf((lineId: 3, short: true))!.before, 'b3');
    expect(c.textOf((lineId: 4, short: false))!.text, 't4');
    await c.ensure([for (var i = 0; i < 450; i++) (lineId: i, short: i % 2 == 1)]);
    expect(calls.length, 3); // all cached
  });

  test('a line cached plain is a miss for a short row; context never downgraded', () async {
    final c = SnippetTextCache(fake);
    await c.ensure([(lineId: 1, short: false)]);
    expect(c.textOf((lineId: 1, short: true)), isNull);
    await c.ensure([(lineId: 1, short: true)]);
    expect(calls.last.$2, [1]);
    c.seed([SnippetText(1, 't1', '', '')]);
    expect(c.textOf((lineId: 1, short: true))!.after, 'a1');
  });

  test('a line without a row reads empty and is not asked again', () async {
    final c = SnippetTextCache(fake);
    await c.ensure([(lineId: 999, short: false), (lineId: 1, short: false)]);
    expect(c.textOf((lineId: 999, short: false))!.text, '');
    await c.ensure([(lineId: 999, short: false)]);
    expect(calls.length, 1);
  });

  test('an empty answer (a failed read) is not cached: asked again', () async {
    final c = SnippetTextCache(fake);
    await c.ensure([(lineId: 999, short: false)]);
    expect(c.textOf((lineId: 999, short: false)), isNull);
    await c.ensure([(lineId: 999, short: false)]);
    expect(calls.length, 2);
  });

  test('the same line asked plain and short is asked short once', () async {
    final c = SnippetTextCache(fake);
    await c.ensure([(lineId: 5, short: false), (lineId: 5, short: true)]);
    expect(calls.length, 1);
    expect(calls.single.$2, [5]);
    expect(c.textOf((lineId: 5, short: true))!.before, 'b5');
  });

  test('in-flight lines are awaited, not asked twice', () async {
    final gate = Completer<void>();
    var n = 0;
    final c = SnippetTextCache((ids, shorts) async {
      n++;
      await gate.future;
      return fake(ids, shorts);
    });
    final a = c.ensure([(lineId: 1, short: false), (lineId: 2, short: false)]);
    final b = c.ensure([(lineId: 2, short: false)]);
    gate.complete();
    await Future.wait([a, b]);
    expect(n, 1);
    expect(c.textOf((lineId: 2, short: false))!.text, 't2');
  });

  test('LRU: the least recently used line goes first', () async {
    final c = SnippetTextCache(fake, capacity: 3);
    await c.ensure([for (final i in [1, 2, 3]) (lineId: i, short: false)]);
    c.textOf((lineId: 1, short: false)); // 1 is now the most recent
    await c.ensure([(lineId: 4, short: false)]);
    expect(c.has((lineId: 2, short: false)), isFalse);
    expect(c.has((lineId: 1, short: false)), isTrue);
  });

  test('seeded texts need no request; lookahead 60 ahead, 20 behind', () async {
    final c = SnippetTextCache(fake);
    c.seed([for (var i = 0; i < seedSnippetTextHitCount; i++) SnippetText(i, 't$i', '', '')]);
    await c.ensure([for (var i = 0; i < seedSnippetTextHitCount; i++) (lineId: i, short: false)]);
    expect(calls, isEmpty);
    final rows = [for (var i = 0; i < 1000; i++) (lineId: i, short: false)];
    await c.ensureAround(rows, 500, 30);
    final asked = {for (final call in calls) ...call.$1};
    expect(asked, {for (var i = 480; i < 590; i++) i});
  });
}
