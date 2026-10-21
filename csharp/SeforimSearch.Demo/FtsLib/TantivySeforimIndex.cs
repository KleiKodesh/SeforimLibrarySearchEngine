using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Threading;
using FtsLib.SeforimDb;
using FtsLib.Snippets;
using SeforimSearch.Demo.SeforimDbFullTextSearch;
using SeforimSearch.Engine;

namespace FtsLib.Fts5
{
    /// <summary>
    /// The engine handle a search's results share: the native session (snippet
    /// state) plus its term list. Locates are serialized (the session is not
    /// thread-safe); batch locates run multi-core inside the engine.
    /// </summary>
    /// <summary>Opt-in host-stage counters (SEFORIM_HOST_TIMING): time spent in the
    /// engine's snippet locate, split into batch locates and single-text locates (the
    /// per-hit neighbour widening), with call counts. Diagnostic only.</summary>
    public static class HostTiming
    {
        public static readonly bool Enabled =
            !string.IsNullOrEmpty(Environment.GetEnvironmentVariable("SEFORIM_HOST_TIMING"));
        public static long BatchTicks, BatchCalls, BatchTexts, SingleTicks, SingleCalls;

        public static void Reset() { BatchTicks = BatchCalls = BatchTexts = SingleTicks = SingleCalls = 0; }

        public static string Report() =>
            $"[host] locate batches: {BatchCalls} calls, {BatchTexts} texts, {BatchTicks * 1000.0 / System.Diagnostics.Stopwatch.Frequency:F0} ms; " +
            $"single locates (widening): {SingleCalls} calls, {SingleTicks * 1000.0 / System.Diagnostics.Stopwatch.Frequency:F0} ms";
    }

    internal sealed class SearchSessionRef : IDisposable
    {
        private readonly SearchSession _session;
        private readonly List<string> _terms = new List<string>();
        private readonly object _lock = new object();

        public SearchSessionRef(SearchSession session) { _session = session; }

        public SearchSession Session => _session;

        public string TermAt(int id)
        {
            lock (_lock) return id >= 0 && id < _terms.Count ? _terms[id] : string.Empty;
        }

        public Snippet?[] LocateBatch(IReadOnlyList<string> texts)
        {
            lock (_lock)
            {
                long t0 = HostTiming.Enabled ? System.Diagnostics.Stopwatch.GetTimestamp() : 0;
                var located = _session.LocateBatch(texts);
                _terms.AddRange(_session.TakeNewTerms());
                if (HostTiming.Enabled)
                {
                    long dt = System.Diagnostics.Stopwatch.GetTimestamp() - t0;
                    if (texts.Count == 1) { HostTiming.SingleTicks += dt; HostTiming.SingleCalls++; }
                    else { HostTiming.BatchTicks += dt; HostTiming.BatchCalls++; HostTiming.BatchTexts += texts.Count; }
                }
                return located;
            }
        }

        public void Dispose() => _session.Dispose();
    }

    /// <summary>Search settings FtsLib's engine did not take but this one applies
    /// while locating (FtsLib filtered them in Core; Core's checks stay harmless).</summary>
    public sealed class TantivySearchOptions
    {
        /// <summary>Otzaria's per-word options (prefixes, suffixes, ktiv, nikud…).
        /// IgnoreQuotes defaults ON here: FtsLib always matched רשי ↔ רש"י.</summary>
        public WalkOptions WordOptions { get; set; } = new WalkOptions { IgnoreQuotes = true };

        /// <summary>מרחק בין מילים. Null = no limit.</summary>
        public int? MaxWordDistance { get; set; }

        public bool RequireOrdered { get; set; }

        /// <summary>Otzaria's user regex (/.../). Off in FtsLib mode: FtsLib had none.</summary>
        public bool UserRegex { get; set; }

        public int ContextWords { get; set; } = TantivySeforimIndex.DefaultContextWords;

        /// <summary>Result order (Otzaria's). Catalog, as FtsLib, by default.</summary>
        public ResultsOrder Order { get; set; }
    }

    /// <summary>
    /// The tantivy engine behind the full-text search, shaped like FtsLib's
    /// Fts5SeforimIndex: Search streams SearchResults in catalog order with their
    /// content fetched in the caller's batch plan; LocateSnippetWindow and
    /// LocateSnippetWindowOverNeighbourLines return SnippetDescriptors; BuildLines
    /// builds or resumes the index from the corpus.
    /// </summary>
    public sealed class TantivySeforimIndex : IDisposable
    {
        /// <summary>The file whose presence means a searchable index.</summary>
        public const string IndexFileName = "meta.json";

