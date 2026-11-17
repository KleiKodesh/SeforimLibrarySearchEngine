import 'dart:io';
import 'dart:typed_data';

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

String _repoRoot() {
  var dir = Directory.current;
  for (var i = 0; i < 5; i++) {
    if (File('${dir.path}/Cargo.toml').existsSync() && Directory('${dir.path}/seforim-vectors').existsSync()) {
      return dir.path;
    }
    dir = dir.parent;
  }
  throw StateError('repository root not found');
}

Float32List v(List<double> x) => Float32List.fromList(x);

void main() {
  late Directory tmp;
  setUpAll(() {
    SeforimNative.load(_nativePath());
    tmp = Directory.systemTemp.createTempSync('se_dart_passages_');
  });
  tearDownAll(() => tmp.deleteSync(recursive: true));

  test('passage highlights: plan, host vectors, mark (Otzaria semanticPassageHighlights)', () {
    const line = 'alpha beta gamma delta epsilon, zeta eta theta iota <kappa>; lambda mu. nu xi omicron pi rho';
    final plan = planPassageHighlights('theta', [const PassageTarget(line), const PassageTarget('a short line')]);
    expect(plan.queryText, 'theta');
    expect(plan.needsQueryVector, isTrue);
    expect(plan.lines[0].chosen, [0, 1, 2]);
    expect(plan.lines[0].clauses, [
      'alpha beta gamma delta epsilon,',
      'zeta eta theta iota <kappa>;',
      'lambda mu. nu xi omicron pi rho',
    ]);
    expect(plan.lines[1].clauses, isEmpty);
    final marked = markPassageHighlights('theta', plan, v([0, 1]), [
      [
        v([1, 0]),
        v([0, 1]),
        v([0.6, 0.8]),
      ],
      [],
    ]);
    expect(
      marked[0].snippetHtml,
      'alpha beta gamma delta epsilon, <mark>zeta eta theta iota &lt;kappa&gt;;</mark> lambda mu. nu xi omicron pi rho',
    );
    expect(marked[0].isHighlighted, isTrue);
    expect(marked[0].spanScore, 1.0);
    expect(marked[1].isHighlighted, isFalse);
    expect(marked[1].spanScore, isNull);

    // a short line is marked on the context it was embedded with
    final ctx = planPassageHighlights('x', [
      const PassageTarget(
        'short one',
        contextLines: ['first line of the section', 'short one', 'third line'],
        contextSections: [1, 1, 1],
        contextIndex: 1,
      ),
    ]);
    expect(ctx.lines[0].text, 'first line of the section short one third line');
  });

  test('vector set directory info and coverage (Otzaria semanticVectorsInfo / semanticCoverage)', () {
    expect(readVectorsDirInfo('${tmp.path}/none').present, isFalse);
    expect(
      () => VectorCoverageCounter.open('${tmp.path}/none'),
      throwsA(isA<SeforimEngineException>().having((e) => vectorInstallErrorKind(e), 'kind', 'missing')),
    );
    final fixtures = '${_repoRoot()}/seforim-vectors/tests/data/install';
    final dir = '${tmp.path}/set';
    for (final name in ['base-v1', 'delta-v1-v2']) {
      final seg = '${tmp.path}/$name.oxv';
      expandOxv('$fixtures/$name.oxv.zst', seg);
      installVectorSegment(dir, seg, File('$fixtures/$name.manifest.json').readAsStringSync());
    }
    final info = readVectorsDirInfo(dir);
    expect(info.present, isTrue);
    expect(info.libraryVersion, 2);
    expect(info.segments.map((s) => s.kind), ['base', 'delta']);
    expect(info.modelFamilyId, 'f');
    expect(info.recoveredFromPrevious, isFalse);
    // the fixtures' test identity names no recipe this build implements
    expect(info.chunkingIdentity, 7);
    expect(
      () => VectorCoverageCounter.open(dir),
      throwsA(isA<SeforimEngineException>().having((e) => vectorInstallErrorKind(e), 'kind', 'incompatible')),
    );
  });

  test('semantic status without a session', () {
    final s = SemanticStatus.of(null, modelLoaded: true);
    expect(s.state, SemanticState.notConfigured);
    expect(s.errorKind, 'notConfigured');
    expect(s.enabled, isFalse);
  });

  test('a passage target without the sections its window needs is marked on its own text', () {
    final lines = ['first line of the section', 'short one', 'third line'];
    final withSections = planPassageHighlights('q', [
      PassageTarget('short one', contextLines: lines, contextSections: const [1, 1, 1], contextIndex: 1),
    ]);
    expect(withSections.lines.single.text, 'first line of the section short one third line');
    final without = planPassageHighlights('q', [
      PassageTarget('short one', contextLines: lines, contextSections: const [1, 1], contextIndex: 1),
    ]);
    expect(without.lines.single.text, 'short one');
  });
}
