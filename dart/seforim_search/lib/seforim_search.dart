/// Dart binding for the seforim_search_native Hebrew full-text engine.
///
/// Mirrors the C# package `SeforimSearch.Engine`: [SeforimIndex],
/// [IndexWriter], [SearchSession] and the request/result models.
library;

export 'src/models.dart';
export 'src/native.dart' show SeforimNative, SeforimEngineException, SearchCancelledException;
export 'src/index.dart';
export 'src/session.dart';
