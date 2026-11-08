import 'dart:io';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

/// Loads the library with no path: through the native asset the build hook
/// bundled (this package's pubspec selects source: cargo), ABI checked.
void main() {
  test('default load resolves the bundled native asset', () {
    if (Platform.environment['SEFORIM_SEARCH_NATIVE'] != null) {
      return markTestSkipped('SEFORIM_SEARCH_NATIVE overrides the asset');
    }
    final native = SeforimNative.load();
    expect(native.nativeVersion, isNotEmpty);
  });
}