        public const int DefaultContextWords = 8;

        /// <summary>Bumped when the indexing pipeline changes; part of the provenance stamp.</summary>
        public const string IndexFormatVersion = "tantivy8"; // 2: trailing quotes dropped from terms; 3: bare-minimum schema (no line_index/toc_entry_id); 5: one-letter (and 30+ letter) words indexed with positions; 6: FtsLib word model = Otzaria's (digit runs are words, ב5 one word); 7: Otzaria's remaining tokenizer rules (. [ ] bidi chars join, no script split, geresh kept as ', trailing geresh, quote and presentation-form folding, paired readings share a position, a term of 128 bytes or more not indexed); 8: a keep_marks index keeps marks in canonical order, flags the lines carrying a mark, measures the 128-byte limit without marks

        private readonly string _indexPath;
        private readonly Func<SeforimDbFtsCorpus> _openCorpus;
        private readonly string _seforimDbPath;
        private SeforimIndex? _index;
        private readonly object _openLock = new object();

        public TantivySeforimIndex(string indexPath, string seforimDbPath, Func<SeforimDbFtsCorpus> openCorpus)
        {
            _indexPath = indexPath ?? throw new ArgumentNullException(nameof(indexPath));
            _seforimDbPath = seforimDbPath ?? throw new ArgumentNullException(nameof(seforimDbPath));
            _openCorpus = openCorpus ?? throw new ArgumentNullException(nameof(openCorpus));
        }

        public bool IsBuilt => SeforimIndex.Exists(_indexPath) && (ReadBuildState()?.Complete ?? false);

        private SeforimIndex Engine()
        {
            lock (_openLock)
                return _index ??= SeforimIndex.Open(_indexPath);
        }

        // ── Search ────────────────────────────────────────────────────

        /// <summary>
        /// Every matching line in catalog order, its content fetched from the
        /// corpus in batches of <paramref name="batchSizeAt"/>. Results are lazy:
        /// stop enumerating to stop. Each result carries the search's snippet state.
        /// </summary>
        public IEnumerable<SearchResult> Search(string query, TantivySearchOptions? options = null,
                                                CancellationToken ct = default, Action<string>? onStage = null,
                                                Func<int, int>? batchSizeAt = null)
        {
            if (!SeforimIndex.Exists(_indexPath)) yield break;
            options ??= new TantivySearchOptions();
            if (string.IsNullOrWhiteSpace(query)) yield break;
            // The engine parses the query (FtsLib's QueryParser rules) and builds
            // one walk per word with these options.
            var request = ToRequest(query, options);

            onStage?.Invoke("search");
            // NOT disposed with the enumeration: results outlive it (Core snippets the
            // last batch after the loop ends), as FtsLib's results carried their own
            // prepared state. The native session is freed when the last result holding
            // it is collected (SafeHandle).
            var session = new SearchSessionRef(Engine().Search(request)); // no ct: the session outlives the enumeration (FtsLib-shaped locates never throw)
            using var corpus = _openCorpus();
            batchSizeAt ??= _ => 256;

            long offset = 0;
            int batchIndex = 0;
            bool firstFetch = true;
            while (offset < session.Session.HitCount)
            {
                ct.ThrowIfCancellationRequested();
                int size = batchSizeAt(batchIndex++);
                PlacedHit[] hits = session.Session.GetHits(offset, size);
                offset += hits.Length;
                if (hits.Length == 0) break;

                if (firstFetch) { onStage?.Invoke("fetch"); firstFetch = false; }
                var ids = new int[hits.Length];
                for (int i = 0; i < hits.Length; i++) ids[i] = (int)hits[i].LineId;
                var byId = new Dictionary<int, (string Text, FtsDocumentPlacement Placement)>(hits.Length);
                foreach (var (id, text, placement) in corpus.FetchPlacedDocuments(ids))
                    byId[id] = (text, placement);

                foreach (int id in ids)
                {
                    if (!byId.TryGetValue(id, out var row)) continue; // line gone from the DB
                    yield return new SearchResult(id, row.Text, row.Placement, session);
                }
            }
        }

