// Native Assets build hook: provides the seforim_search_native library for
// the target OS / architecture. Sources (user-define `source`, see
// lib/src/hook/native_source.dart and README): prebuilt (default: download
// from the GitHub release of this package version, SHA-256 pinned), cargo
// (build from the repository's Rust workspace), file (a local library).
import 'dart:ffi' show Abi;
import 'dart:io';

import 'package:code_assets/code_assets.dart';
import 'package:hooks/hooks.dart';
import 'package:seforim_search/src/hook/native_source.dart';

void main(List<String> args) async {
  await build(args, (input, output) async {
    if (!input.config.buildCodeAssets) return;
    final code = input.config.code;
    final target = targetName(code);
    final workspace = Directory.fromUri(input.packageRoot.resolve('../../'));
    final hasSources = File.fromUri(workspace.uri.resolve('engine-ffi/Cargo.toml')).existsSync();
    final cargo = _findCargo();

    final source = NativeSource.resolve(
      userDefines: input.userDefines,
      target: target,
      osName: code.targetOS.name,
      packageVersion: _packageVersion(input.packageRoot),
      hasCargo: cargo != null,
      hasSources: hasSources,
    );
    output.dependencies.add(input.packageRoot.resolve('pubspec.yaml'));

    final libName = code.targetOS.dylibFileName('seforim_search_native');
    final File library;
    switch (source) {
      case LocalFileSource(:final file, :final sha256):
        if (!file.existsSync()) throw StateError('seforim_search: source: file: ${file.path} does not exist');
        if (sha256 != null) await verifySha256(file, sha256);
        output.dependencies.add(file.uri);
        library = file;
      case PrebuiltSource():
        library = await _download(input, source, libName);
      case CargoSource():
        if (cargo == null) throw StateError('seforim_search: source: cargo, but cargo was not found (PATH or ~/.cargo/bin)');
        if (!hasSources) {
          throw StateError('seforim_search: source: cargo needs the Rust workspace next to the package '
              '(a checkout of $releaseRepository); none at ${workspace.path}');
        }
        library = await _cargoBuild(input, output, cargo, workspace, target, libName);
    }

    output.assets.code.add(CodeAsset(
      package: input.packageName,
      name: assetName,
      linkMode: DynamicLoadingBundled(),
      file: library.uri,
    ));
  });
}

String _packageVersion(Uri packageRoot) {
  final pubspec = File.fromUri(packageRoot.resolve('pubspec.yaml')).readAsStringSync();
  final m = RegExp(r'^version:\s*([^\s#]+)', multiLine: true).firstMatch(pubspec);
  if (m == null) throw StateError('seforim_search: no version in pubspec.yaml');
  return m.group(1)!;
}

String? _findCargo() {
  final exe = Platform.isWindows ? 'cargo.exe' : 'cargo';
  final dirs = [
    ...?Platform.environment['PATH']?.split(Platform.isWindows ? ';' : ':'),
    if (Platform.environment['CARGO_HOME'] case final h?) '$h/bin',
    if ((Platform.environment['USERPROFILE'] ?? Platform.environment['HOME']) case final h?) '$h/.cargo/bin',
  ];
  for (final d in dirs) {
    if (d.isEmpty) continue;
    final f = File('$d${Platform.pathSeparator}$exe');
    if (f.existsSync()) return f.path;
  }
  return null;
}

Future<File> _download(BuildInput input, PrebuiltSource source, String libName) async {
  final dir = Directory.fromUri(input.outputDirectoryShared.resolve('download-${source.sha256.substring(0, 16)}/'));
  final file = File('${dir.path}$libName');
  if (file.existsSync()) {
    try {
      await verifySha256(file, source.sha256);
      return file;
    } on StateError {
      file.deleteSync();
    }
  }
  dir.createSync(recursive: true);
  final partial = File('${file.path}.$pid.${DateTime.now().microsecondsSinceEpoch}.part');
  final client = HttpClient()..connectionTimeout = const Duration(seconds: 30);
  try {
    final response = await (await client.getUrl(source.url)).close();
    if (response.statusCode != 200) {
      throw HttpException('seforim_search: download of ${source.url} failed: HTTP ${response.statusCode}');
    }
    await response.pipe(partial.openWrite()).timeout(const Duration(minutes: 10));
  } finally {
    client.close();
  }
  try {
    await verifySha256(partial, source.sha256);
  } catch (_) {
    partial.deleteSync();
    rethrow;
  }
  try {
    return partial.renameSync(file.path);
  } on FileSystemException {
    // A concurrent build (another architecture) placed it first.
    partial.deleteSync();
    await verifySha256(file, source.sha256);
    return file;
  }
}

Future<File> _cargoBuild(BuildInput input, BuildOutputBuilder output, String cargo, Directory workspace,
    String target, String libName) async {
  final triple = rustTriple(target);
  if (triple == null) throw UnsupportedError('seforim_search: no Rust target for $target');
  final host = _hostTarget();
  final crossArgs = triple == host ? <String>[] : ['--target', triple];
  final result = await Process.run(cargo, ['build', '--release', '--locked', '-p', 'engine-ffi', ...crossArgs],
      workingDirectory: workspace.path);
  if (result.exitCode != 0) {
    throw StateError('seforim_search: cargo build failed (exit ${result.exitCode}):\n${result.stderr}');
  }
  final targetDir = Platform.environment['CARGO_TARGET_DIR'];
  final targetUri = targetDir == null
      ? workspace.uri.resolve('target/')
      : Directory(targetDir).absolute.uri;
  final built = File.fromUri(targetUri.resolve(triple == host ? 'release/$libName' : '$triple/release/$libName'));
  if (!built.existsSync()) throw StateError('seforim_search: cargo built no ${built.path}');
  final out = File.fromUri(input.outputDirectory.resolve(libName));
  built.copySync(out.path);
  output.dependencies.addAll([
    workspace.uri.resolve('Cargo.lock'),
    workspace.uri.resolve('Cargo.toml'),
    for (final crate in ['engine-ffi', 'seforim-engine', 'hebrew-fts', 'tantivy']) workspace.uri.resolve('$crate/Cargo.toml'),
    for (final crate in ['engine-ffi', 'seforim-engine', 'hebrew-fts', 'tantivy']) workspace.uri.resolve('$crate/src/'),
  ]);
  return out;
}

String? _hostTarget() {
  final os = Platform.isWindows
      ? 'windows'
      : Platform.isMacOS
          ? 'macos'
          : Platform.isLinux
              ? 'linux'
              : null;
  if (os == null) return null;
  final arch = switch (Abi.current()) {
    Abi.windowsArm64 || Abi.macosArm64 || Abi.linuxArm64 => 'arm64',
    _ => 'x64',
  };
  return rustTriple('$os-$arch');
}
