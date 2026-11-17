// Query embedding with Otzaria's Meivin model, as Otzaria does it
// (otzaria-semantic-search recipe.rs + onnx_backend.rs): text = "[QUERY] " +
// query.trim(); tokenizer.json with the [CLS] A [SEP] template, truncated on
// the right to query_max_tokens (specials included); inputs input_ids and an
// all-ones attention_mask, int64 [1, len] (token_type_ids zeros only when the
// graph declares it); the first output [1, dim], L2-normalized.
//
// The ONNX runtime is reached through its own C API with dart:ffi (pub.dev's
// `onnxruntime` package is a Flutter plugin and does not run in a console
// app). The host passes the onnxruntime library path (e.g. the Microsoft.ML.OnnxRuntime NuGet
// package's runtimes/win-x64/native/onnxruntime.dll, 1.28.0).
//
// Host-side code: the model stays in the host, never in the engine.

import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'meivin_tokenizer.dart';

/// The default model folder of an Otzaria install.
String? defaultMeivinDir() {
  final appData = Platform.environment['APPDATA'];
  if (appData == null) return null;
  final d = '$appData/otzaria/books/meivin-qa491-onnx';
  return Directory(d).existsSync() ? d : null;
}

// OrtApi slot indices (onnxruntime_c_api.h, struct OrtApi; stable since API 1).
const _getErrorMessage = 2,
    _createEnv = 3,
    _createSession = 7,
    _run = 9,
    _createSessionOptions = 10,
    _setIntraOpNumThreads = 24,
    _sessionGetInputCount = 30,
    _sessionGetInputName = 36,
    _sessionGetOutputName = 37,
    _createTensorWithData = 49,
    _getTensorMutableData = 51,
    _getDimensionsCount = 61,
    _getDimensions = 62,
    _getTensorTypeAndShape = 65,
    _createCpuMemoryInfo = 69,
    _allocatorFree = 76,
    _getAllocatorWithDefaultOptions = 78,
    _releaseEnv = 92,
    _releaseStatus = 93,
    _releaseMemoryInfo = 94,
    _releaseSession = 95,
    _releaseValue = 96,
    _releaseTensorTypeAndShapeInfo = 99,
    _releaseSessionOptions = 100;

typedef _St = Pointer<Void>; // OrtStatus*, null = success

/// Otzaria's text recipe (recipe.rs `EmbeddingTextRecipe`, model.json
/// `embedding_text_version`): what a query / passage carries to the model.
enum MeivinTextRecipe {
  /// 1: the text as it is, on both sides.
  plain,

  /// 2: `"[QUERY] " + trim(query)` and `"[PASSAGE] " + trim(passage)`.
  rolePrefixed,

  /// 3 (Pardes): `"query: " + query`; passages as they are.
  pardes;

  static MeivinTextRecipe fromVersion(int v) => switch (v) {
        1 => plain,
        2 => rolePrefixed,
        3 => pardes,
        _ => throw UnsupportedError('embedding_text_version $v (supported: 1, 2, 3)'),
      };

  String queryText(String q) => switch (this) {
        plain => q,
        pardes => 'query: $q',
        rolePrefixed => '[QUERY] ${_trim(q)}',
      };

  String passageText(String t) => this == rolePrefixed ? '[PASSAGE] ${_trim(t)}' : t;
}

/// Otzaria's text normalization (recipe.rs `TextNormalizationRecipe`, model.json
/// `normalization_version`): applied to the text on both sides, before the
/// [MeivinTextRecipe] adds its role prefix.
enum MeivinTextNormalization {
  /// 1 (`AsSuppliedByCorpus`): the text as it is.
  asSupplied,

  /// 2 (`PardesClean1`): pardes_clean.rs `clean` ([meivinPardesClean]).
  pardesClean;

  static MeivinTextNormalization fromVersion(int v) => switch (v) {
        1 => asSupplied,
        2 => pardesClean,
        _ => throw UnsupportedError('normalization_version $v (supported: 1, 2)'),
      };