        /// <summary>The engine request for a query and this facade's options.</summary>
        private static SearchOptions ToRequest(string query, TantivySearchOptions options)
        {
            var w = options.WordOptions ?? new WalkOptions();
            return new SearchOptions(query)
            {
                GrammarPrefixes = w.GrammarPrefixes,
                GrammarSuffixes = w.GrammarSuffixes,
                AramaicPrefixes = w.AramaicPrefixes,
                AramaicSuffixes = w.AramaicSuffixes,
                KtivMaleHaser = w.KtivMaleHaser,
                PartOfWord = w.PartOfWord,
                AdvancedPrefixes = w.AdvancedPrefixes,
                AdvancedSuffixes = w.AdvancedSuffixes,
                IgnoreQuotes = w.IgnoreQuotes,
                ExactQuotes = w.ExactQuotes,
                SpellingErrors = w.SpellingErrors,
                TypoMode = w.TypoMode,
                MatchNikud = w.MatchNikud,
                MatchTeamim = w.MatchTeamim,
                PrefixMaxLetters = w.PrefixMaxLetters,
                SuffixMaxLetters = w.SuffixMaxLetters,
                PartOfWordMaxLetters = w.PartOfWordMaxLetters,
                GrammarPrefixMaxLetters = w.GrammarPrefixMaxLetters,
                MaxWordDistance = options.MaxWordDistance,
                RequireOrdered = options.RequireOrdered,
                ContextWords = options.ContextWords,
                UserRegex = options.UserRegex,
                Order = options.Order,
            };
        }

        /// <summary>
        /// Exact totals (Otzaria's count / countByBook): the lines passing every
        /// filter, in all and per book id. When the engine says no text is needed
        /// (no text-level check) the hit list is the answer and no text is read;
        /// otherwise the texts are fetched in catalog order and validated by the
        /// engine, multi-core, batch by batch (there, a line gone from the DB is not counted).
        /// </summary>
        public HitCounts CountByBook(string query, TantivySearchOptions? options = null,
                                     CancellationToken ct = default, int batchSize = 4096)
            => CountByBook(query, options, ct, batchSize, out _);

        /// <summary><see cref="CountByBook(string, TantivySearchOptions?, CancellationToken, int)"/>,
        /// also returning how the search was served (notices).</summary>
        public HitCounts CountByBook(string query, TantivySearchOptions? options, CancellationToken ct,
                                     int batchSize, out SearchInfo info)
        {
            options ??= new TantivySearchOptions();
            using var session = Engine().Search(ToRequest(query, options), ct);
            info = session.GetInfo();
            if (!session.CountNeedsText) return session.GetCounts();
            using var corpus = _openCorpus();
            for (long offset = 0; offset < session.HitCount;)
            {
                ct.ThrowIfCancellationRequested();
                PlacedHit[] hits = session.GetHits(offset, batchSize);
                if (hits.Length == 0) break;
                offset += hits.Length;
                var ids = new int[hits.Length];
                for (int i = 0; i < hits.Length; i++) ids[i] = (int)hits[i].LineId;
                var byId = new Dictionary<int, string>(hits.Length);
                foreach (var (id, text, _) in corpus.FetchPlacedDocuments(ids)) byId[id] = text;
                var texts = new string[hits.Length];
                for (int i = 0; i < ids.Length; i++) texts[i] = byId.TryGetValue(ids[i], out var t) ? t : string.Empty;
                session.CountBatch(texts);
            }
            return session.GetCounts();
        }

        // ── Snippet windows ───────────────────────────────────────────

        /// <summary>
        /// Locates a whole batch of results' snippets in one multi-core engine call.
        /// Later <see cref="LocateSnippetWindow"/> calls on these results return the
        /// located window without locating again.
        /// </summary>
        public void LocateSnippetWindows(IReadOnlyList<SearchResult> results)
        {
            if (results.Count == 0) return;
            // results of one search share one session; group defensively anyway
            var bySession = new Dictionary<SearchSessionRef, List<SearchResult>>();
            foreach (var r in results)
            {
                if (!bySession.TryGetValue(r.Session, out var list)) bySession[r.Session] = list = new List<SearchResult>();
                list.Add(r);
            }
            foreach (var kv in bySession)
            {
                var texts = new string[kv.Value.Count];
                for (int i = 0; i < texts.Length; i++) texts[i] = kv.Value[i].Content;
                var located = kv.Key.LocateBatch(texts);
                for (int i = 0; i < texts.Length; i++)
                {
                    kv.Value[i].LocatedSnippet = located[i];
                    kv.Value[i].IsLocated = true;
                }
            }
        }

