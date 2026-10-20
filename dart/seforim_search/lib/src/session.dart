import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'index.dart';
import 'models.dart';
import 'native.dart';

/// One search: every matching line (catalog order) and the snippet state
/// (prepared groups, distance/order filters, term table). Not thread-safe.
final class SearchSession implements Finalizable {
  SearchSession.internal(this._index, this._handle, this.contextWords) {
    _n.searchCloseFinalizer.attach(this, _handle, detach: this);
    hitCount = _n.searchHitCount(_handle);
  }

  static SeforimNative get _n => SeforimNative.instance;

  // ignore: unused_field
  final SeforimIndex _index;
  Pointer<Void> _handle;

  /// Words of context per side this search locates with.
  final int contextWords;

  /// Number of matching lines (before the snippet filters).
  late final int hitCount;

  Pointer<Void> get _h {
    if (_handle == nullptr) throw StateError('SearchSession is closed');
    return _handle;
  }

  /// Hits [offset, offset+count), catalog order.
  List<PlacedHit> hits(int offset, int count) {
    if (count <= 0 || offset >= hitCount) return const [];
    if (count > hitCount - offset) count = hitCount - offset;
    final raw = calloc<Uint64>(count * 5);
    try {
      final copied = _n.searchCopyHits(_h, offset, count, raw);
      final v = raw.asTypedList(copied * 5);
      return [
        for (var i = 0; i < copied; i++) PlacedHit(v[i * 5], v[i * 5 + 1], v[i * 5 + 2], v[i * 5 + 3], v[i * 5 + 4]),
      ];
    } finally {
      calloc.free(raw);
    }
  }

  /// Every hit, catalog order, paged.
  Iterable<PlacedHit> allHits({int pageSize = 4096}) sync* {
    for (var o = 0; o < hitCount; o += pageSize) {
      yield* hits(o, pageSize);
    }
  }

  /// Locates the snippets of a batch of matched lines' texts. Null entries are
  /// lines that do not pass the filters. Offsets index each text.
  List<Snippet?> locateBatch(List<String> texts) {
    final n = texts.length;
    final out = List<Snippet?>.filled(n, null);
    if (n == 0) return out;
    final data = using((a) {
      final ptrs = a<Pointer<Uint8>>(n);
      final lens = a<Size>(n);
      for (var i = 0; i < n; i++) {
        final (p, l) = utf8Native(texts[i], a);
        ptrs[i] = p;
        lens[i] = l;
      }
      return _n.takeBuffer(_n.locateBatch(_h, ptrs, lens, n));
    });
    var at = 0;
    for (var i = 0; i < n; i++) {
      if (data[at++] == 0) continue;
      final (s, next) = Snippet.read(data, at);
      out[i] = s;
      at = next;
    }
    return out;
  }

  /// One line's snippet.
  Snippet? locate(String text) => locateBatch([text])[0];

  /// Re-locates a SHORT window over `previous + ' ' + text + ' ' + next`
  /// (empty sides dropped). Offsets index that joined text. Null if it fails
  /// (keep the original snippet then).
  Snippet? locateWithNeighbours(String previous, String text, String next, Snippet matchedLine) {
    final data = using((a) {
      final (p, pl) = utf8Native(previous, a);
      final (t, tl) = utf8Native(text, a);
      final (x, xl) = utf8Native(next, a);
      return _n.takeBuffer(
          _n.locateWithNeighbours(_h, p, pl, t, tl, x, xl, matchedLine.score, matchedLine.wordDistance));
    });
    if (data.isEmpty || data[0] == 0) return null;
    return Snippet.read(data, 1).$1;
  }

  /// Terms added to the term table since the previous call, in id order.
  /// A mark's termId indexes the concatenation of every call's result.
  List<String> takeNewTerms() =>
      (jsonDecode(_n.takeString(_n.takeNewTerms(_h))) as List).cast<String>();

  void close() {
    if (_handle == nullptr) return;
    _n.searchCloseFinalizer.detach(this);
    _n.searchClose(_handle);
    _handle = nullptr;
  }
}
