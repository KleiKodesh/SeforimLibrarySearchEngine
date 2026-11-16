// Vector set install / verify on disk (issue #18): the set layout of Otzaria's
// sidecar (otzaria-semantic-search segment_set/{install,files,mod}.rs). The host
// finds and downloads a release itself (release lookup, URLs, base / delta
// choice are app logic, not this library's); it hands the downloaded segment
// (expanded with `expandOxv`) and its manifest to [installVectorSegment] /
// [VectorSetWriter.install], which build the set directory the engine opens:
//
//   <dir>/segments/<id>.oxv, <dir>/segments/<id>.package.json
//   <dir>/gen-NNNNNN/set.json, <dir>/gen-NNNNNN/<id>.del
//   <dir>/CURRENT, <dir>/PREVIOUS ({"generation","set","set_sha256"})
//   <dir>/.lock (one install at a time)
//
// The install itself runs in the engine (`se_vectors_install`, a port of the
// sidecar's `install_package`: block verification, a delta's tombstones,
// duplicates and foreign links); `VectorSetWriter(native: false)` keeps a Dart
// port for a base only.
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';

import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'async_search.dart' show SearchWorker;
import 'index.dart' show SearchCancelToken;
import 'native.dart';
/// Checks a staged segment before the Dart writer installs it: throw to refuse it.
typedef VectorSegmentVerifier = Future<void> Function(String segmentPath, String manifestJson);

/// Verifies a plain `.oxv` in the engine (`se_oxv_verify`, Otzaria's sidecar
/// `check_header` + `verify_blocks`): header and directory, every 1 MiB block
/// CRC and, with [manifestJson], the header against its release manifest.
/// Returns the report JSON; throws [SeforimEngineException] when it fails and
/// [SearchCancelledException] when [cancel] (or the token at [cancelAddress], a
/// worker's) was cancelled.
Map<String, dynamic> verifyOxv(String path, {String? manifestJson, SearchCancelToken? cancel, int? cancelAddress}) {
  final n = SeforimNative.instance;
  return jsonDecode(using((al) {
    final (p, pl) = utf8Native(path, al);
    final (m, ml) = manifestJson == null ? (nullptr.cast<Uint8>(), 0) : utf8Native(manifestJson, al);
    return n.takeString(n.oxvVerify(p, pl, m, ml, _token(cancel, cancelAddress)));
  })) as Map<String, dynamic>;
}

/// Installs a downloaded plain `.oxv` into the set directory [setDir] in the
/// engine (`se_vectors_install`, Otzaria's sidecar `install_package`): a base
/// starts a new generation, a delta applies its tombstones, duplicates and
/// foreign links to the current one. A segment inside `<setDir>/incoming/` is
/// moved, any other copied. [expectedModelJson]: the query model's model.json,
/// which the release must be of. [cancel] stops it between blocks, hashed chunks
/// and slot ranges, leaving the set as it was ([SearchCancelledException]).
VectorInstallReport installVectorSegment(String setDir, String segmentPath, String manifestJson,
    {String? publishedManifestSha256, String? expectedModelJson, SearchCancelToken? cancel, int? cancelAddress}) {
  final n = SeforimNative.instance;
  return VectorInstallReport.fromJson(jsonDecode(using((al) {
    final (d, dl) = utf8Native(setDir, al);
    final (s, sl) = utf8Native(segmentPath, al);
    final (m, ml) = utf8Native(manifestJson, al);
    final (h, hl) = publishedManifestSha256 == null
        ? (nullptr.cast<Uint8>(), 0)
        : utf8Native(publishedManifestSha256, al);
    final (j, jl) = expectedModelJson == null ? (nullptr.cast<Uint8>(), 0) : utf8Native(expectedModelJson, al);
    return n.takeString(n.vectorsInstall(d, dl, s, sl, m, ml, h, hl, j, jl, _token(cancel, cancelAddress)));
  })) as Map<String, dynamic>);
}