        /// <summary>The result's snippet window (located in its batch, or now).
        /// <c>IsMatch</c> false when the line does not pass the filters.</summary>
        public SnippetDescriptor LocateSnippetWindow(SearchResult result, bool requireOrdered = false,
                                                     int contextWords = DefaultContextWords)
        {
            if (result == null) return SnippetDescriptor.NoMatch;
            // A line the batch already located and that did not pass has LocatedSnippet
            // null: it must not be located again (that re-located every filtered line of
            // a distance query one native call at a time).
            var snippet = result.IsLocated ? result.LocatedSnippet : result.Session.LocateBatch(new[] { result.Content })[0];
            result.LocatedSnippet = snippet;
            result.IsLocated = true;
            return snippet == null ? SnippetDescriptor.NoMatch : new SnippetDescriptor(snippet, result.Session.TermAt);
        }

        /// <summary>FtsLib's <c>SnippetDescriptorPipeline.LocateOverNeighbourLines</c>:
        /// locates over the matched line PLUS the given surrounding lines (already
        /// fetched, in document order). The text is joined by
        /// <see cref="JoinWithNeighbourLines"/>, then the normal locate runs; the match is
        /// found in the matched line and the context expands into the neighbour text.
        /// Offsets are into that JOINED text. Used only for lines whose own window came
        /// out shorter than the requested context. Deviation: <paramref name="requireOrdered"/>
        /// and <paramref name="contextWords"/> are kept for FtsLib's signature but the native
        /// session locates with the values the search was made with, and applies the
        /// search's MaxWordDistance filter (FtsLib's pipeline did not filter here); the Core
        /// caller passes the same values, and the joined text's tightest window can never be
        /// wider than the line's own, so the result is the same.</summary>
        public SnippetDescriptor LocateSnippetWindowOverNeighbourLines(SearchResult result, string previousContent,
                                                                       string nextContent, bool requireOrdered = false,
                                                                       int contextWords = DefaultContextWords)
        {
            if (result == null || string.IsNullOrEmpty(result.Content)) return SnippetDescriptor.NoMatch;
            string joined = JoinWithNeighbourLines(previousContent, result.Content, nextContent);
            var snippet = result.Session.LocateBatch(new[] { joined })[0];
            return snippet == null ? SnippetDescriptor.NoMatch : new SnippetDescriptor(snippet, result.Session.TermAt);
        }

        /// <summary>
        /// <see cref="LocateSnippetWindowOverNeighbourLines"/> for many results at once: the
        /// joined texts go to the engine in one batch call per search session (located
        /// multi-core), instead of one native call per result. Same descriptors, in order.
        /// </summary>
        public SnippetDescriptor[] LocateSnippetWindowsOverNeighbourLines(IReadOnlyList<SearchResult> results,
                                                                          IReadOnlyList<string> previousContents,
                                                                          IReadOnlyList<string> nextContents)
        {
            var output = new SnippetDescriptor[results.Count];
            var bySession = new Dictionary<SearchSessionRef, List<int>>();
            for (int i = 0; i < results.Count; i++)
            {
                output[i] = SnippetDescriptor.NoMatch;
                var r = results[i];
                if (r == null || string.IsNullOrEmpty(r.Content)) continue;
                if (!bySession.TryGetValue(r.Session, out var list)) bySession[r.Session] = list = new List<int>();
                list.Add(i);
            }
            foreach (var kv in bySession)
            {
                var texts = new string[kv.Value.Count];
                for (int k = 0; k < texts.Length; k++)
                {
                    int i = kv.Value[k];
                    texts[k] = JoinWithNeighbourLines(previousContents[i], results[i].Content, nextContents[i]);
                }
                var located = kv.Key.LocateBatch(texts);
                for (int k = 0; k < texts.Length; k++)
                {
                    var snippet = located[k];
                    if (snippet != null) output[kv.Value[k]] = new SnippetDescriptor(snippet, kv.Key.TermAt);
                }
            }
            return output;
        }

