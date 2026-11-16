/// Where the build hook (`hook/build.dart`) gets the native library from.
///
/// Modelled on `package:sqlite3` v3: the app picks the source with hook
/// user-defines in its root `pubspec.yaml`:
///
/// ```yaml
/// hooks:
///   user_defines:
///     seforim_search:
///       source: prebuilt   # default; or: cargo, file
///       path: native/seforim_search_native.dll   # source: file (string, or a map per OS)
///       sha256: <hex>      # optional for source: file
///       url_pattern: https://mirror/{tag}/{file}  # optional for prebuilt
/// ```
library;

import 'dart:io';

import 'package:code_assets/code_assets.dart';
import 'package:crypto/crypto.dart';
import 'package:hooks/hooks.dart';

import 'asset_hashes.g.dart';

/// The Dart native asset name (`package:seforim_search/seforim_search_native`).
const String assetName = 'seforim_search_native';

/// The GitHub repository whose releases hold the prebuilt libraries.
const String releaseRepository = 'KleiKodesh/SeforimLibrarySearchEngine';

/// Default download URL; `{tag}` and `{file}` are replaced.
const String defaultUrlPattern = 'https://github.com/$releaseRepository/releases/download/{tag}/{file}';

/// The target name used in release asset names (`windows-x64`, `ios_sim-arm64`, ...).
String targetName(CodeConfig code) {
  final os = switch (code.targetOS) {
    OS.iOS when code.iOS.targetSdk == IOSSdk.iPhoneSimulator => 'ios_sim',
    final OS os => os.name,
  };
  return '$os-${code.targetArchitecture.name}';
}

/// The release asset name for a target: `seforim_search_native-<target>.<ext>`.
String releaseFileName(String target) {
  final ext = target.startsWith('windows-')
      ? 'dll'
      : (target.startsWith('macos-') || target.startsWith('ios')) ? 'dylib' : 'so';
  return '$assetName-$target.$ext';
}

/// The Rust target triple for a target name (cargo source), or null.
String? rustTriple(String target) => const {
      'windows-x64': 'x86_64-pc-windows-msvc',
      'windows-arm64': 'aarch64-pc-windows-msvc',
      'linux-x64': 'x86_64-unknown-linux-gnu',
      'linux-arm64': 'aarch64-unknown-linux-gnu',
      'macos-x64': 'x86_64-apple-darwin',
      'macos-arm64': 'aarch64-apple-darwin',
      'android-arm64': 'aarch64-linux-android',
      'android-x64': 'x86_64-linux-android',
      'android-arm': 'armv7-linux-androideabi',
      'ios-arm64': 'aarch64-apple-ios',
      'ios_sim-arm64': 'aarch64-apple-ios-sim',
      'ios_sim-x64': 'x86_64-apple-ios',
    }[target];

/// The source the app chose (or the default).
sealed class NativeSource {
  const NativeSource();

  /// Reads the `seforim_search` user-defines. [packageVersion] is this
  /// package's pubspec version; [hasCargo] / [hasSources] decide the fallback
  /// when no prebuilt library is pinned for the target.
  static NativeSource resolve({
    required HookInputUserDefines userDefines,
    required String target,
    required String osName,
    required String packageVersion,
    required bool hasCargo,
    required bool hasSources,
  }) {
    String? perOs(String key) {
      final value = userDefines[key];
      final v = value is Map ? (value[osName] ?? value['default']) : value;
      if (v == null || v is String) return v as String?;
      throw ArgumentError.value(value, key, 'expected a string or a map of strings per OS');
    }

    final source = perOs('source') ?? 'prebuilt';
    switch (source) {
      case 'file':
        final path = perOs('path');
        final base = userDefines.baseUri(['path']);
        if (path == null || base == null) {
          throw ArgumentError('seforim_search: source: file needs a path user-define');
        }
        final file = File(Uri.file(path).isAbsolute ? path : base.resolve(path).toFilePath());
        return LocalFileSource(file, sha256: perOs('sha256'));
      case 'cargo':
        return const CargoSource();
      case 'prebuilt':
        final tag = 'v$packageVersion';
        final file = releaseFileName(target);
        final hash = releaseTag == tag ? assetSha256[file] : null;
        if (hash == null) {
          if (hasCargo && hasSources) return const CargoSource();
          throw UnsupportedError(
              'seforim_search: no prebuilt native library pinned for $target in release $tag '
              '(pinned release: ${releaseTag ?? 'none'}). Build it with cargo (source: cargo, '
              'from a checkout of $releaseRepository) or point source: file at a library.');
        }
        final pattern = userDefines['url_pattern'] as String? ?? defaultUrlPattern;
        return PrebuiltSource(
            Uri.parse(pattern.replaceAll('{tag}', tag).replaceAll('{file}', file)), file, hash);
      default:
        throw ArgumentError.value(source, 'source', 'seforim_search: expected prebuilt, cargo or file');
    }
  }
}

/// A prebuilt library downloaded from the GitHub release, SHA-256 pinned.
final class PrebuiltSource extends NativeSource {
  const PrebuiltSource(this.url, this.fileName, this.sha256);
  final Uri url;
  final String fileName;
  final String sha256;
}

/// Built from the repository's Rust workspace with cargo.
final class CargoSource extends NativeSource {
  const CargoSource();
}

/// A library file on disk (offline), optionally SHA-256 checked.
final class LocalFileSource extends NativeSource {
  const LocalFileSource(this.file, {this.sha256});
  final File file;
  final String? sha256;
}

/// The SHA-256 of [file] (lower-case hex).
Future<String> sha256Of(File file) async => (await sha256.bind(file.openRead()).first).toString();

/// Throws [StateError] unless [file]'s SHA-256 is [expected] (hex, any case).
Future<void> verifySha256(File file, String expected) async {
  final actual = await sha256Of(file);
  if (actual != expected.toLowerCase()) {
    throw StateError('seforim_search: SHA-256 of ${file.path} is $actual, expected $expected; refusing to use it.');
  }
}