/// Why the engine refused an install / verify ([installVectorSegment], [verifyOxv]),
/// as Otzaria's sidecar classifies it for the app: `corrupt` (damaged package or set),
/// `incompatible` (a sound release this set or build cannot take, a release of another
/// model), `not_published` (not the published manifest), `busy` (the set's lock is
/// held), `no_space`, `invalid` (a bad argument), `internal`. Null for anything that is
/// not an install error (a cancellation is [SearchCancelledException]).
String? vectorInstallErrorKind(Object error) {
  if (error is! SeforimEngineException) return null;
  return RegExp(r'vector install \[([a-z_]+)\]').firstMatch(error.message)?.group(1);
}

Pointer<Void> _token(SearchCancelToken? cancel, int? address) =>
    cancel?.handle ?? (address == null ? nullptr : Pointer<Void>.fromAddress(address));

class VectorSetException implements Exception {
  final String message;
  const VectorSetException(this.message);
  @override
  String toString() => 'VectorSetException: $message';
}

/// What an install did.
class VectorInstallReport {
  final String kind;
  final int libraryVersion, generation, segments, slotsAdded, bytesOnDisk;
  final bool alreadyApplied;

  /// Delta only (0 otherwise): older slots a tombstone retired, older copies of a
  /// key the delta ships again, foreign records no older live slot holds.
  final int tombstonesApplied, duplicatesRemoved, foreignUnresolved;

  /// Otzaria's default compaction policy on the installed set: more than 4
  /// segments, deltas over 20 % of the base's bytes, or over 5 % of slots dead.
  final bool needsCompaction;
  const VectorInstallReport(this.kind, this.libraryVersion, this.generation, this.segments,
      this.slotsAdded, this.bytesOnDisk, this.alreadyApplied,
      {this.tombstonesApplied = 0,
      this.duplicatesRemoved = 0,
      this.foreignUnresolved = 0,
      this.needsCompaction = false});

  factory VectorInstallReport.fromJson(Map<String, dynamic> j) => VectorInstallReport(
      j['kind'] as String,
      j['library_version'] as int,
      j['generation'] as int,
      j['segments'] as int,
      j['slots_added'] as int,
      j['bytes_on_disk'] as int,
      j['already_applied'] as bool,
      tombstonesApplied: j['tombstones_applied'] as int,
      duplicatesRemoved: j['duplicates_removed'] as int,
      foreignUnresolved: j['foreign_unresolved'] as int,
      needsCompaction: j['needs_compaction'] as bool);
}

/// A set directory as the sidecar lays it out. [install] goes through the
/// engine ([installVectorSegment]: base and delta, the segment verified) unless
/// [native] is false: then the Dart port of `install_package` /
/// `NewGeneration::commit` installs a base (checked by [verifySegment] when
/// given, else by its SHA-256 only) and a delta is refused.
class VectorSetWriter {
  static const releaseFormat = 'otzaria-vectors-release';
  static const setFormat = 'otzaria-vector-set';

  final String dir;
  final bool native;
  final VectorSegmentVerifier? verifySegment;

  /// The worker isolate the engine's install runs on ([SearchWorker.installVectorSegment]);
  /// `null`: on the calling isolate (blocks it while the segment is hashed).
  final SearchWorker? worker;

  VectorSetWriter(this.dir, {this.native = true, this.verifySegment, this.worker});

  String get incoming => '$dir${Platform.pathSeparator}incoming';
  String _p(String rel) => '$dir/$rel';

  /// The CURRENT generation's set.json, or `null` when nothing is installed or it
  /// does not read (or its hash is not the pointer's).
  /// Falls back to PREVIOUS when CURRENT does not read, as the sidecar's open.
  Map<String, dynamic>? currentSet() =>
      _readGeneration(_readPointer('CURRENT')) ?? _readGeneration(_readPointer('PREVIOUS'));

  /// The library version the set stands at, or `null`.
  int? installedVersion() => currentSet()?['library_version'] as int?;

