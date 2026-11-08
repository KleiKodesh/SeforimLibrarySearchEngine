import 'dart:ffi';
import 'dart:io';

import 'package:code_assets/code_assets.dart';
import 'package:hooks/hooks.dart';
import 'package:seforim_search/seforim_search.dart';
import 'package:seforim_search/src/hook/native_source.dart';
import 'package:test/test.dart';

import '../hook/build.dart' as hook;

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
  setUp(() => tmp = Directory.systemTemp.createTempSync('se_pack_'));
  tearDown(() => tmp.deleteSync(recursive: true));

  group('ABI check', () {
    test('the loaded library has this package\'s ABI', () {
      final path = _nativePath();
      if (path == null) return markTestSkipped('native library not built');
      final lib = DynamicLibrary.open(path);
      expect(SeforimNative.abiVersionOf(lib), seforimAbiVersion);
      SeforimNative.checkAbi(lib, path); // no throw
    });

    test('a mismatch names both versions and the loaded path', () {
      final path = _nativePath();
      if (path == null) return markTestSkipped('native library not built');
      final lib = DynamicLibrary.open(path);
      expect(
        () => SeforimNative.checkAbi(lib, 'requested-name', expected: seforimAbiVersion + 1),
        throwsA(isA<NativeAbiMismatchException>()
            .having((e) => e.expected, 'expected', seforimAbiVersion + 1)
            .having((e) => e.loaded, 'loaded', seforimAbiVersion)
            .having((e) => e.message, 'message', contains('ABI ${seforimAbiVersion + 1}'))
            .having((e) => e.message, 'message', contains('has ABI $seforimAbiVersion'))
            .having((e) => e.path.toLowerCase(), 'path', contains('seforim_search_native'))),
      );
    });
  });

  group('build hook', () {
    test('release file names per target', () {
      expect(releaseFileName('windows-arm64'), 'seforim_search_native-windows-arm64.dll');
      expect(releaseFileName('macos-x64'), 'seforim_search_native-macos-x64.dylib');
      expect(releaseFileName('ios_sim-arm64'), 'seforim_search_native-ios_sim-arm64.dylib');
      expect(releaseFileName('android-arm64'), 'seforim_search_native-android-arm64.so');
      expect(rustTriple('linux-arm64'), 'aarch64-unknown-linux-gnu');
    });

    test('verifySha256 accepts the right hash and refuses another', () async {
      final f = File('${tmp.path}/fake.bin')..writeAsBytesSync([1, 2, 3, 4]);
      final hash = await sha256Of(f);
      await verifySha256(f, hash.toUpperCase());
      await expectLater(verifySha256(f, '0' * 64), throwsA(isA<StateError>()));
    });

    Future<void> runHook(Map<String, Object?> defines, void Function(BuildOutput) check) => testCodeBuildHook(
          mainMethod: hook.main,
          userDefines: PackageUserDefines(
              workspacePubspec: PackageUserDefinesSource(defines: defines, basePath: tmp.uri.resolve('pubspec.yaml'))),
          check: (_, output) => check(output),
        );

    test('source: file bundles the local library after its hash check', () async {
      final f = File('${tmp.path}/${OS.current.dylibFileName('fake')}')..writeAsBytesSync([9, 8, 7]);
      final hash = await sha256Of(f);
      await runHook({'source': 'file', 'path': f.uri.pathSegments.last, 'sha256': hash}, (output) {
        final assets = output.assets.code;
        expect(assets, hasLength(1));
        expect(assets.single.id, 'package:seforim_search/seforim_search_native');
        expect(File.fromUri(assets.single.file!).readAsBytesSync(), [9, 8, 7]);
      });
    });

    test('source: file refuses a hash mismatch', () async {
      final f = File('${tmp.path}/${OS.current.dylibFileName('fake')}')..writeAsBytesSync([9, 8, 7]);
      await expectLater(
          runHook({'source': 'file', 'path': f.path, 'sha256': 'ab' * 32}, (_) => fail('must not bundle')),
          throwsA(anything));
    });
  });
}
