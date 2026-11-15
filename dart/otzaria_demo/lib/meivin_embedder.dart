// Query embedding with Otzaria's Meivin model, as Otzaria does it
// (otzaria-semantic-search recipe.rs + onnx_backend.rs): text = "[QUERY] " +
// query.trim(); tokenizer.json with the [CLS] A [SEP] template, truncated on
// the right to query_max_tokens (specials included); inputs input_ids and an
// all-ones attention_mask, int64 [1, len] (token_type_ids zeros only when the
// graph declares it); the first output [1, dim], L2-normalized.
//
// The ONNX runtime is reached through its own C API with dart:ffi (pub.dev's
// `onnxruntime` package is a Flutter plugin and does not run in a console
// app). Pass the onnxruntime library (e.g. the Microsoft.ML.OnnxRuntime NuGet
// package's runtimes/win-x64/native/onnxruntime.dll, 1.28.0) with --ort.
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

class MeivinEmbedder {
  MeivinEmbedder._(this.tokenizer, this.dims, this.maxTokens, this._api, this._env, this._session, this._mem,
      this._inputs, this._output);

  final MeivinTokenizer tokenizer;
  final int dims, maxTokens;
  final Pointer<Pointer<Void>> _api;
  final Pointer<Void> _env, _session, _mem;
  final List<String> _inputs;
  final String _output;

  /// Opens the model folder (query.int8.onnx, tokenizer.json, model.json).
  static MeivinEmbedder open(String modelDir, String ortLibrary, {int threads = 1}) {
    final meta = jsonDecode(File('$modelDir/model.json').readAsStringSync()) as Map<String, dynamic>;
    final dims = meta['embedding_dim'] as int;
    final maxTokens = meta['query_max_tokens'] as int? ?? meta['max_tokens'] as int;
    final tok = MeivinTokenizer.load('$modelDir/tokenizer.json');
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
      final model = '$modelDir/query.int8.onnx';
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
      return MeivinEmbedder._(tok, dims, maxTokens, api, env, session, mem, inputs, output);
    });
  }

  /// The token ids Otzaria feeds for a query.
  List<int> queryTokens(String query) => tokenizer.encode('[QUERY] ${query.trim()}', maxTokens: maxTokens);

  /// The query's embedding, L2-normalized.
  Float32List embedQuery(String query) => embedTokens(queryTokens(query));

  Float32List embedTokens(List<int> ids) {
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
        var norm = 0.0;
        for (final x in v) {
          norm += x * x;
        }
        norm = math.sqrt(norm);
        if (norm == 0 || norm.isNaN) throw StateError('the model returned a zero / NaN vector');
        for (var i = 0; i < dims; i++) {
          v[i] = v[i] / norm;
        }
        return v;
      } finally {
        for (final v in values) {
          o.release(_releaseValue, v);
        }
      }
    });
  }

  void close() {
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
