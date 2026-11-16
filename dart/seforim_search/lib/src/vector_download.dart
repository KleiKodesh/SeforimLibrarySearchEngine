// Host-driven download / update of a published vector set (issue #18).
//
// Ported from Otzaria's app (lib/semantic_search: semantic_release_locator.dart,
// semantic_vectors_family.dart, semantic_vectors_release.dart,
// semantic_download_client.dart, semantic_staged_import.dart) and from the set
// layout of Otzaria's sidecar (otzaria-semantic-search segment_set/{install,
// files,mod}.rs). A reusable library feature: no UI, no app settings. The host
// passes the library version, its query model's model.json and a directory; this
// file finds the release, downloads base or a delta chain into `<dir>/incoming/`,
// verifies, expands (`expandOxv`) and installs it as the set directory the engine
// opens:
//
//   <dir>/segments/<id>.oxv, <dir>/segments/<id>.package.json
//   <dir>/gen-NNNNNN/set.json, <dir>/gen-NNNNNN/<id>.del
//   <dir>/CURRENT, <dir>/PREVIOUS ({"generation","set","set_sha256"})
//   <dir>/incoming/ (downloads), <dir>/.lock (one install at a time)
//
// Native-only steps (the sidecar reads the segment for them) are hooks:
// [VectorSegmentVerifier] (header + block checks, sidecar `check_header` +
// `Segment::verify_blocks`) and [VectorDeltaApplier] (a delta's tombstones,
// duplicates and foreign links resolved against the older segments). Without a
// delta applier a delta is refused with [VectorSetException], never installed
// half-way.
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';

import 'semantic.dart' show expandOxv;

/// Limits on a delta chain instead of a base: at most [maxSteps] deltas, whose
/// downloads together stay under [maxBaseSizeRatio] of the base (Otzaria's
/// `kSemanticDeltaChainLimits`).
const vectorDeltaChainLimits = (maxSteps: 8, maxBaseSizeRatio: 0.5);

/// A family of vector releases: the release tag prefixes of one query model.
class VectorFamily {
  /// `<prefix><libraryTag>` names the base release.
  final String releaseTagPrefix;

  /// `<prefix><libraryTag>` names a delta release; `null` = base only.
  final String? deltaReleaseTagPrefix;

  const VectorFamily(this.releaseTagPrefix, {this.deltaReleaseTagPrefix});

  /// Otzaria's three published families (`semantic_vectors_family.dart`).
  static const qa491Windows = VectorFamily('vectors-qa491-windows-',
      deltaReleaseTagPrefix: 'vectors-qa491-windows-delta-');
  static const qa491 = VectorFamily('vectors-qa491-');
  static const pardesQa491Windows = VectorFamily('vectors-pardesqa491-windows-',
      deltaReleaseTagPrefix: 'vectors-pardesqa491-windows-delta-');

  @override
  bool operator ==(Object other) =>
      other is VectorFamily &&
      other.releaseTagPrefix == releaseTagPrefix &&
      other.deltaReleaseTagPrefix == deltaReleaseTagPrefix;
  @override
  int get hashCode => Object.hash(releaseTagPrefix, deltaReleaseTagPrefix);
}

/// One release a host can use: a family plus the query model's identity
/// (`family_id` and `chunking_identity` of its model.json).
class VectorCandidate {
  final VectorFamily family;
  final String modelFamilyId;
  final int chunkingIdentity;

  const VectorCandidate(this.family, this.modelFamilyId, this.chunkingIdentity);

  /// From the query model's model.json text.
  factory VectorCandidate.fromModelJson(VectorFamily family, String modelJson) {
    final j = jsonDecode(modelJson);
    if (j is! Map<String, dynamic>) {
      throw const FormatException('model.json is not a JSON object');
    }
    final id = j['family_id'];
    final chunking = _chunking(j['chunking_identity'], modelJson);
    if (id is! String || id.isEmpty || chunking == null) {
      throw const FormatException('model.json: family_id / chunking_identity missing');
    }
    return VectorCandidate(family, id, chunking);
  }
}