  String apply(String text) => this == pardesClean ? meivinPardesClean(text) : text;
}

/// pardes_clean.rs `space`: Rust White_Space plus U+FEFF.
bool _pardesSpace(int c) =>
    (c >= 0x09 && c <= 0x0D) ||
    c == 0x20 ||
    c == 0x85 ||
    c == 0xA0 ||
    c == 0x1680 ||
    (c >= 0x2000 && c <= 0x200A) ||
    c == 0x2028 ||
    c == 0x2029 ||
    c == 0x202F ||
    c == 0x205F ||
    c == 0x3000 ||
    c == 0xFEFF;

/// pardes_clean.rs `entity`: the code point a named or numeric entity body stands for.
int? _pardesEntity(String s) {
  switch (s) {
    case 'amp':
      return 0x26;
    case 'lt':
      return 0x3C;
    case 'gt':
      return 0x3E;
    case 'quot':
      return 0x22;
    case 'apos':
      return 0x27;
    case 'nbsp':
      return 0x20;
  }
  String digits;
  int radix, max;
  if (s.startsWith('#x') || s.startsWith('#X')) {
    (digits, radix, max) = (s.substring(2), 16, 6);
  } else if (s.startsWith('#')) {
    (digits, radix, max) = (s.substring(1), 10, 7);
  } else {
    return null;
  }
  // Rust counts bytes; a non-ASCII char fails the digit test below anyway.
  if (digits.isEmpty || digits.length > max) return null;
  for (final c in digits.codeUnits) {
    final dec = c >= 0x30 && c <= 0x39;
    final hex = dec || (c >= 0x41 && c <= 0x46) || (c >= 0x61 && c <= 0x66);
    if (!(radix == 16 ? hex : dec)) return null;
  }
  final n = int.parse(digits, radix: radix);
  // char::from_u32: no surrogates, nothing past U+10FFFF; 0 is refused.
  if (n == 0 || n > 0x10FFFF || (n >= 0xD800 && n <= 0xDFFF)) return null;
  return n;
}

/// UTF-8 length of [s] (Rust string offsets are bytes).
int _utf8Len(String s) {
  var n = 0;
  for (final r in s.runes) {
    n += r < 0x80 ? 1 : r < 0x800 ? 2 : r < 0x10000 ? 3 : 4;
  }
  return n;
}

/// Otzaria's pardes-clean-1 (otzaria-semantic-search pardes_clean.rs `clean`) step
/// for step: every `<...>` tag becomes a space (an unclosed `<` stays text),
/// entities of at most 8 bytes are decoded, whitespace runs fold to one space
/// (none leading or trailing), and U+05F4 / U+05F3 become `"` / `'`.
String meivinPardesClean(String text) {
  final tags = StringBuffer();
  var rest = text;
  while (true) {
    final i = rest.indexOf('<');
    if (i < 0) break;
    tags.write(rest.substring(0, i));
    final j = rest.indexOf('>', i);
    if (j >= 0) {
      tags.write(' ');
      rest = rest.substring(j + 1);
    } else {
      tags.write(rest.substring(i));
      rest = '';
      break;
    }
  }
  tags.write(rest);
  final decoded = StringBuffer();
  rest = tags.toString();
  while (true) {
    final i = rest.indexOf('&');
    if (i < 0) break;
    decoded.write(rest.substring(0, i));
    final tail = rest.substring(i + 1);
    final j = tail.indexOf(';');
    final c = (j >= 0 && _utf8Len(tail.substring(0, j)) <= 8) ? _pardesEntity(tail.substring(0, j)) : null;
    if (c != null) {
      decoded.writeCharCode(c);
      rest = tail.substring(j + 1);
    } else {
      decoded.write('&');
      rest = tail;
    }
  }
  decoded.write(rest);
  final out = StringBuffer();
  var pending = false;
  for (final c in decoded.toString().runes) {
    if (_pardesSpace(c)) {
      pending = out.isNotEmpty;
      continue;
    }
    if (pending) {
      out.write(' ');
      pending = false;
    }
    out.writeCharCode(c == 0x05F4 ? 0x22 : c == 0x05F3 ? 0x27 : c);
  }
  return out.toString();
}