  /// Installs [segmentPath] (a plain `.oxv` that [manifestJson] describes) as a
  /// new generation. A segment inside [incoming] is moved, any other copied.
  ///
  /// Through the engine: [expectedModelJson] (the query model's model.json) is
  /// checked against the release, and [cancel] stops it, leaving the set as it was.
  Future<VectorInstallReport> install(String segmentPath, String manifestJson,
      {String? publishedManifestSha256,
      bool Function()? isCancelled,
      String? expectedModelJson,
      SearchCancelToken? cancel}) async {
    if (native) {
      final w = worker;
      if (w != null) {
        return w.installVectorSegment(dir, segmentPath, manifestJson,
            publishedManifestSha256: publishedManifestSha256, expectedModelJson: expectedModelJson, cancel: cancel);
      }
      return installVectorSegment(dir, segmentPath, manifestJson,
          publishedManifestSha256: publishedManifestSha256, expectedModelJson: expectedModelJson, cancel: cancel);
    }
    await Directory(dir).create(recursive: true);
    final lock = await File(_p('.lock')).open(mode: FileMode.append);
    try {
      try {
        await lock.lock(FileLock.exclusive);
      } on FileSystemException {
        throw const VectorSetException('another install of this vector set is running');
      }
      return await _install(segmentPath, manifestJson, publishedManifestSha256, isCancelled);
    } finally {
      try {
        await lock.unlock();
      } on FileSystemException {
        // not locked
      }
      await lock.close();
    }
  }