/// A downloadable file of a release (a whole `.oxv.zst` or one `.part-NNN`).
class VectorFile {
  final String name, downloadUrl, sha256, assetId;
  final int size;
  const VectorFile(this.name, this.downloadUrl, this.size, this.sha256, this.assetId);
}

/// A vector release (base or delta) of one library version, its manifest verified
/// against the published SHA-256.
class VectorRelease {
  final String libraryTag, releaseTag, kind, manifestJson;
  final String publishedManifestSha256;
  final int fromLibraryVersion, toLibraryVersion;
  final List<VectorFile> files;
  final String segmentId, segmentSha256;
  final int segmentSize;
  final String identityDigest, modelFamilyId;
  final int? chunkingIdentity;
  final VectorFamily family;

  const VectorRelease({
    required this.libraryTag,
    required this.releaseTag,
    required this.kind,
    required this.manifestJson,
    required this.publishedManifestSha256,
    required this.fromLibraryVersion,
    required this.toLibraryVersion,
    required this.files,
    required this.segmentId,
    required this.segmentSha256,
    required this.segmentSize,
    required this.identityDigest,
    required this.modelFamilyId,
    required this.chunkingIdentity,
    required this.family,
  });

  bool get isDelta => kind == 'delta';
  int get downloadSize => files.fold(0, (s, f) => s + f.size);

  /// The compressed segment's name: the single file, or the parts' common name.
  String get segmentFileName => files.length == 1
      ? files.single.name
      : files.first.name.replaceFirst(RegExp(r'\.part-\d+$'), '');
}

enum VectorFailure {
  releaseMalformed,
  unsupportedRelease,
  artifactNotPublished,
  releaseUnverified,
  artifactIncompatible,
  network,
  rateLimited,
  checksum,
  cancelled,
}

class VectorReleaseException implements Exception {
  final VectorFailure kind;
  final String message;
  const VectorReleaseException(this.kind, this.message);
  @override
  String toString() => 'VectorReleaseException(${kind.name}): $message';
}

/// Finds the vector release of a library version (Otzaria's
/// `SemanticVectorsReleaseLocator`, GitHub releases API).
class VectorReleaseLocator {
  static const defaultApiBase = 'https://api.github.com/repos/Otzaria/SeforimLibrary';
  static const _maxPages = 3;
  static final _libraryTag = RegExp(r'^v(\d+)-');
  static final _deltaManifest = RegExp(r'-v(\d+)-v(\d+)\.manifest\.json$');
  static final _sha256 = RegExp(r'\b[0-9a-f]{64}\b');

  final String apiBase;
  final HttpClient Function() _client;
  final String userAgent;
  final Duration timeout;

  VectorReleaseLocator({
    this.apiBase = defaultApiBase,
    HttpClient Function()? clientFactory,
    this.userAgent = 'seforim-search',
    this.timeout = const Duration(seconds: 15),
  }) : _client = clientFactory ?? HttpClient.new;

  /// The release of [libraryVersion] for the first of [candidates] published,
  /// or `null`. [libraryTag] skips the release listing when it is that version's.
  Future<VectorRelease?> findForLibraryVersion(int libraryVersion,
      {String? libraryTag, required List<VectorCandidate> candidates}) async {
    final client = _client();
    try {
      final tag = libraryTag != null && versionOfTag(libraryTag) == libraryVersion
          ? libraryTag
          : await _findLibraryTag(client, libraryVersion);
      if (tag == null) return null;
      Object? failure;
      for (final c in candidates) {
        try {
          final r = await _findCandidate(client, c, tag, libraryVersion);
          if (r != null) return r;
        } on Exception catch (e) {
          failure = e;
        }
      }
      if (failure != null) throw failure;
      return null;
    } finally {
      client.close(force: true);
    }
  }

