import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

// Fake GitHub releases API + asset host on loopback; tiny fixtures only.

const family = VectorFamily('vectors-qa491-windows-',
    deltaReleaseTagPrefix: 'vectors-qa491-windows-delta-');
const modelId = 'test/model';
const chunking = 320260894818558297;
const digest = 'aa00000000000000000000000000000000000000000000000000000000000000';

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

String _sha(List<int> b) => sha256.convert(b).toString();

class FakeGithub {
  late HttpServer server;
  final assets = <String, Uint8List>{}; // path -> bytes
  final json = <String, Object>{}; // path -> JSON
  final ranges = <String>[];
  String get base => 'http://127.0.0.1:${server.port}';

  Future<void> start() async {
    server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    server.listen((rq) async {
      final path = rq.uri.toString();
      final r = rq.response;
      if (json.containsKey(path)) {
        r.write(jsonEncode(json[path]));
      } else if (assets.containsKey(path)) {
        var bytes = assets[path]!;
        final range = rq.headers.value(HttpHeaders.rangeHeader);
        if (range != null) {
          ranges.add(range);
          final from = int.parse(range.substring(6, range.length - 1));
          bytes = Uint8List.sublistView(bytes, from);
          r.statusCode = 206;
        }
        r.add(bytes);
      } else {
        r.statusCode = 404;
      }
      await r.close();
    });
  }

  Map<String, dynamic> manifest(
      {required String kind, required int from, required int to, required String tag,
      required String segmentId, required List<int> segment, required List<Map> files,
      String fam = modelId}) => {
        'format': 'otzaria-vectors-release',
        'formatVersion': 1,
        'kind': kind,
        'identity': {
          'text': {'line_text_version': 4, 'key_version': 1},
          'model': {
            'family_id': fam,
            'chunking_identity': chunking,
            'query_max_tokens': 128,
            'query_packages': [
              {'checksum': 'c1', 'quantization': 'int8'},
              {'checksum': 'c2', 'quantization': 'fp32'},
            ],
          },
          'store': {'backend_id': 'otzaria-oxv'},
        },
        'identityDigest': digest,
        'codecParamsSha256': 'bb',
        if (kind == 'delta') 'fromLibraryVersion': from,
        'toLibraryVersion': to,
        'libraryReleaseTag': tag,
        'segmentId': segmentId,
        'counts': {'slots': 100},
        'segment': {'sha256': _sha(segment), 'size': segment.length},
        'packageDigest': 'cc',
        'provenance': {'worker': 'test'},
        'files': files,
      };

  /// Publishes a release: its compressed bytes split in [parts].
  void publish(String releaseTag, String libraryTag, Map<String, dynamic> manifestBase,
      Uint8List compressed, {int parts = 1, bool badNotes = false, bool delta = false}) {
    final m = manifestBase;
    final step = (compressed.length / parts).ceil();
    final stem = 'otzaria-vectors-aa-${delta ? 'v${m['fromLibraryVersion']}-v${m['toLibraryVersion']}' : 'v${m['toLibraryVersion']}-base'}';
    final files = <Map>[];
    final assetList = <Map>[];
    for (var i = 0; i < parts; i++) {
      final chunk = Uint8List.sublistView(
          compressed, i * step, (i + 1) * step > compressed.length ? compressed.length : (i + 1) * step);
      final name = parts == 1 ? '$stem.oxv.zst' : '$stem.oxv.zst.part-${i.toString().padLeft(3, '0')}';
      final path = '/dl/$releaseTag/$name';
      assets[path] = chunk;
      files.add({'file': name, 'sha256': _sha(chunk), 'size': chunk.length});
      assetList.add({'name': name, 'id': i + 1, 'browser_download_url': '$base$path'});
    }
    m['files'] = files;
    final manifestBytes = utf8.encode(jsonEncode(m));
    final mName = '$stem.manifest.json';
    assets['/dl/$releaseTag/$mName'] = manifestBytes;
    assetList.add({
      'name': mName,
      'browser_download_url': '$base/dl/$releaseTag/$mName',
      'digest': 'sha256:${_sha(manifestBytes)}',
    });
    json['/releases/tags/$releaseTag'] = {
      'tag_name': releaseTag,
      'body': '$mName ${badNotes ? 'f' * 64 : _sha(manifestBytes)}',
      'assets': assetList,
    };
  }
}