  Future<VectorInstallReport> _install(String segmentPath, String manifestJson,
      String? published, bool Function()? isCancelled) async {
    if (published != null &&
        sha256.convert(utf8.encode(manifestJson)).toString() != published) {
      throw const VectorSetException('the manifest is not the published one');
    }
    final m = jsonDecode(manifestJson);
    if (m is! Map<String, dynamic>) throw const VectorSetException('manifest shape');
    if (m['format'] != releaseFormat || m['formatVersion'] != 1) {
      throw VectorSetException('manifest is ${m['format']} ${m['formatVersion']}, '
          'this reads $releaseFormat 1');
    }
    final id = m['segmentId'];
    if (id is! String || !_isSegmentId(id)) {
      throw const VectorSetException('a segment id is 32 lowercase hex digits');
    }
    final kind = m['kind'];
    if (kind == 'compacted') {
      throw const VectorSetException('a compacted segment is never published');
    }
    if (kind != 'base' && kind != 'delta') throw VectorSetException('kind $kind');
    final seg = m['segment'];
    final counts = m['counts'];
    final identity = m['identity'];
    if (seg is! Map || seg['sha256'] is! String || seg['size'] is! int ||
        counts is! Map || counts['slots'] is! int || identity is! Map<String, dynamic> ||
        m['identityDigest'] is! String || m['codecParamsSha256'] is! String ||
        m['toLibraryVersion'] is! int || m['libraryReleaseTag'] is! String) {
      throw const VectorSetException('manifest fields missing');
    }
    final to = m['toLibraryVersion'] as int;
    final current = currentSet();
    if (kind == 'delta') {
      if (current == null) {
        throw const VectorSetException('a delta needs an installed base');
      }
      if (m['identityDigest'] != current['identity_digest'] ||
          m['codecParamsSha256'] != current['codec_params_sha256']) {
        throw const VectorSetException('the delta is not of the installed set\'s identity');
      }
      final stands = current['library_version'] as int;
      if (to <= stands) {
        return VectorInstallReport('delta', stands, current['generation'] as int,
            (current['segments'] as List).length, 0, current['stats']['bytes'] as int, true);
      }
      if (m['fromLibraryVersion'] != stands) {
        throw VectorSetException(
            'the delta goes from ${m['fromLibraryVersion']}, the set stands at $stands');
      }
      throw const VectorSetException('a delta is installed by the engine (native: true)');
    }

    // Stage (move from incoming/, else copy), then check size + hash + segment.
    final source = File(segmentPath);
    final inIncoming = _same(source.parent.path, incoming);
    String staged;
    if (inIncoming) {
      staged = source.path;
    } else {
      final staging = Directory(_p('staging'))..createSync(recursive: true);
      staged = '${staging.path}/$id.oxv';
      await source.copy(staged);
    }
    final size = await File(staged).length();
    if (size != seg['size']) {
      throw VectorSetException('the segment is $size bytes, its manifest says ${seg['size']}');
    }
    final sha = await sha256OfFile(staged, isCancelled: isCancelled);
    if (sha != seg['sha256']) throw VectorSetException('segment sha256 $sha != ${seg['sha256']}');
    await verifySegment?.call(staged, manifestJson);

    // Place the segment (kept when the same bytes are already there).
    await Directory(_p('segments')).create(recursive: true);
    final target = File(_p('segments/$id.oxv'));
    if (await target.exists()) {
      final held = await sha256OfFile(target.path, isCancelled: isCancelled);
      if (held != sha) {
        final live = [_readPointer('CURRENT'), _readPointer('PREVIOUS')]
            .map(_readGeneration)
            .whereType<Map<String, dynamic>>()
            .any((d) => (d['segments'] as List).any((s) => s['id'] == id && s['sha256'] == held));
        if (live) throw VectorSetException('segment id $id is taken by other bytes');
        await File(staged).rename(target.path);
      } else if (staged != source.path) {
        await File(staged).delete();
      }
    } else {
      await File(staged).rename(target.path);
    }
    _writeAtomically(_p('segments/$id.package.json'), utf8.encode(manifestJson));

    // New generation.
    final generation = _nextGeneration();
    final genName = 'gen-${generation.toString().padLeft(6, '0')}';
    final genDir = Directory(_p(genName))..createSync();
    final entry = <String, dynamic>{
      'id': id,
      'file': 'segments/$id.oxv',
      'kind': kind,
      'from': m['fromLibraryVersion'] ?? 0,
      'to': to,
      'sha256': seg['sha256'],
      'size': seg['size'],
      'slots': counts['slots'],
      if (m['packageDigest'] is String) 'package_digest': m['packageDigest'],
      'provenance': m['provenance'],
    };
    List<Map<String, dynamic>> segments;
    Map<String, dynamic> stats;
    {
      final slots = counts['slots'] as int;
      final (bytes, crc) = encodeDel(slots, Uint64List((slots + 63) ~/ 64));
      _writeAtomically('${genDir.path}/$id.del', bytes);
      entry['del'] = {'file': '$id.del', 'crc32': crc, 'count': 0};
      segments = [entry];
      stats = {
        'slots': slots,
        'slots_dead': 0,
        'foreign': 0,
        'foreign_unresolved': 0,
        'bytes': seg['size'],
      };
    }
    final document = <String, dynamic>{
      'format': setFormat,
      'format_version': 1,
      'generation': generation,
      'identity': identity,
      'identity_digest': m['identityDigest'],
      'codec_params_sha256': m['codecParamsSha256'],
      'library_version': to,
      'library_release_tag': m['libraryReleaseTag'],
      'segments': segments,
      'stats': stats,
      'created_at': _utcTimestamp(DateTime.now().toUtc()),
    };
    final setBytes = utf8.encode(const JsonEncoder.withIndent('  ').convert(document));
    _writeAtomically('${genDir.path}/set.json', setBytes);
    final pointer = {
      'generation': generation,
      'set': '$genName/set.json',
      'set_sha256': sha256.convert(setBytes).toString(),
    };

    // PREVIOUS = the old CURRENT when a host can fall back to it, then flip CURRENT.
    var oldCurrent = _readPointer('CURRENT');
    var oldDoc = _readGeneration(oldCurrent);
    if (oldDoc == null) {
      oldCurrent = _readPointer('PREVIOUS');
      oldDoc = _readGeneration(oldCurrent);
    }
    if (oldCurrent != null &&
        oldDoc != null &&
        fallsBackTo(identity, oldDoc['identity'] as Map<String, dynamic>)) {
      _writeAtomically(_p('PREVIOUS'), utf8.encode(jsonEncode(oldCurrent)));
    }
    _writeAtomically(_p('CURRENT'), utf8.encode(jsonEncode(pointer)));
    final prev = _readPointer('PREVIOUS');
    final prevDoc = _readGeneration(prev);
    if (prev != null && (prevDoc == null || !fallsBackTo(identity, prevDoc['identity']))) {
      File(_p('PREVIOUS')).deleteSync();
    }
    collectGarbage();
    return VectorInstallReport(kind, to, generation, segments.length, counts['slots'] as int,
        stats['bytes'] as int, false);
  }