  /// The deltas from [installedVersion] to [target], oldest first; `null` = take
  /// the base (no chain, too long, too large or not the same identity).
  Future<List<VectorRelease>?> findDeltaChain(
      {required VectorRelease target, required int installedVersion}) async {
    final prefix = target.family.deltaReleaseTagPrefix;
    if (prefix == null ||
        target.identityDigest.isEmpty ||
        installedVersion >= target.toLibraryVersion) {
      return null;
    }
    final maxBytes = target.downloadSize * vectorDeltaChainLimits.maxBaseSizeRatio;
    final client = _client();
    try {
      final chain = <VectorRelease>[];
      final scanned = <dynamic>[];
      var pages = 0;
      Future<String?> tagOf(int version) async {
        var tag = libraryTagFor(scanned, version);
        while (tag == null && pages < _maxPages) {
          final list = await _getJson(client, '$apiBase/releases?per_page=100&page=${++pages}');
          if (list is! List || list.isEmpty) return null;
          scanned.addAll(list);
          tag = libraryTagFor(list, version);
        }
        return tag;
      }

      String? tag = target.libraryTag;
      var to = target.toLibraryVersion;
      var bytes = 0;
      while (to > installedVersion) {
        if (tag == null || chain.length >= vectorDeltaChainLimits.maxSteps) return null;
        final delta = await _findDelta(client, '$prefix$tag', tag, to, target);
        if (delta == null ||
            delta.fromLibraryVersion < installedVersion ||
            delta.fromLibraryVersion >= to) {
          return null;
        }
        bytes += delta.downloadSize;
        if (bytes > maxBytes) return null;
        chain.insert(0, delta);
        to = delta.fromLibraryVersion;
        if (to > installedVersion) tag = await tagOf(to);
      }
      return chain;
    } on VectorReleaseException catch (e) {
      if (const {
        VectorFailure.releaseMalformed,
        VectorFailure.unsupportedRelease,
        VectorFailure.artifactNotPublished,
        VectorFailure.releaseUnverified,
      }.contains(e.kind)) {
        return null;
      }
      rethrow;
    } on FormatException {
      return null;
    } finally {
      client.close(force: true);
    }
  }

  /// The library version of a tag such as `v30-20260930165019`, or `null`.
  static int? versionOfTag(String tag) =>
      int.tryParse(_libraryTag.firstMatch(tag)?.group(1) ?? '');

  /// The library tag of [version] in a releases page, or `null`.
  static String? libraryTagFor(List<dynamic> releases, int version) {
    for (final r in releases) {
      if (r is! Map<String, dynamic>) continue;
      final tag = r['tag_name'];
      if (tag is! String) continue;
      final m = _libraryTag.firstMatch(tag);
      if (m != null && int.parse(m.group(1)!) == version) return tag;
    }
    return null;
  }

  /// The manifest's SHA-256 published in the release notes: the first after its
  /// name, else the only one in the notes; `null` otherwise.
  static String? parsePublishedManifestSha256(String? body, String manifest) {
    if (body == null) return null;
    final text = body.toLowerCase();
    final at = text.indexOf(manifest.toLowerCase());
    if (at >= 0) {
      final after = _sha256.firstMatch(text.substring(at + manifest.length));
      if (after != null) return after.group(0);
    }
    final all = _sha256.allMatches(text).map((m) => m.group(0)).toSet();
    return all.length == 1 ? all.single : null;
  }

  Future<VectorRelease?> _findDelta(HttpClient client, String releaseTag, String tag, int to,
      VectorRelease target) async {
    final release = await _getJson(
        client, '$apiBase/releases/tags/${Uri.encodeComponent(releaseTag)}');
    if (release == null) return null;
    if (release is! Map<String, dynamic>) throw _malformed('release JSON');
    final delta =
        await _parseRelease(client, release, tag, releaseTag, to, target.family, kind: 'delta');
    return delta.identityDigest == target.identityDigest &&
            delta.chunkingIdentity == target.chunkingIdentity
        ? delta
        : null;
  }