/// Rust `str::trim` (Unicode White_Space). Dart's `trim` also strips U+FEFF,
/// which is not White_Space, so it is spelled out.
String _trim(String s) {
  bool ws(int c) =>
      (c >= 0x09 && c <= 0x0D) ||
      c == 0x20 ||
      c == 0x85 ||
      c == 0xA0 ||
      c == 0x1680 ||
      (c >= 0x2000 && c <= 0x200A) ||
      c == 0x2028 ||
      c == 0x2029 ||
      c == 0x202F ||
      c == 0x205F ||
      c == 0x3000;
  var a = 0, b = s.length;
  while (a < b && ws(s.codeUnitAt(a))) {
    a++;
  }
  while (b > a && ws(s.codeUnitAt(b - 1))) {
    b--;
  }
  return s.substring(a, b);
}

/// Otzaria's `l2_normalize` (embedding.rs) in f32 step for step: the squares
/// summed in order in f32, the f32 square root, then each value times the f32
/// reciprocal. Throws on a zero / non-finite norm (`normalize_validated`).
Float32List meivinL2Normalize(Float32List v) {
  final f = Float32List(3); // [0] sum, [1] norm, [2] inv
  for (final x in v) {
    f[2] = x * x;
    f[0] = f[0] + f[2];
  }
  f[1] = math.sqrt(f[0]);
  final norm = f[1];
  if (!norm.isFinite || norm <= 1e-12) throw StateError('the model returned a vector of norm $norm');
  f[2] = 1.0 / norm;
  final inv = f[2];
  for (var i = 0; i < v.length; i++) {
    v[i] = v[i] * inv;
  }
  return v;
}

/// Otzaria's Meivin embedding model run with ONNX Runtime (its C API through
/// dart:ffi). Synchronous; one instance per isolate, [dispose] when done.
class MeivinEmbedder {
  MeivinEmbedder._(this.tokenizer, this.dims, this.maxTokens, this.passageMaxTokens, this.recipe,
      this.normalization, this._api, this._env, this._session, this._mem, this._inputs, this._output);

  final MeivinTokenizer tokenizer;

  /// Output dimensions; token caps (specials included) for queries / passages.
  final int dims, maxTokens, passageMaxTokens;
  final MeivinTextRecipe recipe;
  final MeivinTextNormalization normalization;
  final Pointer<Pointer<Void>> _api;
  final Pointer<Void> _env, _session, _mem;
  final List<String> _inputs;
  final String _output;
  bool _disposed = false;

  /// Opens an Otzaria model folder: [graphFile] (default `query.int8.onnx`),
  /// `tokenizer.json` and `model.json` (dims, token caps, text version,
  /// normalization_version 1 or 2).
  static MeivinEmbedder open(String modelDir, String ortLibrary,
      {int threads = 1, String graphFile = 'query.int8.onnx'}) {
    final meta = jsonDecode(File('$modelDir/model.json').readAsStringSync()) as Map<String, dynamic>;
    final norm = MeivinTextNormalization.fromVersion(meta['normalization_version'] as int? ?? 1);
    return load(
        modelPath: '$modelDir/$graphFile',
        tokenizerPath: '$modelDir/tokenizer.json',
        ortLibrary: ortLibrary,
        dims: meta['embedding_dim'] as int,
        maxTokens: meta['query_max_tokens'] as int? ?? meta['max_tokens'] as int,
        passageMaxTokens: meta['max_tokens'] as int,
        recipe: MeivinTextRecipe.fromVersion(meta['embedding_text_version'] as int? ?? 1),
        normalization: norm,
        threads: threads);
  }