  /// Removes generations and segment files neither CURRENT nor PREVIOUS names
  /// (the sidecar's `collect_garbage`); an unreadable pointer stops it.
  void collectGarbage() {
    for (final name in ['CURRENT', 'PREVIOUS']) {
      if (File(_p(name)).existsSync() && _readPointer(name) == null) return;
    }
    final pointers = [_readPointer('CURRENT'), _readPointer('PREVIOUS')];
    if (pointers[0] == null && pointers[1] != null) return;
    final liveGen = <String>{}, liveSeg = <String>{};
    for (final p in pointers) {
      if (p == null) continue;
      final d = _readGeneration(p);
      if (d == null) return;
      liveGen.add('gen-${(p['generation'] as int).toString().padLeft(6, '0')}');
      liveSeg.addAll((d['segments'] as List).map((s) => s['id'] as String));
    }
    for (final e in Directory(dir).listSync()) {
      final name = e.uri.pathSegments.where((s) => s.isNotEmpty).last;
      if (e is Directory && name.startsWith('gen-') && !liveGen.contains(name)) {
        try {
          e.deleteSync(recursive: true);
        } on FileSystemException {
          // stays for the next install
        }
      }
    }
    final segs = Directory(_p('segments'));
    if (!segs.existsSync()) return;
    for (final e in segs.listSync()) {
      final name = e.uri.pathSegments.last;
      if (!liveSeg.contains(name.split('.').first)) {
        try {
          e.deleteSync();
        } on FileSystemException {
          // stays for the next install
        }
      }
    }
  }

  /// Whether a host opening [newer] can fall back to a set of [older] (sidecar
  /// `falls_back_to`): a query package in common, the fp32 passage package kept
  /// when [newer] has a query length, and otherwise only the query packages or
  /// the store differ.
  static bool fallsBackTo(Map<String, dynamic> newer, Map<String, dynamic> older) {
    final nm = (newer['model'] as Map?) ?? const {};
    final om = (older['model'] as Map?) ?? const {};
    final np = (nm['query_packages'] as List?) ?? const [];
    final op = (om['query_packages'] as List?) ?? const [];
    final sharesPackage = np.any((a) => op.any((b) => b['checksum'] == a['checksum']));
    final sharesPassage = nm['query_max_tokens'] == null ||
        np.any((a) => a['quantization'] == 'fp32' && op.any((b) => _jsonEq(a, b)));
    final nmRest = Map.of(nm)..remove('query_packages');
    final omRest = Map.of(om)..remove('query_packages');
    final rest = {...newer.keys, ...older.keys}..removeAll(['model', 'store']);
    return sharesPackage &&
        sharesPassage &&
        rest.every((k) => _jsonEq(newer[k], older[k])) &&
        _jsonEq(nmRest, omRest);
  }

  Map<String, dynamic>? _readPointer(String name) {
    try {
      final j = jsonDecode(File(_p(name)).readAsStringSync());
      if (j is Map<String, dynamic> && j['generation'] is int && j['set'] is String &&
          j['set_sha256'] is String) {
        return j;
      }
    } on Object {
      // missing or unreadable
    }
    return null;
  }

  Map<String, dynamic>? _readGeneration(Map<String, dynamic>? pointer) {
    if (pointer == null) return null;
    try {
      final bytes = File(_p(pointer['set'] as String)).readAsBytesSync();
      if (sha256.convert(bytes).toString() != pointer['set_sha256']) return null;
      final d = jsonDecode(utf8.decode(bytes));
      if (d is Map<String, dynamic> && d['format'] == setFormat &&
          d['generation'] == pointer['generation']) {
        return d;
      }
    } on Object {
      // missing or unreadable
    }
    return null;
  }