void main() {
  late FakeGithub gh;
  late Directory tmp;
  // The locator tests serve made-up manifests: only the release logic runs.
  final oxv = Uint8List(64);
  final zst = Uint8List(247);
  final native = _nativePath();
  final candidates = [const VectorCandidate(family, modelId, chunking)];
  const id1 = '11111111111111111111111111111111';

  setUp(() async {
    gh = FakeGithub();
    await gh.start();
    tmp = Directory.systemTemp.createTempSync('se_vdl_');
    gh.json['/releases?per_page=100&page=1'] = [
      {'tag_name': 'v3-20261103'},
      {'tag_name': 'v2-20261102'},
      {'tag_name': 'v1-20261101'},
    ];
  });
  tearDown(() async {
    await gh.server.close(force: true);
    tmp.deleteSync(recursive: true);
  });

  VectorReleaseLocator locator() => VectorReleaseLocator(apiBase: gh.base);

  void publishBase(int v, String id, {int parts = 1, String fam = modelId, bool badNotes = false}) {
    final tag = 'v$v-2026110$v';
    gh.publish(
        'vectors-qa491-windows-$tag',
        tag,
        gh.manifest(kind: 'base', from: 0, to: v, tag: tag, segmentId: id, segment: oxv,
            files: [], fam: fam),
        zst,
        parts: parts,
        badNotes: badNotes);
  }

  void publishDelta(int from, int to, String id, int size) {
    final tag = 'v$to-2026110$to';
    gh.publish(
        'vectors-qa491-windows-delta-$tag',
        tag,
        gh.manifest(kind: 'delta', from: from, to: to, tag: tag, segmentId: id, segment: oxv,
            files: []),
        Uint8List(size),
        delta: true);
  }

  test('model.json gives the candidate', () {
    final c = VectorCandidate.fromModelJson(
        family, '{"family_id":"x/y","chunking_identity":320260894818558297}');
    expect(c.modelFamilyId, 'x/y');
    expect(c.chunkingIdentity, chunking);
    expect(() => VectorCandidate.fromModelJson(family, '{}'), throwsFormatException);
  });

  test('.del encoding equals the sidecar (CRC of an empty 10043833-slot set)', () {
    final (bytes, crc) = encodeDel(10043833, Uint64List((10043833 + 63) ~/ 64));
    expect(crc, 2484906093);
    expect(bytes.length, 24 + 156935 * 8 + 4);
    expect(ascii.decode(bytes.sublist(0, 8)), 'OXVDEL1\n');
  });

  test('locator: base, wrong model, unpublished manifest, version tags', () async {
    publishBase(3, id1);
    final r = await locator().findForLibraryVersion(3, candidates: candidates);
    expect(r!.libraryTag, 'v3-20261103');
    expect(r.segmentId, id1);
    expect(r.segmentSize, oxv.length);
    expect(await locator().findForLibraryVersion(9, candidates: candidates), isNull);
    expect(
        () => locator().findForLibraryVersion(3,
            candidates: [const VectorCandidate(family, 'other', chunking)]),
        throwsA(isA<VectorReleaseException>()
            .having((e) => e.kind, 'kind', VectorFailure.artifactIncompatible)));
    publishBase(2, id1, badNotes: true);
    expect(
        () => locator().findForLibraryVersion(2, candidates: candidates),
        throwsA(isA<VectorReleaseException>()
            .having((e) => e.kind, 'kind', VectorFailure.artifactNotPublished)));
    expect(VectorReleaseLocator.versionOfTag('v30-20260930165019'), 30);
  });

  test('locator: delta chain oldest first, refused when too large', () async {
    publishBase(3, id1);
    publishDelta(1, 2, '22222222222222222222222222222222', 10);
    publishDelta(2, 3, '33333333333333333333333333333333', 10);
    final target = (await locator().findForLibraryVersion(3, candidates: candidates))!;
    final chain = await locator().findDeltaChain(target: target, installedVersion: 1);
    expect(chain!.map((d) => '${d.fromLibraryVersion}-${d.toLibraryVersion}'), ['1-2', '2-3']);
    expect(await locator().findDeltaChain(target: target, installedVersion: 0), isNull);
    publishDelta(2, 3, '33333333333333333333333333333333', zst.length);
    expect(await locator().findDeltaChain(target: target, installedVersion: 1), isNull);
  });

  // Fixtures written by seforim-vectors (install_tests.rs): base v1, deltas v1-v2
  // and v2-v3, base v3; real segments with block CRCs, ids and manifests.
  final fixtures = '${_repoRoot()}/seforim-vectors/tests/data/install';
  Map<String, dynamic> fixtureManifest(String name) =>
      jsonDecode(File('$fixtures/$name.manifest.json').readAsStringSync()) as Map<String, dynamic>;
  Uint8List fixtureZst(String name, {int pad = 0}) {
    final b = BytesBuilder()..add(File('$fixtures/$name.oxv.zst').readAsBytesSync());
    if (pad > 0) {
      // a zstd skippable frame: makes the download bigger, expands to nothing
      b.add((ByteData(8)
            ..setUint32(0, 0x184D2A50, Endian.little)
            ..setUint32(4, pad, Endian.little))
          .buffer
          .asUint8List());
      b.add(Uint8List(pad));
    }
    return b.takeBytes();
  }

  final fixtureCandidates = [VectorCandidate.fromModelJson(family, '{"family_id":"f","chunking_identity":7}')];
  void publishFixture(String name, {int parts = 1, int pad = 0}) {
    final m = fixtureManifest(name);
    final tag = m['libraryReleaseTag'] as String;
    final delta = m['kind'] == 'delta';
    gh.publish('vectors-qa491-windows-${delta ? 'delta-' : ''}$tag', tag, m, fixtureZst(name, pad: pad),
        parts: parts, delta: delta);
  }

  test('download resumes, verifies, expands and installs; a delta chain applies', () async {
    if (native == null) {
      markTestSkipped('native library not built');
      return;
    }
    SeforimNative.load(native);
    publishFixture('base-v1', parts: 2);
    publishFixture('delta-v1-v2');
    publishFixture('delta-v2-v3');
    publishFixture('base-v3', pad: 8192);
    final setDir = '${tmp.path}/vectors';
    // the engine's install runs on a SearchWorker isolate
    final indexDir = '${tmp.path}/index';
    SeforimIndex.create(indexDir).close();
    final worker = await SearchWorker.spawn(indexDir, nativePath: native);
    addTearDown(worker.close);
    final writer = VectorSetWriter(setDir, worker: worker);
    final updater = VectorSetUpdater(writer, locator: locator());
    expect((await worker.verifyOxv('${tmp.path}/none.oxv').then((_) => 'ok', onError: (_) => 'refused')), 'refused');

    // A half download of the first part, from the same asset, is resumed.
    final target = (await locator().findForLibraryVersion(1, candidates: fixtureCandidates))!;
    final p0 = target.files.first;
    Directory(writer.incoming).createSync(recursive: true);
    File('${writer.incoming}/${p0.name}.partial')
        .writeAsBytesSync(gh.assets['/dl/vectors-qa491-windows-v1-20261101/${p0.name}']!.sublist(0, 10));
    File('${writer.incoming}/${p0.name}.partial.id').writeAsStringSync('${p0.assetId}:${p0.sha256}:${p0.size}');

    final r1 = (await updater.update(1, candidates: fixtureCandidates))!;
    expect(gh.ranges, ['bytes=10-']);
    expect((r1.kind, r1.generation, r1.libraryVersion), ('base', 1, 1));
    expect(Directory(writer.incoming).listSync(), isEmpty);
    expect((await updater.update(1, candidates: fixtureCandidates))!.alreadyApplied, isTrue);

    // v1 -> v3: two deltas (smaller than half the padded base) applied in the engine
    final r3 = (await updater.update(3, candidates: fixtureCandidates))!;
    expect((r3.kind, r3.generation, r3.libraryVersion, r3.segments), ('delta', 3, 3, 3));
    expect(r3.tombstonesApplied, 1);
    final cur = jsonDecode(File('$setDir/CURRENT').readAsStringSync());
    expect(cur['generation'], 3);
    expect(jsonDecode(File('$setDir/PREVIOUS').readAsStringSync())['generation'], 2);
    expect(File('$setDir/gen-000003/${'33' * 16}.links').existsSync(), isTrue);
    expect(Directory('$setDir/gen-000001').existsSync(), isFalse);

    // the engine's set reader opens what was installed
    final set = jsonDecode(File('$setDir/${cur['set']}').readAsStringSync());
    expect((set['stats']['slots'] as int) - (set['stats']['slots_dead'] as int), 3);
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
    expect(() => verifyOxv(bad.path), throwsA(isA<SeforimEngineException>()));

    // the release must be of the query model; a cancelled install leaves nothing
    final setDir = '${tmp.path}/n';
    expect(
        () => installVectorSegment(setDir, plain, manifestJson,
            expectedModelJson: '{"family_id":"g","chunking_identity":7}'),
        throwsA(isA<SeforimEngineException>()));
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
    expect(() => dartWriter.install(delta, File('$fixtures/delta-v1-v2.manifest.json').readAsStringSync()),
        throwsA(isA<VectorSetException>()));
  });

  test('a bad download is dropped', () async {
    final f = VectorFile('x', '${gh.base}/bad', 3, 'ab' * 32, '1');
    gh.assets['/bad'] = Uint8List.fromList([1, 2, 3]);
    expect(() => VectorDownloader().download(f, '${tmp.path}/x'),
        throwsA(isA<VectorReleaseException>().having((e) => e.kind, 'k', VectorFailure.checksum)));
  });
}
