import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

// Fake GitHub releases API + asset host on loopback; tiny fixtures only.

String _repoRoot() {
  var dir = Directory.current;
  while (!Directory('${dir.path}/seforim-vectors').existsSync()) {
    dir = dir.parent;
  }
  return dir.path;
}

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

void main() {
  late Directory tmp;
  final native = _nativePath();
  setUp(() => tmp = Directory.systemTemp.createTempSync('se_vset_'));
  tearDown(() => tmp.deleteSync(recursive: true));

  // Fixtures written by seforim-vectors (install_tests.rs): base v1, deltas v1-v2
  // and v2-v3, base v3; real segments with block CRCs, ids and manifests.
  final fixtures = '${_repoRoot()}/seforim-vectors/tests/data/install';
  String manifestOf(String name) => File('$fixtures/$name.manifest.json').readAsStringSync();

  test('.del encoding equals the sidecar (CRC of an empty 10043833-slot set)', () {
    final (bytes, crc) = encodeDel(10043833, Uint64List((10043833 + 63) ~/ 64));
    expect(crc, 2484906093);
    expect(bytes.length, 24 + 156935 * 8 + 4);
    expect(ascii.decode(bytes.sublist(0, 8)), 'OXVDEL1\n');
  });

  test('the engine installs a base, then a delta chain, on a worker', () async {
    if (native == null) {
      markTestSkipped('native library not built');
      return;
    }
    SeforimNative.load(native);
    final setDir = '${tmp.path}/vectors';
    final indexDir = '${tmp.path}/index';
    SeforimIndex.create(indexDir).close();
    final worker = await SearchWorker.spawn(indexDir, nativePath: native);
    addTearDown(worker.close);
    final writer = VectorSetWriter(setDir, worker: worker);
    expect((await worker.verifyOxv('${tmp.path}/none.oxv').then((_) => 'ok', onError: (_) => 'refused')), 'refused');
    Future<VectorInstallReport> install(String name) {
      final seg = '${tmp.path}/$name.oxv';
      expandOxv('$fixtures/$name.oxv.zst', seg);
      return writer.install(seg, manifestOf(name));
    }

    final r1 = await install('base-v1');
    expect((r1.kind, r1.generation, r1.libraryVersion), ('base', 1, 1));
    expect(writer.installedVersion(), 1);
    await install('delta-v1-v2');
    final r3 = await install('delta-v2-v3');
    expect((r3.kind, r3.generation, r3.libraryVersion, r3.segments), ('delta', 3, 3, 3));
    expect(r3.tombstonesApplied, 1);
    final cur = jsonDecode(File('$setDir/CURRENT').readAsStringSync());
    expect(cur['generation'], 3);
    expect(jsonDecode(File('$setDir/PREVIOUS').readAsStringSync())['generation'], 2);
    expect(File('$setDir/gen-000003/${'33' * 16}.links').existsSync(), isTrue);
    expect(Directory('$setDir/gen-000001').existsSync(), isFalse);
    final set = jsonDecode(File('$setDir/${cur['set']}').readAsStringSync());
    expect((set['stats']['slots'] as int) - (set['stats']['slots_dead'] as int), 3);
    // Otzaria's compaction policy on this set: 3 segments, deltas vs base bytes, dead slots
    final sizes = [for (final g in set['segments'] as List) g['size'] as int];
    final deltas = sizes.skip(1).fold<int>(0, (a, b) => a + b);
    final slots = set['stats']['slots'] as int, dead = set['stats']['slots_dead'] as int;
    expect(r3.needsCompaction,
        sizes.length > 4 || deltas > 0.2 * sizes.first || dead > 0.05 * slots);

    // compaction in the engine: a quiet policy leaves it, a forced one merges the set
    final quiet = await worker.compactVectorSet(setDir,
        policy: const VectorCompactionPolicy(maxSegments: 10, maxDeltaRatio: 100, maxDeadRatio: 1));
    expect((quiet.compacted, quiet.generation), (false, 3));
    final c = await worker.compactVectorSet(setDir, policy: const VectorCompactionPolicy(force: true));
    expect((c.compacted, c.generation, c.slotsAfter), (true, 4, 3));
    final after = jsonDecode(File('$setDir/${jsonDecode(File('$setDir/CURRENT').readAsStringSync())['set']}')
        .readAsStringSync());
    expect([for (final g in after['segments'] as List) g['kind']], ['compacted']);
    expect(after['library_version'], 3);
    final token = SearchCancelToken()..cancel();
    expect(() => compactVectorSet(setDir, policy: const VectorCompactionPolicy(force: true), cancel: token),
        throwsA(isA<SearchCancelledException>()));
  });

  test('a corrupted block is refused; the Dart writer installs a base without native', () async {
    if (native == null) {
      markTestSkipped('native library not built');
      return;
    }
    SeforimNative.load(native);
    final plain = '${tmp.path}/b.oxv';
    expandOxv('$fixtures/base-v1.oxv.zst', plain);
    final manifestJson = File('$fixtures/base-v1.manifest.json').readAsStringSync();
    expect(verifyOxv(plain, manifestJson: manifestJson)['manifest_checked'], isTrue);
    final bytes = File(plain).readAsBytesSync();
    // flip a byte of the VECTORS section (0x0012, block-summed: only its block CRC sees it)
    final h = ByteData.sublistView(bytes);
    final at = [
      for (var i = 0; i < h.getUint32(28, Endian.little); i++) 224 + i * 40
    ].firstWhere((e) => h.getUint16(e, Endian.little) == 0x0012);
    bytes[h.getUint64(at + 8, Endian.little)] ^= 0x40;
    final bad = File('${tmp.path}/bad.oxv')..writeAsBytesSync(bytes);
    expect(() => verifyOxv(bad.path),
        throwsA(predicate((Object e) => vectorInstallErrorKind(e) == 'corrupt')));

    // the release must be of the query model; a cancelled install leaves nothing
    final setDir = '${tmp.path}/n';
    expect(
        () => installVectorSegment(setDir, plain, manifestJson,
            expectedModelJson: '{"family_id":"g","chunking_identity":7}'),
        throwsA(predicate((Object e) => vectorInstallErrorKind(e) == 'incompatible')));
    expect(() => installVectorSegment(setDir, plain, manifestJson, publishedManifestSha256: '0' * 64),
        throwsA(predicate((Object e) => vectorInstallErrorKind(e) == 'not_published')));
    final token = SearchCancelToken()..cancel();
    expect(() => installVectorSegment(setDir, plain, manifestJson, cancel: token),
        throwsA(isA<SearchCancelledException>()));
    expect(File('$setDir/CURRENT').existsSync(), isFalse);
    expect(Directory('$setDir/staging').existsSync(), isFalse);

    final dartWriter = VectorSetWriter('${tmp.path}/d', native: false);
    final r = await dartWriter.install(plain, manifestJson);
    expect((r.generation, r.libraryVersion), (1, 1));
    final delta = '${tmp.path}/x.oxv';
    expandOxv('$fixtures/delta-v1-v2.oxv.zst', delta);
    expect(() => dartWriter.install(delta, manifestOf('delta-v1-v2')),
        throwsA(isA<VectorSetException>()));
  });

}