  Future<VectorRelease?> _findCandidate(
      HttpClient client, VectorCandidate c, String tag, int libraryVersion) async {
    final releaseTag = '${c.family.releaseTagPrefix}$tag';
    final release = await _getJson(
        client, '$apiBase/releases/tags/${Uri.encodeComponent(releaseTag)}');
    if (release == null) return null;
    if (release is! Map<String, dynamic>) throw _malformed('release JSON is not an object');
    final parsed = await _parseRelease(client, release, tag, releaseTag, libraryVersion, c.family);
    if (parsed.modelFamilyId != c.modelFamilyId) {
      throw VectorReleaseException(VectorFailure.artifactIncompatible,
          '$releaseTag is model ${parsed.modelFamilyId}, expected ${c.modelFamilyId}');
    }
    if (parsed.chunkingIdentity != c.chunkingIdentity) {
      throw VectorReleaseException(VectorFailure.artifactIncompatible,
          '$releaseTag is chunking ${parsed.chunkingIdentity}, expected ${c.chunkingIdentity}');
    }
    return parsed;
  }

  Future<String?> _findLibraryTag(HttpClient client, int version) async {
    for (var page = 1; page <= _maxPages; page++) {
      final list = await _getJson(client, '$apiBase/releases?per_page=100&page=$page');
      if (list is! List || list.isEmpty) return null;
      final tag = libraryTagFor(list, version);
      if (tag != null) return tag;
    }
    return null;
  }