        /// <summary>FtsLib's <c>SnippetDescriptorPipeline.JoinWithNeighbourLines</c>:
        /// previous + ' ' + content + ' ' + next, an empty side dropped together with its
        /// space. The space keeps boundary words distinct (they must not fuse across a
        /// line break) and renders as a normal gap. The page re-joins line texts the same
        /// way, so a descriptor's offsets land on the same characters.</summary>
        internal static string JoinWithNeighbourLines(string? previousContent, string content, string? nextContent)
        {
            bool hasPrevious = !string.IsNullOrEmpty(previousContent);
            bool hasNext = !string.IsNullOrEmpty(nextContent);
            if (!hasPrevious && !hasNext) return content;

            var joined = new System.Text.StringBuilder(
                (previousContent?.Length ?? 0) + content.Length + (nextContent?.Length ?? 0) + 2);
            if (hasPrevious) joined.Append(previousContent).Append(' ');
            joined.Append(content);
            if (hasNext) joined.Append(' ').Append(nextContent);
            return joined.ToString();
        }

        // ── Build ─────────────────────────────────────────────────────

        /// <summary>The build watermark, committed atomically with the documents.</summary>
        public sealed class BuildState
        {
            public int LastLineId;
            public long LinesDone;
            public long TotalLines;
            public bool Complete;
            public string Fingerprint = "";

            public string Serialize() => string.Join("|",
                "v1", LastLineId.ToString(CultureInfo.InvariantCulture), LinesDone.ToString(CultureInfo.InvariantCulture),
                TotalLines.ToString(CultureInfo.InvariantCulture), Complete ? "1" : "0", Fingerprint);

            public static BuildState? Parse(string? payload)
            {
                if (string.IsNullOrEmpty(payload)) return null;
                var parts = payload!.Split(new[] { '|' }, 6);
                if (parts.Length != 6 || parts[0] != "v1") return null;
                return new BuildState
                {
                    LastLineId = int.Parse(parts[1], CultureInfo.InvariantCulture),
                    LinesDone = long.Parse(parts[2], CultureInfo.InvariantCulture),
                    TotalLines = long.Parse(parts[3], CultureInfo.InvariantCulture),
                    Complete = parts[4] == "1",
                    Fingerprint = parts[5],
                };
            }
        }

        public BuildState? ReadBuildState()
        {
            if (!SeforimIndex.Exists(_indexPath)) return null;
            return BuildState.Parse(Engine().GetInfo().Payload);
        }

        /// <summary>Where an interrupted build stopped (FtsLib's GetResumeState).</summary>
        public void GetResumeState(out int lineId, out long totalLines, out long resumeOffset)
        {
            lineId = 0; totalLines = 0; resumeOffset = 0;
            var state = ReadBuildState();
            if (state == null || state.Complete) return;
            lineId = state.LastLineId;
            totalLines = state.TotalLines;
            resumeOffset = state.LinesDone;
        }

        /// <summary>
        /// Builds the index, or resumes an interrupted build of the SAME database, or
        /// returns at once when a complete index of this database exists. Lines go in
        /// catalog order; every <paramref name="linesPerCommit"/> lines are committed
        /// together with the watermark, so a kill loses at most one batch and never
        /// corrupts. A resume whose corpus no longer matches rebuilds from scratch.
        /// </summary>
        /// <param name="indexOptions">Applies to a from-scratch build only.</param>
        public void BuildLines(Action<long, long>? onProgress = null, CancellationToken ct = default,
                               IndexOptions? indexOptions = null, int linesPerCommit = 20000,
                               long heapBytes = 200_000_000, long maxLines = 0, Action<string, TimeSpan>? onStage = null)
        {
            _onStage = onStage;
            string fingerprint = SeforimSearch.Demo.SeforimDb.SeforimDbContentFingerprint.Compute(_seforimDbPath, IndexFormatVersion);
            var state = ReadBuildState();
            if (state != null && state.Fingerprint == fingerprint && state.Complete)
            {
                MergeSegments(heapBytes); // indexes built before merging existed
                return;
            }

            bool resume = state != null && state.Fingerprint == fingerprint && state.LinesDone > 0;
            if (!resume)
            {
                lock (_openLock) { _index?.Dispose(); _index = null; }
                Directory.CreateDirectory(_indexPath);
                using (SeforimIndex.Create(_indexPath, indexOptions)) { }
                using var c = _openCorpus();
                state = new BuildState { TotalLines = c.CountDocuments(), Fingerprint = fingerprint };
            }

            try
            {
                var indexing = System.Diagnostics.Stopwatch.StartNew();
                RunBuild(state!, resume, onProgress, ct, linesPerCommit, heapBytes, maxLines, indexing);
            }
            catch (IndexNoLongerMatchesCorpusException)
            {
                // FtsLib: wipe and build from scratch.
                lock (_openLock) { _index?.Dispose(); _index = null; }
                using (SeforimIndex.Create(_indexPath, indexOptions)) { }
                using var c = _openCorpus();
                var fresh = new BuildState { TotalLines = c.CountDocuments(), Fingerprint = fingerprint };
                RunBuild(fresh, false, onProgress, ct, linesPerCommit, heapBytes, maxLines,
                         System.Diagnostics.Stopwatch.StartNew());
            }
        }

