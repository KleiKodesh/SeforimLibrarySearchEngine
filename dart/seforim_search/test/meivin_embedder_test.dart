import 'dart:convert';
import 'dart:io';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:seforim_search/seforim_search.dart';
import 'package:test/test.dart';

final _env = Platform.environment;
final _modelDir = '${_env['APPDATA']}/otzaria/books/meivin-qa491-onnx';
final _ort = _env['OTZARIA_ONNX_RUNTIME'] ??
    '${_env['USERPROFILE']}/.nuget/packages/microsoft.ml.onnxruntime/1.28.0/runtimes/win-x64/native/onnxruntime.dll';
final _golden = _env['OTZARIA_GOLDEN'] ??
    '${_env['USERPROFILE']}/.cargo/git/checkouts/otzaria-semantic-search-934c7c4f97476962/'
        'd2a782a/tests/data/onnx_golden_vectors.json';

List<Map<String, dynamic>> _cases() =>
    ((jsonDecode(File(_golden).readAsStringSync()) as Map<String, dynamic>)['cases'] as List)
        .cast<Map<String, dynamic>>();

Float32List _vec(Map<String, dynamic> c) =>
    Float32List.fromList(base64Decode(c['vector_f32_le_base64'] as String).buffer.asFloat32List());

double _dot(Float32List a, Float32List b) {
  var s = 0.0;
  for (var i = 0; i < a.length; i++) {
    s += a[i] * b[i];
  }
  return s;
}

final _pardesCases = _env['OTZARIA_PARDES_CASES'] ??
    '${_env['USERPROFILE']}/.cargo/git/checkouts/otzaria-semantic-search-934c7c4f97476962/'
        'd2a782a/tests/data/pardes_clean_cases.json';

void main() {
  test('normalization 2 = Otzaria pardes_clean on its reference cases', () {
    final cases = (jsonDecode(File(_pardesCases).readAsStringSync()) as List).cast<List>();
    expect(cases, isNotEmpty);
    for (final c in cases) {
      expect(meivinPardesClean(c[0] as String), c[1] as String, reason: jsonEncode(c[0]));
    }
    expect(MeivinTextNormalization.fromVersion(1).apply(' <b>x</b> '), ' <b>x</b> ');
    expect(MeivinTextNormalization.fromVersion(2).apply(' <b>x</b>&amp;y '), 'x &y');
    expect(() => MeivinTextNormalization.fromVersion(3), throwsUnsupportedError);
  }, skip: File(_pardesCases).existsSync() ? false : 'set OTZARIA_PARDES_CASES to pardes_clean_cases.json');

  test('L2 normalization is Otzaria\'s f32 arithmetic', () {
    final v = meivinL2Normalize(Float32List.fromList([3, 4]));
    expect(v, [0.6000000238418579, 0.800000011920929]);
    // f32 sum of squares, f32 sqrt, multiply by the f32 reciprocal
    final x = Float32List.fromList([0.1, 0.2, 0.3, 1e-3, 7]);
    final f = Float32List(1);
    for (final e in x) {
      f[0] = f[0] + Float32List.fromList([e * e])[0];
    }
    final inv = Float32List.fromList([1 / Float32List.fromList([math.sqrt(f[0])])[0]])[0];
    final want = [for (final e in x) Float32List.fromList([e * inv])[0]];
    expect(meivinL2Normalize(Float32List.fromList(x)), want);
    expect(() => meivinL2Normalize(Float32List(4)), throwsStateError);
  });

  test('text recipes by version', () {
    expect(MeivinTextRecipe.fromVersion(2).queryText('  a b \n'), '[QUERY] a b');
    expect(MeivinTextRecipe.fromVersion(2).passageText(' a '), '[PASSAGE] a');
    expect(MeivinTextRecipe.fromVersion(1).queryText(' a '), ' a ');
    expect(MeivinTextRecipe.fromVersion(3).queryText(' a'), 'query:  a');
    expect(MeivinTextRecipe.fromVersion(3).passageText(' a'), ' a');
    // Rust str::trim keeps U+FEFF (not White_Space), unlike Dart's trim
    expect(MeivinTextRecipe.rolePrefixed.queryText('﻿a　'), '[QUERY] ﻿a');
    expect(() => MeivinTextRecipe.fromVersion(9), throwsUnsupportedError);
  });

  final haveModel = File('$_modelDir/model.json').existsSync() && File(_ort).existsSync();
  final haveGolden = File(_golden).existsSync();

  test('installed model: query ids and vectors close to the reference', () {
    final e = MeivinEmbedder.open(_modelDir, _ort);
    try {
      expect(e.dims, 256);
      expect(() => e.queryTokens(' \t'), throwsArgumentError);
      final queries = _cases().where((c) => c['role'] == 'query').toList();
      expect(queries, isNotEmpty);
      for (final c in queries) {
        final input = c['input'] as String; // already "[QUERY] ..." (the role prefix)
        final ids = e.tokenizer.encode(input, maxTokens: 256);
        expect(ids, c['token_ids'], reason: c['name'] as String);
        final v = e.embedTokens(ids);
        expect(_dot(v, v), closeTo(1, 1e-5));
        // the reference graph is fp32, the installed one int8: close, not equal
        final ref = meivinL2Normalize(_vec(c));
        expect(_dot(v, ref), greaterThan(0.9), reason: c['name'] as String);
      }
      final batch = e.embedQueries(['a', 'b']);
      expect(batch[0], e.embedQuery('a'));
      e.dispose();
      expect(() => e.embedQuery('a'), throwsStateError);
    } finally {
      e.dispose();
    }
  },
      skip: !haveModel || !haveGolden
          ? 'needs the installed Meivin model (%APPDATA%/otzaria/books/meivin-qa491-onnx), onnxruntime '
              '(OTZARIA_ONNX_RUNTIME) and the otzaria-semantic-search golden file'
          : false);

  // Exact equality needs the reference fp32 graph the goldens were made with.
  final fp32 = _env['OTZARIA_TEST_ONNX_MODEL'];
  test('reference fp32 graph: every golden vector', () {
    final e = MeivinEmbedder.load(
        modelPath: fp32!,
        tokenizerPath: '$_modelDir/tokenizer.json',
        ortLibrary: _ort,
        dims: 256,
        maxTokens: 256);
    try {
      for (final c in _cases()) {
        final v = e.embedTokens(e.tokenizer.encode(c['input'] as String, maxTokens: 256));
        final ref = meivinL2Normalize(_vec(c));
        for (var i = 0; i < v.length; i++) {
          expect(v[i], closeTo(ref[i], 1e-5), reason: '${c['name']} [$i]');
        }
      }
    } finally {
      e.dispose();
    }
  }, skip: fp32 == null || !haveModel || !haveGolden ? 'set OTZARIA_TEST_ONNX_MODEL to the reference fp32 graph' : false);
}
