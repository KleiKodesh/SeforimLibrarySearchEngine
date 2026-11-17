/// Dart binding for the seforim_search_native Hebrew full-text engine.
///
/// Mirrors the C# package `SeforimSearch.Engine`: [SeforimIndex],
/// [IndexWriter], [SearchSession] and the request/result models.
library;

export 'src/models.dart';
export 'src/native.dart' show SeforimNative, SeforimEngineException, NativeAbiMismatchException, seforimAbiVersion, SearchCancelledException, StaleResumeException;
export 'src/index.dart' hide writerResortAndCloseAt;
export 'src/session.dart';
export 'src/async_search.dart';
export 'src/semantic.dart';
export 'src/vector_set.dart';
export 'src/otzaria_text.dart';

export 'src/lexical/lexicon_text.dart';
export 'src/lexical/translation_lexicon.dart';