  Future<VectorRelease> _parseRelease(HttpClient client, Map<String, dynamic> release,
      String libraryTag, String releaseTag, int libraryVersion, VectorFamily family,
      {String kind = 'base'}) async {
    final list = release['assets'];
    if (list is! List) throw _malformed('release assets');
    final assets = <String, Map<String, dynamic>>{
      for (final a in list)
        if (a is Map<String, dynamic> && a['name'] is String) a['name'] as String: a,
    };
    final manifests = assets.values.where((a) {
      final n = a['name'] as String;
      return n.startsWith('otzaria-vectors-') && n.endsWith('.manifest.json');
    });
    final manifestAsset = kind == 'delta'
        ? manifests.where((a) => _deltaManifest.hasMatch(a['name'] as String)).firstOrNull
        : manifests
                .where((a) => (a['name'] as String).endsWith('-base.manifest.json'))
                .firstOrNull ??
            manifests.firstOrNull;
    if (manifestAsset == null) throw _malformed('manifest asset missing');
    final manifestName = manifestAsset['name'] as String;
    final manifestBytes =
        await _getBytes(client, _string(manifestAsset['browser_download_url'], 'manifest URL'));
    final manifestSha = sha256.convert(manifestBytes).toString();
    final assetDigest = switch (manifestAsset['digest']) {
      final String d when d.startsWith('sha256:') => d.substring(7).toLowerCase(),
      _ => null,
    };
    final body = release['body'];
    if (body != null && body is! String) throw _malformed('release body');
    final notesDigest = parsePublishedManifestSha256(body as String?, manifestName);
    final published = notesDigest ?? assetDigest;
    if (published == null) {
      throw VectorReleaseException(
          VectorFailure.releaseUnverified, 'no published SHA-256 for $manifestName');
    }
    for (final expected in [assetDigest, notesDigest]) {
      if (expected != null && expected != manifestSha) {
        throw VectorReleaseException(VectorFailure.artifactNotPublished,
            'manifest $manifestName is $manifestSha, published $expected');
      }
    }
    final manifestJson = utf8.decode(manifestBytes);
    final Object? manifest;
    try {
      manifest = jsonDecode(manifestJson);
    } on FormatException catch (e) {
      throw _malformed('manifest is not JSON: ${e.message}');
    }
    if (manifest is! Map<String, dynamic>) throw _malformed('manifest shape');
    final files = manifest['files'];
    if (files is! List || files.isEmpty) throw _malformed('manifest files');
    if (manifest['kind'] != kind) {
      throw VectorReleaseException(VectorFailure.unsupportedRelease,
          '$releaseTag holds a ${manifest['kind']} manifest, expected $kind');
    }
    final toVersion = _int(manifest['toLibraryVersion'], 'toLibraryVersion');
    final fromVersion = switch (manifest['fromLibraryVersion']) {
      final int v => v,
      _ when kind == 'base' => 0,
      _ => throw _malformed('fromLibraryVersion missing'),
    };
    if (kind == 'delta') {
      final named = _deltaManifest.firstMatch(manifestName)!;
      if (fromVersion >= toVersion ||
          int.parse(named.group(1)!) != fromVersion ||
          int.parse(named.group(2)!) != toVersion) {
        throw _malformed('$manifestName holds v$fromVersion-v$toVersion');
      }
    }
    if (toVersion != libraryVersion || manifest['libraryReleaseTag'] != libraryTag) {
      throw _malformed('manifest is for ${manifest['libraryReleaseTag']} (v$toVersion), '
          'requested $libraryTag (v$libraryVersion)');
    }
    final segment = manifest['segment'];
    if (segment is! Map<String, dynamic>) throw _malformed('manifest segment');
    final segmentId = manifest['segmentId'];
    if (segmentId is! String || !_isSegmentId(segmentId)) throw _malformed('segmentId');
    final identity = manifest['identity'];
    final model = identity is Map ? identity['model'] : null;
    return VectorRelease(
      libraryTag: libraryTag,
      releaseTag: releaseTag,
      kind: kind,
      manifestJson: manifestJson,
      publishedManifestSha256: published,
      fromLibraryVersion: fromVersion,
      toLibraryVersion: toVersion,
      files: [for (final e in files) _file(e, assets)],
      segmentId: segmentId,
      segmentSha256: _string(segment['sha256'], 'segment.sha256').toLowerCase(),
      segmentSize: _int(segment['size'], 'segment.size'),
      identityDigest: manifest['identityDigest'] is String ? manifest['identityDigest'] : '',
      modelFamilyId: model is Map && model['family_id'] is String ? model['family_id'] : '',
      chunkingIdentity:
          model is Map ? _chunking(model['chunking_identity'], manifestJson) : null,
      family: family,
    );
  }

  static VectorFile _file(Object? e, Map<String, Map<String, dynamic>> assets) {
    if (e is! Map<String, dynamic>) throw _malformed('file entry');
    final name = e['file'], sha = e['sha256'];
    if (name is! String || sha is! String || !_isPlainName(name)) {
      throw _malformed('file entry');
    }
    final asset = assets[name];
    if (asset == null) throw _malformed('asset $name missing from release');
    return VectorFile(name, _string(asset['browser_download_url'], '$name URL'),
        _int(e['size'], '$name.size'), sha.toLowerCase(), asset['id']?.toString() ?? '');
  }

  Future<Object?> _getJson(HttpClient client, String url) async {
    final (status, body) = await _get(client, url, json: true);
    if (status == 404) return null;
    if (status < 200 || status >= 300) {
      throw VectorReleaseException(
          status == 403 || status == 429 ? VectorFailure.rateLimited : VectorFailure.network,
          'GitHub API returned $status for $url');
    }
    return jsonDecode(utf8.decode(body));
  }

  Future<List<int>> _getBytes(HttpClient client, String url) async {
    final (status, body) = await _get(client, url);
    if (status < 200 || status >= 300) {
      throw VectorReleaseException(VectorFailure.network, 'manifest download returned $status');
    }
    return body;
  }