        /// <summary>Build stage reporter (stage name, duration); null = silent.</summary>
        private Action<string, TimeSpan>? _onStage;

        private void Stage(string name, System.Diagnostics.Stopwatch sw)
        {
            _onStage?.Invoke(name, sw.Elapsed);
            sw.Restart();
        }

        private void RunBuild(BuildState state, bool resume, Action<long, long>? onProgress, CancellationToken ct,
                              int linesPerCommit, long heapBytes, long maxLines, System.Diagnostics.Stopwatch stage)
        {
            using var writer = Engine().OpenWriter(heapBytes);
            using var corpus = _openCorpus();

            // Resume re-reads the feed from LinesDone-1: its first line must be the last
            // committed one, or the corpus changed under the index.
            long skip = resume ? state.LinesDone - 1 : 0;
            bool verifyFirst = resume, sawFirst = false, capped = false;
            int sinceCommit = 0;

            foreach (var (id, text, bookId) in corpus.ReadDocumentsWithBookInResultOrder(skip, ct))
            {
                if (verifyFirst)
                {
                    verifyFirst = false;
                    sawFirst = true;
                    if (id != state.LastLineId)
                        throw new IndexNoLongerMatchesCorpusException($"position {state.LinesDone} was line {state.LastLineId}, now line {id}");
                    continue;
                }
                if (maxLines > 0 && state.LinesDone >= maxLines) { capped = true; break; }

                writer.Add(new LineDocument { LineId = id, Position = state.LinesDone, BookId = bookId, Text = text });
                state.LinesDone++;
                state.LastLineId = id;
                if (++sinceCommit >= linesPerCommit)
                {
                    writer.Commit(state.Serialize());
                    sinceCommit = 0;
                    onProgress?.Invoke(state.LinesDone, state.TotalLines);
                }
            }
            if (resume && !sawFirst)
                throw new IndexNoLongerMatchesCorpusException($"the corpus has fewer than {state.LinesDone} lines");

            state.Complete = !capped;
            writer.Commit(state.Serialize());
            onProgress?.Invoke(state.LinesDone, state.TotalLines);
            Stage("indexing (read DB + tokenize + write segments + commits)", stage);

            // One segment = one term dictionary per walk (FtsLib had one too). A
            // build commits every linesPerCommit lines, leaving dozens of segments,
            // and every expansion walk repeats per segment (measured: *יצח* 4.8 s
            // over 37 segments).
            if (state.Complete)
            {
                writer.MergeAndClose();
                Stage("merge into one segment", stage);
                Engine().BuildFmSidecar();
                Stage("FM-index sidecar", stage);
            }
        }

        /// <summary>Merges the index into one segment if it has several (a no-op
        /// otherwise) and builds the FM-index sidecar over it (FtsLib's
        /// <c>BuildFmSidecar</c>; skipped when it already matches). Call after
        /// updates when the index is mostly read.</summary>
        public void MergeSegments(long heapBytes = 200_000_000)
        {
            using (var writer = Engine().OpenWriter(heapBytes))
                writer.MergeAndClose();
            if (!Engine().HasFmSidecar) Engine().BuildFmSidecar();
        }

        public void Dispose()
        {
            lock (_openLock) { _index?.Dispose(); _index = null; }
        }
    }

    /// <summary>A resumed build found the corpus no longer matches the index.</summary>
    public sealed class IndexNoLongerMatchesCorpusException : Exception
    {
        public IndexNoLongerMatchesCorpusException(string message) : base(message) { }
    }
}
