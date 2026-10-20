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
                var located = _session.LocateBatch(texts);
                _terms.AddRange(_session.TakeNewTerms());
                return located;
            }
        }

        public Snippet? LocateWithNeighbours(string previous, string text, string next, Snippet matched)
        {
            lock (_lock)
            {
                var located = _session.LocateWithNeighbours(previous, text, next, matched);
                _terms.AddRange(_session.TakeNewTerms());
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

        public int ContextWords { get; set; } = TantivySeforimIndex.DefaultContextWords;
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
        public const string IndexFormatVersion = "tantivy2"; // 2: trailing quotes dropped from terms

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
            var request = new SearchRequest
            {
                MaxWordDistance = options.MaxWordDistance,
                RequireOrdered = options.RequireOrdered,
                ContextWords = options.ContextWords,
            };
            request.Groups.AddRange(Search_.QueryParser.Parse(query, options.WordOptions));
            if (request.Groups.Count == 0) yield break;

            onStage?.Invoke("search");
            // NOT disposed with the enumeration: results outlive it (Core snippets the
            // last batch after the loop ends), as FtsLib's results carried their own
            // prepared state. The native session is freed when the last result holding
            // it is collected (SafeHandle).
            var session = new SearchSessionRef(Engine().Search(request));
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
                for (int i = 0; i < texts.Length; i++) kv.Value[i].LocatedSnippet = located[i];
            }
        }

        /// <summary>The result's snippet window (located in its batch, or now).
        /// <c>IsMatch</c> false when the line does not pass the filters.</summary>
        public SnippetDescriptor LocateSnippetWindow(SearchResult result, bool requireOrdered = false,
                                                     int contextWords = DefaultContextWords)
        {
            if (result == null) return SnippetDescriptor.NoMatch;
            var snippet = result.LocatedSnippet ?? result.Session.LocateBatch(new[] { result.Content })[0];
            result.LocatedSnippet = snippet;
            return snippet == null ? SnippetDescriptor.NoMatch : new SnippetDescriptor(snippet, result.Session.TermAt);
        }

        /// <summary>Re-locates a short window over the line joined with its neighbours
        /// (previous + ' ' + line + ' ' + next). Offsets are into that joined text.</summary>
        public SnippetDescriptor LocateSnippetWindowOverNeighbourLines(SearchResult result, string previousContent,
                                                                       string nextContent, bool requireOrdered = false,
                                                                       int contextWords = DefaultContextWords)
        {
            if (result?.LocatedSnippet == null) return SnippetDescriptor.NoMatch;
            var extended = result.Session.LocateWithNeighbours(previousContent ?? "", result.Content,
                                                               nextContent ?? "", result.LocatedSnippet);
            return extended == null ? SnippetDescriptor.NoMatch : new SnippetDescriptor(extended, result.Session.TermAt);
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
                               long heapBytes = 200_000_000, long maxLines = 0)
        {
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
                RunBuild(state!, resume, onProgress, ct, linesPerCommit, heapBytes, maxLines);
            }
            catch (IndexNoLongerMatchesCorpusException)
            {
                // FtsLib: wipe and build from scratch.
                lock (_openLock) { _index?.Dispose(); _index = null; }
                using (SeforimIndex.Create(_indexPath, indexOptions)) { }
                using var c = _openCorpus();
                var fresh = new BuildState { TotalLines = c.CountDocuments(), Fingerprint = fingerprint };
                RunBuild(fresh, false, onProgress, ct, linesPerCommit, heapBytes, maxLines);
            }
        }

        private void RunBuild(BuildState state, bool resume, Action<long, long>? onProgress, CancellationToken ct,
                              int linesPerCommit, long heapBytes, long maxLines)
        {
            using var writer = Engine().OpenWriter(heapBytes);
            using var corpus = _openCorpus();

            // Resume re-reads the feed from LinesDone-1: its first line must be the last
            // committed one, or the corpus changed under the index.
            long skip = resume ? state.LinesDone - 1 : 0;
            bool verifyFirst = resume, sawFirst = false, capped = false;
            int sinceCommit = 0;

            foreach (var (id, text) in corpus.ReadDocumentsInResultOrder(skip, ct))
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

                writer.Add(new LineDocument { LineId = id, Position = state.LinesDone, Text = text });
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

            // One segment = one term dictionary per walk (FtsLib had one too). A
            // build commits every linesPerCommit lines, leaving dozens of segments,
            // and every expansion walk repeats per segment (measured: *יצח* 4.8 s
            // over 37 segments).
            if (state.Complete) writer.MergeAndClose();
        }

        /// <summary>Merges the index into one segment if it has several (a no-op
        /// otherwise). Call after updates when the index is mostly read.</summary>
        public void MergeSegments(long heapBytes = 200_000_000)
        {
            using var writer = Engine().OpenWriter(heapBytes);
            writer.MergeAndClose();
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

namespace FtsLib.Search_
{
    // alias so TantivySeforimIndex can reach the parser without a using cycle
    internal static class QueryParser
    {
        public static List<SeforimSearch.Engine.QueryGroup> Parse(string q, SeforimSearch.Engine.WalkOptions o)
            => FtsLib.Search.QueryParser.Parse(q, o);
    }
}