  Future<(int, List<int>)> _get(HttpClient client, String url, {bool json = false}) async {
    try {
      final rq = await client.getUrl(Uri.parse(url)).timeout(timeout);
      rq.headers.set(HttpHeaders.userAgentHeader, userAgent);
      if (json) rq.headers.set(HttpHeaders.acceptHeader, 'application/vnd.github+json');
      final rs = await rq.close().timeout(timeout);
      final bytes = await rs.fold<BytesBuilder>(BytesBuilder(copy: false), (b, c) => b..add(c));
      return (rs.statusCode, bytes.takeBytes());
    } on IOException catch (e) {
      throw VectorReleaseException(VectorFailure.network, '$url: $e');
    } on TimeoutException {
      throw VectorReleaseException(VectorFailure.network, '$url: timed out');
    }
  }
}

/// Progress of a download / install: [stage], bytes done and total (if known).
typedef VectorProgress = void Function(String stage, int done, int? total);

/// Resumable, verified file download (Otzaria's `SemanticDownloadClient` +
/// staged download): bytes go to `<dest>.partial` (resumed with a Range request
/// when the same asset was half downloaded), are checked for size and SHA-256,
/// then renamed to `<dest>`. A complete `<dest>` with the right hash is adopted.
class VectorDownloader {
  final HttpClient Function() _client;
  final String userAgent;

  VectorDownloader({HttpClient Function()? clientFactory, this.userAgent = 'seforim-search'})
      : _client = clientFactory ?? HttpClient.new;

  Future<void> download(VectorFile file, String dest,
      {VectorProgress? onProgress, bool Function()? isCancelled}) async {
    final done = File(dest);
    if (await done.exists() &&
        await done.length() == file.size &&
        await sha256OfFile(dest, isCancelled: isCancelled) == file.sha256) {
      onProgress?.call('download', file.size, file.size);
      return;
    }
    final partial = File('$dest.partial');
    final idFile = File('$dest.partial.id');
    final identity = '${file.assetId}:${file.sha256}:${file.size}';
    if (await partial.exists() &&
        (!await idFile.exists() || await idFile.readAsString() != identity)) {
      await partial.delete();
    }
    await partial.parent.create(recursive: true);
    await idFile.writeAsString(identity, flush: true);
    var have = await partial.exists() ? await partial.length() : 0;
    if (have > file.size) {
      await partial.delete();
      have = 0;
    }
    final client = _client();
    try {
      if (have < file.size) {
        final rq = await client.getUrl(Uri.parse(file.downloadUrl));
        rq.headers.set(HttpHeaders.userAgentHeader, userAgent);
        if (have > 0) rq.headers.set(HttpHeaders.rangeHeader, 'bytes=$have-');
        final rs = await rq.close();
        var status = rs.statusCode;
        final cr = rs.headers.value(HttpHeaders.contentRangeHeader);
        if (status == 416 || (status == 206 && cr != null && !cr.startsWith('bytes $have-'))) {
          // The partial is not resumable here: restart it from 0 next time.
          await rs.drain<void>();
          await partial.delete();
          throw VectorReleaseException(VectorFailure.network,
              '${file.name}: range not served (status $status), the partial was dropped');
        }
        if (status == 408 || status == 429 || status == 403 || status >= 500 ||
            (status != 200 && status != 206)) {
          await rs.drain<void>();
          throw VectorReleaseException(
              status == 429 || status == 403 ? VectorFailure.rateLimited : VectorFailure.network,
              'vector download returned $status');
        }
        if (status == 200) have = 0;
        final sink = partial.openWrite(mode: have == 0 ? FileMode.write : FileMode.append);
        try {
          await for (final chunk in rs) {
            if (isCancelled?.call() ?? false) {
              throw const VectorReleaseException(VectorFailure.cancelled, 'cancelled');
            }
            if (have + chunk.length > file.size) {
              sink.add(chunk.sublist(0, file.size - have));
              have = file.size + 1;
              break;
            }
            sink.add(chunk);
            have += chunk.length;
            onProgress?.call('download', have, file.size);
          }
          await sink.flush();
        } finally {
          await sink.close();
        }
      }
    } on IOException catch (e) {
      throw VectorReleaseException(VectorFailure.network, '${file.name}: $e');
    } finally {
      client.close(force: true);
    }
    final size = await partial.length();
    final sha = await sha256OfFile(partial.path, isCancelled: isCancelled);
    if (size != file.size || sha != file.sha256) {
      await partial.delete();
      await idFile.delete();
      throw VectorReleaseException(VectorFailure.checksum,
          '${file.name} is $size bytes sha256 $sha, expected ${file.size} ${file.sha256}');
    }
    if (await done.exists()) await done.delete();
    await partial.rename(dest);
    await idFile.delete();
  }
}

