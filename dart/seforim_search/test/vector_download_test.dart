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
  final oxv = File('../../seforim-vectors/tests/data/synthetic.oxv').readAsBytesSync();
  final zst = File('test/fixtures/synthetic.oxv.zst').readAsBytesSync();
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

  test('download resumes, verifies, expands and installs; generations roll and GC', () async {
    int Function(String, String) expand;
    if (native != null) {
      SeforimNative.load(native);
      expand = expandOxv;
    } else {
      // No native build here: the fixture's plain bytes stand in for the expansion.
      expand = (s, d) => (File(d)..writeAsBytesSync(oxv)).lengthSync();
    }
    publishBase(1, id1, parts: 2);
    final setDir = '${tmp.path}/vectors';
    final writer = VectorSetWriter(setDir);
    final updater = VectorSetUpdater(writer, locator: locator(), expand: expand);

    // A half download of the first part, from the same asset, is resumed.
    final target = (await locator().findForLibraryVersion(1, candidates: candidates))!;
    final p0 = target.files.first;
    Directory(writer.incoming).createSync(recursive: true);
    File('${writer.incoming}/${p0.name}.partial')
        .writeAsBytesSync(gh.assets['/dl/vectors-qa491-windows-v1-20261101/${p0.name}']!.sublist(0, 10));
    File('${writer.incoming}/${p0.name}.partial.id')
        .writeAsStringSync('${p0.assetId}:${p0.sha256}:${p0.size}');

    final r1 = (await updater.update(1, candidates: candidates))!;
    expect(gh.ranges, ['bytes=10-']);
    expect(r1.generation, 1);
    expect(r1.libraryVersion, 1);
    expect(File('$setDir/segments/$id1.oxv').readAsBytesSync(), oxv);
    expect(File('$setDir/segments/$id1.package.json').existsSync(), isTrue);
    expect(Directory(writer.incoming).listSync(), isEmpty);
    final cur = jsonDecode(File('$setDir/CURRENT').readAsStringSync());
    final setBytes = File('$setDir/${cur['set']}').readAsBytesSync();
    expect(cur['set_sha256'], _sha(setBytes));
    final set = jsonDecode(utf8.decode(setBytes));
    expect(set['format'], 'otzaria-vector-set');
    expect(set['segments'][0]['del']['file'], '$id1.del');
    expect(File('$setDir/gen-000001/$id1.del').lengthSync(), 24 + 2 * 8 + 4);
    expect(writer.installedVersion(), 1);

    // Same version again: nothing to do.
    expect((await updater.update(1, candidates: candidates))!.alreadyApplied, isTrue);

    // v2 (new segment id): gen 2 current, gen 1 previous; v3: gen 1 collected.
    const id2 = '22222222222222222222222222222222', id3 = '33333333333333333333333333333333';
    publishBase(2, id2);
    expect((await updater.update(2, candidates: candidates))!.generation, 2);
    expect(jsonDecode(File('$setDir/PREVIOUS').readAsStringSync())['generation'], 1);
    publishBase(3, id3);
    expect((await updater.update(3, candidates: candidates))!.generation, 3);
    expect(Directory('$setDir/gen-000001').existsSync(), isFalse);
    expect(File('$setDir/segments/$id1.oxv').existsSync(), isFalse);
    expect(File('$setDir/segments/$id2.oxv').existsSync(), isTrue);
  });

  test('a delta is refused without the native applier; a bad download is dropped', () async {
    final setDir = '${tmp.path}/v';
    final writer = VectorSetWriter(setDir);
    final seg = File('${tmp.path}/s.oxv')..writeAsBytesSync(oxv);
    final base = gh.manifest(
        kind: 'base', from: 0, to: 1, tag: 'v1-x', segmentId: id1, segment: oxv, files: []);
    await writer.install(seg.path, jsonEncode(base));
    final delta = gh.manifest(kind: 'delta', from: 1, to: 2, tag: 'v2-x',
        segmentId: '22222222222222222222222222222222', segment: oxv, files: []);
    expect(() => writer.install(seg.path, jsonEncode(delta)), throwsA(isA<VectorSetException>()));

    final f = VectorFile('x', '${gh.base}/missing-but-404', 3, 'ab' * 32, '1');
    gh.assets['/missing-but-404'] = Uint8List.fromList([1, 2, 3]);
    expect(() => VectorDownloader().download(f, '${tmp.path}/x'),
        throwsA(isA<VectorReleaseException>().having((e) => e.kind, 'k', VectorFailure.checksum)));
  });
}