  /// Loads a graph and a tokenizer.json from explicit paths.
  static MeivinEmbedder load(
      {required String modelPath,
      required String tokenizerPath,
      required String ortLibrary,
      required int dims,
      required int maxTokens,
      int? passageMaxTokens,
      MeivinTextRecipe recipe = MeivinTextRecipe.rolePrefixed,
      MeivinTextNormalization normalization = MeivinTextNormalization.asSupplied,
      int threads = 1}) {
    final tok = MeivinTokenizer.load(tokenizerPath);
    final lib = DynamicLibrary.open(ortLibrary);
    final base =
        lib.lookupFunction<Pointer<Pointer<Void>> Function(), Pointer<Pointer<Void>> Function()>('OrtGetApiBase')();
    // OrtApiBase { GetApi(uint32), GetVersionString() }
    final getApi = base[0]
        .cast<NativeFunction<Pointer<Pointer<Void>> Function(Uint32)>>()
        .asFunction<Pointer<Pointer<Void>> Function(int)>();
    final api = getApi(17); // every slot used here exists since API 1; 17 = ORT 1.17+
    if (api == nullptr) throw StateError('onnxruntime at $ortLibrary is older than 1.17');
    final o = _Ort(api);
    return using((al) {
      final pp = al<Pointer<Void>>();
      final logId = 'meivin'.toNativeUtf8(allocator: al);
      o.check(o.api[_createEnv]
              .cast<NativeFunction<_St Function(Int32, Pointer<Utf8>, Pointer<Pointer<Void>>)>>()
              .asFunction<_St Function(int, Pointer<Utf8>, Pointer<Pointer<Void>>)>()(
          3, logId, pp)); // ORT_LOGGING_LEVEL_ERROR
      final env = pp.value;
      o.check(o.api[_createSessionOptions]
          .cast<NativeFunction<_St Function(Pointer<Pointer<Void>>)>>()
          .asFunction<_St Function(Pointer<Pointer<Void>>)>()(pp));
      final opts = pp.value;
      o.check(o.api[_setIntraOpNumThreads]
          .cast<NativeFunction<_St Function(Pointer<Void>, Int32)>>()
          .asFunction<_St Function(Pointer<Void>, int)>()(opts, threads));
      final model = modelPath;
      // ORTCHAR_T is wchar_t (UTF-16) on Windows, char elsewhere
      final Pointer<Void> path =
          Platform.isWindows ? _utf16(model, al).cast() : model.toNativeUtf8(allocator: al).cast();
      o.check(o.api[_createSession]
              .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Void>, Pointer<Void>, Pointer<Pointer<Void>>)>>()
              .asFunction<_St Function(Pointer<Void>, Pointer<Void>, Pointer<Void>, Pointer<Pointer<Void>>)>()(
          env, path, opts, pp));
      final session = pp.value;
      o.release(_releaseSessionOptions, opts);
      o.check(o.api[_createCpuMemoryInfo]
              .cast<NativeFunction<_St Function(Int32, Int32, Pointer<Pointer<Void>>)>>()
              .asFunction<_St Function(int, int, Pointer<Pointer<Void>>)>()(
          0, 0, pp)); // OrtArenaAllocator, OrtMemTypeDefault
      final mem = pp.value;
      o.check(o.api[_getAllocatorWithDefaultOptions]
          .cast<NativeFunction<_St Function(Pointer<Pointer<Void>>)>>()
          .asFunction<_St Function(Pointer<Pointer<Void>>)>()(pp));
      final alloc = pp.value;
      String name(int slot, int i) {
        final sp = al<Pointer<Utf8>>();
        o.check(o.api[slot]
                .cast<NativeFunction<_St Function(Pointer<Void>, Size, Pointer<Void>, Pointer<Pointer<Utf8>>)>>()
                .asFunction<_St Function(Pointer<Void>, int, Pointer<Void>, Pointer<Pointer<Utf8>>)>()(
            session, i, alloc, sp));
        final s = sp.value.toDartString();
        o.check(o.api[_allocatorFree]
            .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Void>)>>()
            .asFunction<_St Function(Pointer<Void>, Pointer<Void>)>()(alloc, sp.value.cast()));
        return s;
      }

      final count = al<Size>();
      o.check(o.api[_sessionGetInputCount]
          .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Size>)>>()
          .asFunction<_St Function(Pointer<Void>, Pointer<Size>)>()(session, count));
      final inputs = [for (var i = 0; i < count.value; i++) name(_sessionGetInputName, i)];
      for (final i in inputs) {
        if (i != 'input_ids' && i != 'attention_mask' && i != 'token_type_ids') {
          throw StateError('the graph declares an unknown input "$i"');
        }
      }
      final output = name(_sessionGetOutputName, 0);
      return MeivinEmbedder._(
          tok, dims, maxTokens, passageMaxTokens ?? maxTokens, recipe, normalization, api, env, session, mem, inputs, output);
    });
  }

  /// The token ids Otzaria feeds for a query (recipe.rs `query_input`: the
  /// query normalized, refused when no text is left after trimming, then the
  /// recipe's query text).
  List<int> queryTokens(String query) {
    final normalized = normalization.apply(query);
    if (_trim(normalized).isEmpty) throw ArgumentError.value(query, 'query', 'holds no text, nothing to embed');
    return tokenizer.encode(recipe.queryText(normalized), maxTokens: maxTokens);
  }

  /// The token ids for a stored passage (normalized, then recipe `passage_text`, `max_tokens`).
  List<int> passageTokens(String text) =>
      tokenizer.encode(recipe.passageText(normalization.apply(text)), maxTokens: passageMaxTokens);

  /// The query's embedding, L2-normalized (Otzaria's query vector).
  Float32List embedQuery(String query) => embedTokens(queryTokens(query));

  /// Each query embedded alone (one `[1, len]` run per text, never batched, as
  /// Otzaria's backend: a padded batch would move the vectors).
  List<Float32List> embedQueries(Iterable<String> queries) => [for (final q in queries) embedQuery(q)];

  /// Passages embedded with the passage recipe, one run per text.
  List<Float32List> embedPassages(Iterable<String> texts) => [for (final t in texts) embedTokens(passageTokens(t))];

  /// The L2-normalized output for token ids as given.
  Float32List embedTokens(List<int> ids) {
    if (_disposed) throw StateError('MeivinEmbedder is disposed');
    final o = _Ort(_api);
    final n = ids.length;
    return using((al) {
      final shape = al<Int64>(2)
        ..[0] = 1
        ..[1] = n;
      final values = <Pointer<Void>>[];
      final names = al<Pointer<Utf8>>(_inputs.length);
      final inVals = al<Pointer<Void>>(_inputs.length);
      try {
        for (var k = 0; k < _inputs.length; k++) {
          final data = al<Int64>(n);
          for (var i = 0; i < n; i++) {
            data[i] = switch (_inputs[k]) { 'input_ids' => ids[i], 'attention_mask' => 1, _ => 0 };
          }
          final pv = al<Pointer<Void>>();
          o.check(o.api[_createTensorWithData]
                  .cast<
                      NativeFunction<
                          _St Function(Pointer<Void>, Pointer<Void>, Size, Pointer<Int64>, Size, Int32,
                              Pointer<Pointer<Void>>)>>()
                  .asFunction<
                      _St Function(
                          Pointer<Void>, Pointer<Void>, int, Pointer<Int64>, int, int, Pointer<Pointer<Void>>)>()(
              _mem, data.cast(), n * 8, shape, 2, 7, pv)); // ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64
          values.add(pv.value);
          names[k] = _inputs[k].toNativeUtf8(allocator: al);
          inVals[k] = pv.value;
        }
        final outName = al<Pointer<Utf8>>()..value = _output.toNativeUtf8(allocator: al);
        final outVal = al<Pointer<Void>>()..value = nullptr;
        o.check(o.api[_run]
            .cast<
                NativeFunction<
                    _St Function(Pointer<Void>, Pointer<Void>, Pointer<Pointer<Utf8>>, Pointer<Pointer<Void>>, Size,
                        Pointer<Pointer<Utf8>>, Size, Pointer<Pointer<Void>>)>>()
            .asFunction<
                _St Function(
                    Pointer<Void>,
                    Pointer<Void>,
                    Pointer<Pointer<Utf8>>,
                    Pointer<Pointer<Void>>,
                    int,
                    Pointer<Pointer<Utf8>>,
                    int,
                    Pointer<Pointer<Void>>)>()(_session, nullptr, names, inVals, _inputs.length, outName, 1, outVal));
        values.add(outVal.value);
        // shape check: [1, dims]
        final info = al<Pointer<Void>>();
        o.check(o.api[_getTensorTypeAndShape]
            .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Pointer<Void>>)>>()
            .asFunction<_St Function(Pointer<Void>, Pointer<Pointer<Void>>)>()(outVal.value, info));
        final dc = al<Size>();
        o.check(o.api[_getDimensionsCount]
            .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Size>)>>()
            .asFunction<_St Function(Pointer<Void>, Pointer<Size>)>()(info.value, dc));
        final dimsP = al<Int64>(dc.value);
        o.check(o.api[_getDimensions]
            .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Int64>, Size)>>()
            .asFunction<_St Function(Pointer<Void>, Pointer<Int64>, int)>()(info.value, dimsP, dc.value));
        final shapeOut = [for (var i = 0; i < dc.value; i++) dimsP[i]];
        o.release(_releaseTensorTypeAndShapeInfo, info.value);
        final elems = shapeOut.fold<int>(1, (a, b) => a * b);
        if (elems != dims) throw StateError('model output shape $shapeOut, expected [1, $dims]');
        final dp = al<Pointer<Void>>();
        o.check(o.api[_getTensorMutableData]
            .cast<NativeFunction<_St Function(Pointer<Void>, Pointer<Pointer<Void>>)>>()
            .asFunction<_St Function(Pointer<Void>, Pointer<Pointer<Void>>)>()(outVal.value, dp));
        final v = Float32List.fromList(dp.value.cast<Float>().asTypedList(dims));
        if (v.any((x) => !x.isFinite)) throw StateError('the model returned a non-finite vector');
        return meivinL2Normalize(v);
      } finally {
        for (final v in values) {
          o.release(_releaseValue, v);
        }
      }
    });
  }

  /// Frees the ONNX session; further calls throw. Idempotent.
  void dispose() {
    if (_disposed) return;
    _disposed = true;
    final o = _Ort(_api);
    o.release(_releaseMemoryInfo, _mem);
    o.release(_releaseSession, _session);
    o.release(_releaseEnv, _env);
  }

  static Pointer<Uint16> _utf16(String s, Allocator al) {
    final p = al<Uint16>(s.length + 1);
    for (var i = 0; i < s.length; i++) {
      p[i] = s.codeUnitAt(i);
    }
    p[s.length] = 0;
    return p;
  }
}

class _Ort {
  _Ort(this.api);
  final Pointer<Pointer<Void>> api;

  void check(_St status) {
    if (status == nullptr) return;
    final msg = api[_getErrorMessage]
        .cast<NativeFunction<Pointer<Utf8> Function(Pointer<Void>)>>()
        .asFunction<Pointer<Utf8> Function(Pointer<Void>)>()(status)
        .toDartString();
    release(_releaseStatus, status);
    throw StateError('onnxruntime: $msg');
  }

  void release(int slot, Pointer<Void> p) {
    if (p == nullptr) return;
    api[slot].cast<NativeFunction<Void Function(Pointer<Void>)>>().asFunction<void Function(Pointer<Void>)>()(p);
  }
}