/// Checks a staged segment before it is installed (the sidecar's `check_header`
/// and `Segment::verify_blocks`): throw to refuse it. A native call (see README).
typedef VectorSegmentVerifier = Future<void> Function(String segmentPath, Map<String, dynamic> manifest);

/// Applies a verified delta segment onto the current generation: writes the new
/// generation's `.del` / `.links` files and returns its set.json segment entries
/// and stats. Needs the segments' keys, tombstones and foreign records: a native call.
typedef VectorDeltaApplier = Future<VectorDeltaResult> Function(
    String setDir, String segmentPath, Map<String, dynamic> manifest, String generationDir);

class VectorDeltaResult {
  final List<Map<String, dynamic>> segments;
  final Map<String, dynamic> stats;
  const VectorDeltaResult(this.segments, this.stats);
}

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
  const VectorInstallReport(this.kind, this.libraryVersion, this.generation, this.segments,
      this.slotsAdded, this.bytesOnDisk, this.alreadyApplied);
}

/// A set directory as the sidecar lays it out; writes generations
/// (port of `install_package` / `NewGeneration::commit` for a base segment).
class VectorSetWriter {
  static const releaseFormat = 'otzaria-vectors-release';
  static const setFormat = 'otzaria-vector-set';

  final String dir;
  final VectorSegmentVerifier? verifySegment;
  final VectorDeltaApplier? applyDelta;

