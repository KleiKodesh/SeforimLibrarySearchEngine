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
    /// engine's snippet locate, split into batch locates and single-text locates (one
    /// text or hit at a time), with call counts. Diagnostic only.</summary>
    public static class HostTiming
    {
        public static readonly bool Enabled =
            !string.IsNullOrEmpty(Environment.GetEnvironmentVariable("SEFORIM_HOST_TIMING"));
        public static long BatchTicks, BatchCalls, BatchTexts, SingleTicks, SingleCalls;

        public static void Reset() { BatchTicks = BatchCalls = BatchTexts = SingleTicks = SingleCalls = 0; }

        public static string Report() =>
            $"[host] locate batches: {BatchCalls} calls, {BatchTexts} texts, {BatchTicks * 1000.0 / System.Diagnostics.Stopwatch.Frequency:F0} ms; " +
            $"single locates: {SingleCalls} calls, {SingleTicks * 1000.0 / System.Diagnostics.Stopwatch.Frequency:F0} ms";
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

        /// <summary><see cref="SearchSession.LocateLines"/> (hits given as their lines).</summary>
        public Snippet?[] LocateLines(IReadOnlyList<HitLines> hits)
        {
            lock (_lock)
            {
                long t0 = HostTiming.Enabled ? System.Diagnostics.Stopwatch.GetTimestamp() : 0;
                var located = _session.LocateLines(hits);
                _terms.AddRange(_session.TakeNewTerms());
                if (HostTiming.Enabled)
                {
                    long dt = System.Diagnostics.Stopwatch.GetTimestamp() - t0;
                    if (hits.Count == 1) { HostTiming.SingleTicks += dt; HostTiming.SingleCalls++; }
                    else { HostTiming.BatchTicks += dt; HostTiming.BatchCalls++; HostTiming.BatchTexts += hits.Count; }
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
        /// KtivRules defaults to FtsLib's KetivExpander rules here (FtsLib's expandKetiv).
        public WalkOptions WordOptions { get; set; } = new WalkOptions { IgnoreQuotes = true, KtivRules = KtivRules.FtsLib };

        /// <summary>מרחק בין מילים. Null = no limit.</summary>
        public int? MaxWordDistance { get; set; }

        public bool RequireOrdered { get; set; }

        /// <summary>Otzaria's user regex (/.../). Off in FtsLib mode: FtsLib had none.</summary>
        public bool UserRegex { get; set; }

        public int ContextWords { get; set; } = TantivySeforimIndex.DefaultContextWords;

        /// <summary>Result order (Otzaria's). Catalog, as FtsLib, by default.</summary>
        public ResultsOrder Order { get; set; }

        /// <summary>Expansion cap per query word, one cap across all segments (null: 2000,
        /// FtsLib's; 0: uncapped). Whether it was hit: SearchInfo.Truncated.</summary>
        public int? MaxExpandedTerms { get; set; }
    }

    /// <summary>
    /// The tantivy engine behind the full-text search, shaped like FtsLib's
    /// Fts5SeforimIndex: Search streams SearchResults in catalog order with their
    /// content fetched in the caller's batch plan; LocateSnippetWindow returns
    /// SnippetDescriptors; BuildLines
    /// builds or resumes the index from the corpus.
    /// </summary>
    public sealed partial class TantivySeforimIndex : IDisposable
    {
        /// <summary>The file whose presence means a searchable index.</summary>
        public const string IndexFileName = "meta.json";

        public const int DefaultContextWords = 8;

        /// <summary>Bumped when the indexing pipeline changes; part of the provenance stamp.</summary>
        public const string IndexFormatVersion = "tantivy16"; // 16: 64-bit notes ids (R = 2^52), issue #13; 15: a line flag for a word too long to be a term (a line regex always reads the line), issue #15; 14: footnotes (line_note) indexed as virtual notes books, issue #13; 13: catalog order (= position) counts a base text by the isBaseBook flag only (no title-equals-folder rule; issue #6); 12: a <sup> holding a vocalized Hebrew letter mid-word is transparent (the word continues through it); a keep_marks term drops U+034F (combining grapheme joiner); 11: doc order = catalog order (index sorted by position; search skips its sort on the merged segment); 10: an unterminated '<' is a plain separator (the rest of the line is indexed, as Otzaria); per-line content edges (line_first / line_last, edge-enumerator flags) replace the word count, so cross-line matching skips enumerators like (x) {x} (49); 9: per-line word count, line flags (headings: no cross-line phrase) and section id (TOC entry); 2: trailing quotes dropped from terms; 3: bare-minimum schema (no line_index/toc_entry_id); 5: one-letter (and 30+ letter) words indexed with positions; 6: FtsLib word model = Otzaria's (digit runs are words, ב5 one word); 7: Otzaria's remaining tokenizer rules (. [ ] bidi chars join, no script split, geresh kept as ', trailing geresh, quote and presentation-form folding, paired readings share a position, a term of 128 bytes or more not indexed); 8: a keep_marks index keeps marks in canonical order, flags the lines carrying a mark, measures the 128-byte limit without marks

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
                                                Func<int, int>? batchSizeAt = null, bool withPlacement = true,
                                                long afterPosition = 0)
        {
            if (!SeforimIndex.Exists(_indexPath)) yield break;
            options ??= new TantivySearchOptions();
            if (string.IsNullOrWhiteSpace(query)) yield break;
            // The engine parses the query (FtsLib's QueryParser rules) and builds
            // one walk per word with these options.
            var request = WithBaseBooks(ToRequest(query, options));
            // Resume (FtsLib's afterPosition: rowid > position, 0 = from the start). FtsLib's
            // positions start at 1 and the engine's at 0 (same order), so FtsLib's position p
            // is the engine's p - 1; the engine bounds the hits while collecting.
            if (afterPosition > 0) request.AfterPosition = afterPosition - 1;

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
                long first = offset;
                offset += hits.Length;
                if (hits.Length == 0) break;

                if (firstFetch) { onStage?.Invoke("fetch"); firstFetch = false; }
                var ids = new long[hits.Length];
                for (int i = 0; i < hits.Length; i++) ids[i] = hits[i].LineId;
                // withPlacement: false = the text only (all the locate reads); the placement
                // carries just the book id the index already returned, and the caller reads
                // the rest for the hits it keeps (FtsLib's Search(withPlacement) contract).
                var byId = new Dictionary<long, (string Text, FtsDocumentPlacement Placement)>(hits.Length);
                if (withPlacement)
                    foreach (var (id, text, placement) in corpus.FetchPlacedDocuments(ids))
                        byId[id] = (text, placement);
                else
                {
                    var bookOf = new Dictionary<long, long>(hits.Length);
                    foreach (var h in hits) bookOf[h.LineId] = h.BookId;
                    foreach (var (id, text, _) in corpus.FetchDocuments(ids))
                        byId[id] = (text, new FtsDocumentPlacement(bookOf[id], 0));
                }

                for (int i = 0; i < ids.Length; i++)
                {
                    long id = ids[i];
                    if (!byId.TryGetValue(id, out var row)) continue; // line gone from the DB
                    yield return new SearchResult(id, row.Text, row.Placement, session, first + i);
                }
            }
        }

        /// <summary>The relevance order's base books (seforim.db <c>book.isBaseBook</c>), read
        /// once per facade; the other orders need none.</summary>
        private HashSet<long>? _baseBooks;

        private SearchOptions WithBaseBooks(SearchOptions request)
        {
            if (request.Order != ResultsOrder.Relevance) return request;
            if (_baseBooks == null)
            {
                using var corpus = _openCorpus();
                _baseBooks = corpus.BaseBookIds();
            }
            request.BaseBooks.UnionWith(_baseBooks);
            return request;
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
                KtivRules = w.KtivRules,
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
                MaxExpandedTerms = options.MaxExpandedTerms,
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
            => CountByBook(query, options, ct, batchSize, out info, null);

        /// <summary><see cref="CountByBook(string, TantivySearchOptions?, CancellationToken, int, out SearchInfo)"/>
        /// with the search's rule for unreadable rows (issue #13): an undecodable line or note
        /// counts as empty text and is recorded in <paramref name="unreadable"/> (its count is the
        /// caller's to surface), never aborting the count. Null = a private scope (lenient, not surfaced).</summary>
        public HitCounts CountByBook(string query, TantivySearchOptions? options, CancellationToken ct,
                                     int batchSize, out SearchInfo info, SeforimDbFtsUnreadableRows? unreadable)
        {
            using var scope = (unreadable ?? new SeforimDbFtsUnreadableRows()).Enter();
            options ??= new TantivySearchOptions();
            using var session = Engine().Search(WithBaseBooks(ToRequest(query, options)), ct);
            var line = session.GetLineRegexStatus();
            if (line != null)
            {
                RunLineRegex(session, line, ct, batchSize, null, unreadable);
                info = session.GetInfo();
                return session.GetCounts();
            }
            info = session.GetInfo();
            if (!session.CountNeedsText) return session.GetCounts();
            using var corpus = _openCorpus();
            for (long offset = 0; offset < session.HitCount;)
            {
                ct.ThrowIfCancellationRequested();
                PlacedHit[] hits = session.GetHits(offset, batchSize);
                if (hits.Length == 0) break;
                offset += hits.Length;
                var ids = new long[hits.Length];
                for (int i = 0; i < hits.Length; i++) ids[i] = hits[i].LineId;
                var byId = new Dictionary<long, string>(hits.Length);
                foreach (var (id, text, _) in corpus.FetchDocuments(ids)) byId[id] = text;
                var texts = new string[hits.Length];
                for (int i = 0; i < ids.Length; i++) texts[i] = byId.TryGetValue(ids[i], out var t) ? t : string.Empty;
                session.CountBatch(texts);
            }
            return session.GetCounts();
        }

        /// <summary>
        /// Runs a user regex of Otzaria's Line kind to the end (or its 100,000-line cap):
        /// the candidates' texts are fetched by line id and fed in hit order; a full scan
        /// streams every line of the DB in catalog order (book by book, as Otzaria reads the
        /// books) with its position and book id. <paramref name="onMatch"/> gets each
        /// matching line with its UTF-16 match ranges.
        /// </summary>
        public void RunLineRegex(SearchSession session, LineRegexStatus status, CancellationToken ct, int batchSize,
                                 Action<long, string, (int Start, int End)[]>? onMatch)
            => RunLineRegex(session, status, ct, batchSize, onMatch, null);

        /// <summary><see cref="RunLineRegex(SearchSession, LineRegexStatus, CancellationToken, int, Action{long, string, ValueTuple{int, int}[]}?)"/>
        /// with the search's rule for unreadable rows (issue #13): an undecodable row (candidate or
        /// full scan) reads as empty and is recorded in <paramref name="unreadable"/>; null = a private
        /// scope (lenient, not surfaced). The scope covers the calling thread for the whole run.</summary>
        public void RunLineRegex(SearchSession session, LineRegexStatus status, CancellationToken ct, int batchSize,
                                 Action<long, string, (int Start, int End)[]>? onMatch, SeforimDbFtsUnreadableRows? unreadable)
        {
            using var scope = (unreadable ?? new SeforimDbFtsUnreadableRows()).Enter();
            using var corpus = _openCorpus();
            if (!status.FullScan)
            {
                for (long offset = 0; offset < session.HitCount;)
                {
                    ct.ThrowIfCancellationRequested();
                    PlacedHit[] hits = session.GetHits(offset, batchSize);
                    if (hits.Length == 0) break;
                    offset += hits.Length;
                    var ids = new long[hits.Length];
                    for (int i = 0; i < hits.Length; i++) ids[i] = hits[i].LineId;
                    var byId = new Dictionary<long, string>(hits.Length);
                    foreach (var (id, text, _) in corpus.FetchDocuments(ids)) byId[id] = text;
                    var texts = new string[hits.Length];
                    for (int i = 0; i < ids.Length; i++) texts[i] = byId.TryGetValue(ids[i], out var t) ? t : string.Empty;
                    var found = session.LineMatchBatch(texts);
                    for (int i = 0; i < found.Length; i++)
                        if (found[i].Length > 0) onMatch?.Invoke(hits[i].LineId, texts[i], found[i]);
                    if (session.GetLineRegexStatus()!.Done) break;
                }
                return;
            }
            var lines = new List<PlacedHit>(batchSize);
            var batchTexts = new List<string>(batchSize);
            long position = 0;
            bool Flush()
            {
                var found = session.LineScanBatch(lines, batchTexts);
                for (int i = 0; i < found.Length; i++)
                    if (found[i].Length > 0) onMatch?.Invoke(lines[i].LineId, batchTexts[i], found[i]);
                lines.Clear();
                batchTexts.Clear();
                return session.GetLineRegexStatus()!.Done;
            }
            foreach (var (id, text, bookId) in corpus.ReadDocumentsWithBookInResultOrder(0, ct))
            {
                lines.Add(new PlacedHit(position++, id, bookId));
                batchTexts.Add(text);
                if (lines.Count >= batchSize && Flush()) return;
            }
            if (lines.Count > 0) Flush();
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
                // as their lines: on a positions index the engine reads the matched words
                // from the postings (same snippets as LocateBatch on the texts)
                var hits = new HitLines[kv.Value.Count];
                for (int i = 0; i < hits.Length; i++)
                {
                    var r = kv.Value[i];
                    hits[i] = new HitLines(r.HitIndex, new[] { ((long)r.LineId, r.Content) });
                }
                var located = kv.Key.LocateLines(hits);
                for (int i = 0; i < hits.Length; i++)
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

        // ── Build ─────────────────────────────────────────────────────

        /// <summary>The build watermark, committed atomically with the documents. A complete
        /// index also records each book's <c>contentHash</c> and line counts (<see cref="Books"/>)
        /// and the books a per-book update has not finished (<see cref="Pending"/>), issue #29.</summary>
        public sealed class BuildState
        {
            public long LastLineId;
            public long LinesDone;
            public long TotalLines;
            public bool Complete;
            public string Fingerprint = "";
            /// <summary>"bookId:contentHash:lines:notes;..." (null = not recorded: an index built
            /// before issue #29, adopted at the next update when its fingerprint still matches).</summary>
            public string? Books;
            /// <summary>"bookId,bookId,..." of a per-book update in progress (null or "" = none).</summary>
            public string? Pending;

            // v2 puts the new fields before the fingerprint, which may itself hold '|'
            public string Serialize() => Books == null && string.IsNullOrEmpty(Pending)
                ? string.Join("|", "v1", LastLineId.ToString(CultureInfo.InvariantCulture), LinesDone.ToString(CultureInfo.InvariantCulture),
                    TotalLines.ToString(CultureInfo.InvariantCulture), Complete ? "1" : "0", Fingerprint)
                : string.Join("|", "v2", LastLineId.ToString(CultureInfo.InvariantCulture), LinesDone.ToString(CultureInfo.InvariantCulture),
                    TotalLines.ToString(CultureInfo.InvariantCulture), Complete ? "1" : "0", Books ?? "-", Pending ?? "", Fingerprint);

            public static BuildState? Parse(string? payload)
            {
                if (string.IsNullOrEmpty(payload)) return null;
                if (payload!.StartsWith("v2|", StringComparison.Ordinal))
                {
                    var p2 = payload.Split(new[] { '|' }, 8);
                    if (p2.Length != 8) return null;
                    return new BuildState
                    {
                        LastLineId = long.Parse(p2[1], CultureInfo.InvariantCulture),
                        LinesDone = long.Parse(p2[2], CultureInfo.InvariantCulture),
                        TotalLines = long.Parse(p2[3], CultureInfo.InvariantCulture),
                        Complete = p2[4] == "1",
                        Books = p2[5] == "-" ? null : p2[5],
                        Pending = p2[6],
                        Fingerprint = p2[7],
                    };
                }
                var parts = payload.Split(new[] { '|' }, 6);
                if (parts.Length != 6 || parts[0] != "v1") return null;
                return new BuildState
                {
                    LastLineId = long.Parse(parts[1], CultureInfo.InvariantCulture),
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
        public void GetResumeState(out long lineId, out long totalLines, out long resumeOffset)
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
                               long heapBytes = 0, long maxLines = 0, Action<string, TimeSpan>? onStage = null)
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
            // heapBytes 0 = the engine's auto mode (issue #19): threads per core in the OS
            // background mode, memory budget following the machine's load
            using var writer = heapBytes > 0 ? Engine().OpenWriter(heapBytes) : Engine().OpenWriter();
            // this feeding thread (corpus reads) runs in the OS background mode too; the
            // engine leaves it for commits, the final merge and the FM sidecar by itself
            using var background = BackgroundPriority.Enter();
            using var corpus = _openCorpus();

            // Resume re-reads the feed from LinesDone-1: its first line must be the last
            // committed one, or the corpus changed under the index.
            long skip = resume ? state.LinesDone - 1 : 0;
            bool verifyFirst = resume, sawFirst = false, capped = false;
            int sinceCommit = 0;

            foreach (var (id, text, bookId, sectionId) in corpus.ReadDocumentsWithSectionInResultOrder(skip, ct))
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

                writer.Add(ToLineDocument(id, state.LinesDone, bookId, text, sectionId));
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
                BuildFmSidecarReported(stage);
            }
        }

        /// <summary>The document the build writes for one corpus line at catalog
        /// <paramref name="position"/>.</summary>
        private static LineDocument ToLineDocument(long id, long position, long bookId, string text, long? sectionId) =>
            new LineDocument
            {
                // Otzaria's rule: a heading interrupts the text, no phrase continues into or out of it.
                // A footnote (virtual notes-book line, issue #13) is a text of its own: never joined.
                LineId = id, Position = position, BookId = bookId, Text = text,
                SectionId = sectionId, NoCross = text.StartsWith("<h", StringComparison.Ordinal) || global::SeforimSearch.Demo.SeforimDb.SeforimDbNoteIds.IsVirtual(id),
            };

        /// <summary>Reads the documents of the lines at catalog positions
        /// [<paramref name="first"/>, first + <paramref name="count"/>) from seforim.db: the same
        /// documents the build writes. A host updating changed lines deletes each line id and adds
        /// its document again (<see cref="SeforimIndexWriter.Delete"/> + Add); the demo's
        /// <c>load</c> benchmark (issue #31) does so while searches run.</summary>
        public List<LineDocument> ReadLineDocuments(long first, int count, CancellationToken ct = default)
        {
            var docs = new List<LineDocument>(count);
            if (count <= 0) return docs;
            using var corpus = _openCorpus();
            long position = first;
            foreach (var (id, text, bookId, sectionId) in corpus.ReadDocumentsWithSectionInResultOrder(first, ct))
            {
                docs.Add(ToLineDocument(id, position++, bookId, text, sectionId));
                if (docs.Count >= count) break;
            }
            return docs;
        }

        /// <summary>Opens a writer on this index (fixed resources, see
        /// <see cref="SeforimIndex.OpenWriter(long, int, bool)"/>). One writer per index directory:
        /// a second one throws the engine's lock error. Commit it with
        /// <see cref="CommitKeepingBuildState"/> so the index stays built.</summary>
        public SeforimIndexWriter OpenWriter(long heapBytes, int threads) => Engine().OpenWriter(heapBytes, threads);

        /// <summary>Commits <paramref name="writer"/> with the index's current build state as
        /// the payload (a commit without it would leave the index "not built").</summary>
        public void CommitKeepingBuildState(SeforimIndexWriter writer) => writer.Commit(ReadBuildState()?.Serialize());

        /// <summary>Builds the FM sidecar and reports the outcome as a build stage line:
        /// built (with its time), not built (several segments or no terms), or FAILED
        /// with the engine's reason (then rethrown: never a silent skip; the index itself
        /// is complete and searches use the walk until the sidecar is rebuilt).</summary>
        private void BuildFmSidecarReported(System.Diagnostics.Stopwatch stage)
        {
            bool built;
            try { built = Engine().BuildFmSidecar(); }
            catch (Exception e)
            {
                Stage("FM-index sidecar FAILED: " + e.Message, stage);
                throw;
            }
            Stage(built ? "FM-index sidecar" : "FM-index sidecar not built (several segments or no terms)", stage);
        }

        /// <summary>Merges the index into one segment if it has several (a no-op
        /// otherwise) and builds the FM-index sidecar over it (FtsLib's
        /// <c>BuildFmSidecar</c>; skipped when it already matches). Call after
        /// updates when the index is mostly read.</summary>
        public void MergeSegments(long heapBytes = 0)
        {
            using (var writer = heapBytes > 0 ? Engine().OpenWriter(heapBytes) : Engine().OpenWriter())
                writer.MergeAndClose();
            if (!Engine().HasFmSidecar) BuildFmSidecarReported(System.Diagnostics.Stopwatch.StartNew());
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