  int _nextGeneration() {
    var highest = 0;
    for (final e in Directory(dir).listSync()) {
      final m = RegExp(r'^gen-(\d+)$').firstMatch(e.uri.pathSegments.where((s) => s.isNotEmpty).last);
      if (m != null) highest = highest > int.parse(m[1]!) ? highest : int.parse(m[1]!);
    }
    for (final p in [_readPointer('CURRENT'), _readPointer('PREVIOUS')]) {
      final g = p?['generation'] as int?;
      if (g != null && g > highest) highest = g;
    }
    return highest + 1;
  }

  static void _writeAtomically(String path, List<int> bytes) {
    final tmp = File('$path.$pid-${DateTime.now().microsecondsSinceEpoch}.tmp');
    final raf = tmp.openSync(mode: FileMode.write);
    try {
      raf.writeFromSync(bytes);
      raf.flushSync();
    } finally {
      raf.closeSync();
    }
    tmp.renameSync(path);
  }
}

/// SHA-256 of a file, streamed.
Future<String> sha256OfFile(String path, {bool Function()? isCancelled}) async {
  final out = _DigestSink();
  final input = sha256.startChunkedConversion(out);
  await for (final chunk in File(path).openRead()) {
    if (isCancelled?.call() ?? false) {
      throw const VectorSetException('cancelled');
    }
    input.add(chunk);
  }
  input.close();
  return out.value.toString();
}

/// A `.del` file (sidecar `Deleted::encode`): `OXVDEL1\n`, slots u64, dead u64,
/// the bitmap words u64, all little-endian, then the CRC-32 of all that.
(Uint8List, int) encodeDel(int slots, Uint64List words) {
  var dead = 0;
  for (final w in words) {
    var x = w;
    while (x != 0) {
      x &= x - 1;
      dead++;
    }
  }
  final b = BytesBuilder();
  b.add(ascii.encode('OXVDEL1\n'));
  final head = ByteData(16)
    ..setUint64(0, slots, Endian.little)
    ..setUint64(8, dead, Endian.little);
  b.add(head.buffer.asUint8List());
  final body = ByteData(words.length * 8);
  for (var i = 0; i < words.length; i++) {
    body.setUint64(i * 8, words[i], Endian.little);
  }
  b.add(body.buffer.asUint8List());
  final crc = crc32(b.toBytes());
  b.add((ByteData(4)..setUint32(0, crc, Endian.little)).buffer.asUint8List());
  return (b.takeBytes(), crc);
}

final _crcTable = List<int>.generate(256, (n) {
  var c = n;
  for (var k = 0; k < 8; k++) {
    c = (c & 1) != 0 ? 0xEDB88320 ^ (c >> 1) : c >> 1;
  }
  return c;
});

/// CRC-32 (IEEE), as crc32fast.
int crc32(List<int> bytes) {
  var c = 0xFFFFFFFF;
  for (final x in bytes) {
    c = _crcTable[(c ^ x) & 0xFF] ^ (c >> 8);
  }
  return c ^ 0xFFFFFFFF;
}

class _DigestSink implements Sink<Digest> {
  late Digest value;
  @override
  void add(Digest data) => value = data;
  @override
  void close() {}
}


bool _isSegmentId(String id) => RegExp(r'^[0-9a-f]{32}$').hasMatch(id);

bool _same(String a, String b) {
  try {
    return Directory(a).resolveSymbolicLinksSync().toLowerCase() ==
        Directory(b).resolveSymbolicLinksSync().toLowerCase();
  } on FileSystemException {
    return false;
  }
}

bool _jsonEq(Object? a, Object? b) => jsonEncode(_canon(a)) == jsonEncode(_canon(b));
Object? _canon(Object? v) => v is Map
    ? {for (final k in (v.keys.map((k) => '$k').toList()..sort())) k: _canon(v[k])}
    : v is List
        ? v.map(_canon).toList()
        : v;

String _utcTimestamp(DateTime t) {
  String two(int n) => n.toString().padLeft(2, '0');
  return '${t.year.toString().padLeft(4, '0')}-${two(t.month)}-${two(t.day)}'
      'T${two(t.hour)}:${two(t.minute)}:${two(t.second)}Z';
}