  VectorSetWriter(this.dir, {this.verifySegment, this.applyDelta});

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
  Future<VectorInstallReport> install(String segmentPath, String manifestJson,
      {String? publishedManifestSha256, bool Function()? isCancelled}) async {
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
      if (applyDelta == null) {
        throw const VectorSetException(
            'applying a delta needs the native delta applier (not in the C ABI yet)');
      }
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
    await verifySegment?.call(staged, m);

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
    if (kind == 'delta') {
      final r = await applyDelta!(dir, target.path, m, genDir.path);
      segments = r.segments;
      stats = r.stats;
    } else {
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

/// Finds, downloads, verifies, expands and installs the vector set of a library
/// version into a set directory: a base, or a delta chain when a set of the same
/// identity is installed and a short enough chain exists.
class VectorSetUpdater {
  final VectorReleaseLocator locator;
  final VectorDownloader downloader;
  final VectorSetWriter writer;

  /// Expands a `.oxv.zst` to a plain `.oxv` (default: the engine's `expandOxv`).
  final int Function(String source, String destination) expand;

  VectorSetUpdater(this.writer,
      {VectorReleaseLocator? locator,
      VectorDownloader? downloader,
      int Function(String, String)? expand})
      : locator = locator ?? VectorReleaseLocator(),
        downloader = downloader ?? VectorDownloader(),
        expand = expand ?? expandOxv;

  /// Brings the set to [libraryVersion]; `null` when no release of it is
  /// published for [candidates].
  Future<VectorInstallReport?> update(int libraryVersion,
      {required List<VectorCandidate> candidates,
      String? libraryTag,
      VectorProgress? onProgress,
      bool Function()? isCancelled}) async {
    final target = await locator.findForLibraryVersion(libraryVersion,
        libraryTag: libraryTag, candidates: candidates);
    if (target == null) return null;
    final current = writer.currentSet();
    final installed = current?['library_version'] as int?;
    if (installed != null &&
        installed >= target.toLibraryVersion &&
        current!['identity_digest'] == target.identityDigest) {
      return VectorInstallReport('base', installed, current['generation'] as int,
          (current['segments'] as List).length, 0, current['stats']['bytes'] as int, true);
    }
    List<VectorRelease>? chain;
    if (installed != null &&
        writer.applyDelta != null &&
        current!['identity_digest'] == target.identityDigest) {
      chain = await locator.findDeltaChain(target: target, installedVersion: installed);
    }
    VectorInstallReport? last;
    for (final r in (chain == null || chain.isEmpty) ? [target] : chain) {
      last = await installRelease(r, onProgress: onProgress, isCancelled: isCancelled);
    }
    return last;
  }

  /// Downloads (resumable) every file of [release] to incoming/, joins parts,
  /// expands, checks the segment's size and SHA-256, and installs it.
  Future<VectorInstallReport> installRelease(VectorRelease release,
      {VectorProgress? onProgress, bool Function()? isCancelled}) async {
    final incoming = writer.incoming;
    await Directory(incoming).create(recursive: true);
    final local = <String>[];
    for (final f in release.files) {
      final dest = '$incoming/${f.name}';
      await downloader.download(f, dest, onProgress: onProgress, isCancelled: isCancelled);
      local.add(dest);
    }
    var compressed = local.first;
    if (local.length > 1) {
      compressed = '$incoming/${release.segmentFileName}';
      final out = File(compressed).openWrite();
      for (final part in local) {
        await out.addStream(File(part).openRead());
      }
      await out.close();
    }
    if (isCancelled?.call() ?? false) {
      throw const VectorReleaseException(VectorFailure.cancelled, 'cancelled');
    }
    onProgress?.call('expand', 0, release.segmentSize);
    final segment = '$incoming/${release.segmentId}.oxv';
    final written = expand(compressed, segment);
    if (written != release.segmentSize) {
      await File(segment).delete();
      throw VectorReleaseException(VectorFailure.checksum,
          'expanded segment is $written bytes, expected ${release.segmentSize}');
    }
    onProgress?.call('install', 0, null);
    final report = await writer.install(segment, release.manifestJson,
        publishedManifestSha256: release.publishedManifestSha256, isCancelled: isCancelled);
    for (final p in {...local, compressed}) {
      final f = File(p);
      if (f.existsSync()) f.deleteSync();
    }
    return report;
  }
}

/// SHA-256 of a file, streamed.
Future<String> sha256OfFile(String path, {bool Function()? isCancelled}) async {
  final out = _DigestSink();
  final input = sha256.startChunkedConversion(out);
  await for (final chunk in File(path).openRead()) {
    if (isCancelled?.call() ?? false) {
      throw const VectorReleaseException(VectorFailure.cancelled, 'cancelled');
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

int? _chunking(Object? v, String raw) {
  if (v is int && v >= 0) return v;
  if (v is double && v.isFinite) {
    final ms = RegExp(r'"chunking_identity"\s*:\s*(0|[1-9][0-9]*)(?=\s*[,}])').allMatches(raw).toList();
    if (ms.length == 1) return int.tryParse(ms.single.group(1)!);
  }
  return null;
}

bool _isSegmentId(String id) => RegExp(r'^[0-9a-f]{32}$').hasMatch(id);
bool _isPlainName(String n) =>
    n.isNotEmpty && !n.contains('/') && !n.contains('\\') && n != '.' && n != '..';

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

String _string(Object? v, String field) =>
    v is String ? v : throw _malformed('$field missing');
int _int(Object? v, String field) => v is int ? v : throw _malformed('$field missing');
VectorReleaseException _malformed(String d) =>
    VectorReleaseException(VectorFailure.releaseMalformed, d);
