using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
using FtsLib.Fts5;
using SeforimSearch.Demo.Common;
using SeforimSearch.Demo.SeforimDb;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// KitveiHakodesh Core's <c>SeforimDbFtsIndexCoordinator</c> over the tantivy engine: the same
    /// constructor and members the service calls, so swapping Core's class for this one is
    /// mechanical (see the README's "Swap into KitveiHakodesh"). It owns the index lifecycle
    /// (background build with progress, rebuild when the DB changed, reset, shutdown, idle
    /// release) and the searches (streamed, one-shot, in-book / commentary substring find).
    ///
    /// What differs inside, not in the shape: the build is <see cref="TantivySeforimIndex.BuildLines"/>
    /// (resumable, merged to one segment + FM sidecar at the end), a DB change goes through the
    /// per-book update (issue #29) before falling back to a full rebuild, and the substring find
    /// is the engine's substring query over position ranges (issue G-K1).
    /// </summary>
    public sealed class SeforimDbFtsIndexCoordinator
    {
        private readonly ISeforimDbFtsDataSource? _source;
        private readonly string _indexPath;
        private readonly bool _external;
        private readonly Action<FtsIndexLogLevel, string, Exception?> _log;
        private readonly SeforimDbFtsRelatedFormExpander? _expander;

        private readonly object _lock = new object();
        private SeforimDbFtsEngine? _engine;
        private Task? _buildTask;
        private CancellationTokenSource? _buildCts;
        private int _activeOneShotSearches;

        private volatile bool _isReady;
        private volatile bool _isIndexing;
        private volatile bool _buildFailed;
        private volatile bool _shuttingDown;
        private volatile bool _isMerging;
        private volatile int _mergePassesDone;
        private volatile int _mergePassesExpected;
        private double _pct;
        private volatile int _processed;
        private volatile int _total;
        private volatile IndexBuildReason? _lastBuildReason;

        /// <param name="dataSource">The seforim DB to index and search, through the host's own
        /// database layer (the facade holds no SQLite); null when none was found.</param>
        /// <param name="indexPath">Where the index lives.</param>
        /// <param name="indexIsExternal">True when the host was handed an index to use as-is:
        /// never built into, never wiped.</param>
        /// <param name="log">The host's log sink.</param>
        /// <param name="relatedFormExpander">Rewrites a query with related forms when a word asks
        /// for '+' or '='; null leaves queries unexpanded.</param>
        public SeforimDbFtsIndexCoordinator(
            ISeforimDbFtsDataSource? dataSource, string indexPath, bool indexIsExternal,
            Action<FtsIndexLogLevel, string, Exception?> log,
            SeforimDbFtsRelatedFormExpander? relatedFormExpander = null)
        {
            _source = dataSource;
            _indexPath = indexPath ?? throw new ArgumentNullException(nameof(indexPath));
            _external = indexIsExternal;
            _log = log ?? ((_, _, _) => { });
            _expander = relatedFormExpander;
        }

        /// <summary>Demo CLI: a coordinator over an engine whose index the <c>build</c> command
        /// made (used as-is, never built into or wiped).</summary>
        public SeforimDbFtsIndexCoordinator(SeforimDbFtsEngine? engine,
            Action<FtsIndexLogLevel, string, Exception?>? log = null)
        {
            _engine = engine;
            _indexPath = "";
            _external = true;
            _log = log ?? ((_, _, _) => { });
        }

        public SeforimDbFtsSearcher Searcher { get; } = new SeforimDbFtsSearcher();

        /// <summary>Demo only: Otzaria's per-word options for the streamed searches (the CLI's
        /// --prefixes etc.). Null = Core's behaviour (FtsLib's ketiv rules, quotes ignored).</summary>
        public SeforimSearch.Engine.WalkOptions? WordOptions { get; set; }

        public bool HasDb => _source == null ? _engine != null : _source.IsAvailable;

        /// <summary>True while background work runs (a build or update, or a live search): the
        /// idle memory trimmer must not run then.</summary>
        public bool IsBusy => _isIndexing || !_sessions.IsEmpty || Volatile.Read(ref _activeOneShotSearches) > 0;

        /// <summary>What the FTS side keeps resident right now (diagnostics).</summary>
        public string DescribeResidentState()
        {
            lock (_lock) return $"engine={(_engine is null ? "none" : "created")} sessions={_sessions.Count}";
        }

        /// <summary>Idle memory release: closes the engine (index handle, pooled seforim.db
        /// connections); the next search reopens it. Skipped while a build or a search runs.
        /// True when something was released.</summary>
        public bool ReleaseIdleResources()
        {
            lock (_lock)
            {
                if (_isIndexing || !_sessions.IsEmpty || _activeOneShotSearches > 0) return false;
                if (_engine == null || _source == null) return false;   // a handed-in engine is not ours
                _engine.Dispose();
                _engine = null;
                return true;
            }
        }

        private SeforimDbFtsEngine GetEngine()
        {
            lock (_lock)
            {
                if (_engine != null) return _engine;
                if (_source == null) throw new InvalidOperationException("no database");
                return _engine = new SeforimDbFtsEngine(_indexPath, _source);
            }
        }

        private void WarmBookPositionsInBackground()
        {
            var thread = new Thread(() =>
            {
                try { GetEngine().WarmBookPositions(); }
                catch (Exception ex) { _log(FtsIndexLogLevel.Debug, "FTS: book positions not warmed", ex); }
            })
            { IsBackground = true, Priority = ThreadPriority.BelowNormal, Name = "FTS book positions" };
            thread.Start();
        }

        /// <summary>
        /// Idempotent. Marks the index ready when a complete index of the current DB exists,
        /// otherwise starts (or resumes) a background build. Called at startup and lazily from
        /// search and status.
        /// </summary>
        public void EnsureIndexing()
        {
            if (!HasDb || _isReady || _shuttingDown) return;
            lock (_lock)
            {
                if (_isReady || _shuttingDown || _resetInFlight || _buildTask != null || _buildFailed) return;   // a reset in flight rebuilds itself
                if (_external)
                {
                    if (GetEngine().Index.IsBuilt) _isReady = true;
                    return;
                }
                var engine = GetEngine();
                string reason;
                try
                {
                    var state = engine.Index.ReadBuildState();
                    string fingerprint = _source!.ContentFingerprint(TantivySeforimIndex.IndexFormatVersion);
                    if (state != null && state.Complete && state.Fingerprint == fingerprint)
                    {
                        _isReady = true;
                        WarmBookPositionsInBackground();
                        return;
                    }
                    reason = state == null ? "no index yet"
                        : state.Fingerprint == fingerprint ? "resuming an interrupted build"
                        : "stale index: " + (_source!.DescribeFingerprintChange(state.Fingerprint, fingerprint) ?? "built from another database or format");
                }
                catch (Exception ex)
                {
                    _log(FtsIndexLogLevel.Error, "FTS: index state could not be read", ex);
                    return;
                }
                StartBackground(reason, RunBuild);
            }
        }

        /// <summary>Starts <paramref name="work"/> as THE background build (call under _lock).</summary>
        private void StartBackground(string reason, Action<SeforimDbFtsEngine, CancellationToken> work)
        {
            NoteBuildReason(_pendingBuildReason ?? reason);
            _pendingBuildReason = null;
            _isIndexing = true;
            _buildFailed = false;
            _pct = 0; _processed = 0; _total = 0;
            _isMerging = false; _mergePassesDone = 0; _mergePassesExpected = 0;
            var cts = _buildCts = new CancellationTokenSource();
            var engine = GetEngine();
            _buildTask = Task.Run(() =>
            {
                try { work(engine, cts.Token); }
                catch (OperationCanceledException) { _log(FtsIndexLogLevel.Information, "FTS build cancelled", null); }
                catch (Exception ex)
                {
                    _log(FtsIndexLogLevel.Error, "FTS index build failed", ex);
                    if (!_isReady) _buildFailed = true;   // terminal: nothing retries in this process
                }
                finally
                {
                    lock (_lock)
                    {
                        _isIndexing = false;
                        _isMerging = false;
                        if (ReferenceEquals(_buildCts, cts)) { _buildTask = null; _buildCts = null; }
                    }
                    cts.Dispose();
                    NotifyProgress();
                }
            });
        }

        private void RunBuild(SeforimDbFtsEngine engine, CancellationToken ct)
        {
            _log(FtsIndexLogLevel.Information, "FTS build starting, index=" + _indexPath, null);
            engine.Index.BuildLines(
                onProgress: (done, total) =>
                {
                    if (total <= 0) return;
                    _total = (int)Math.Min(total, int.MaxValue);
                    _processed = (int)Math.Min(done, int.MaxValue);
                    _pct = Math.Min(99.9, done * 100.0 / total);
                    if (!_isReady && done > 0) _isReady = true;   // committed segments are searchable
                    NotifyProgress();
                },
                ct: ct,
                onStage: (stage, _) =>
                {
                    // the final phase: merge into one segment, then the FM sidecar (2 passes)
                    if (stage.StartsWith("indexing", StringComparison.Ordinal)) { _isMerging = true; _mergePassesExpected = 2; }
                    else if (stage.StartsWith("merge", StringComparison.Ordinal)) _mergePassesDone = 1;
                    else if (stage.StartsWith("FM", StringComparison.Ordinal)) _mergePassesDone = 2;
                    NotifyProgress();
                });
            if (!engine.Index.IsBuilt) throw new InvalidOperationException("the build ended without a complete index");
            engine.InvalidateBookPositions();
            _pct = 100.0;
            _isReady = true;
            _log(FtsIndexLogLevel.Information, "FTS build complete, " + _indexPath, null);
            WarmBookPositionsInBackground();
        }

        /// <summary>
        /// Called by the DB change watcher: when the seforim DB's content no longer matches the
        /// index, updates the changed books in the background (the per-book update, issue #29),
        /// or wipes and rebuilds when that does not apply. True when work was started.
        /// </summary>
        public bool RebuildIfDbChanged()
        {
            if (_external || !HasDb || _shuttingDown) return false;
            lock (_lock)
            {
                if (_buildTask != null || !_isReady) return false;   // a build in flight reads the DB itself
                string? change;
                try
                {
                    var state = GetEngine().Index.ReadBuildState();
                    if (state == null || !state.Complete) return false;
                    string fingerprint = _source!.ContentFingerprint(TantivySeforimIndex.IndexFormatVersion);
                    if (state.Fingerprint == fingerprint) return false;
                    change = _source!.DescribeFingerprintChange(state.Fingerprint, fingerprint) ?? "changed";
                }
                catch { return false; }
                StartBackground("changed while running: " + change, (engine, ct) =>
                {
                    var report = engine.Index.UpdateChangedBooks(ct, line => _log(FtsIndexLogLevel.Information, "FTS update: " + line, null));
                    engine.InvalidateBookPositions();
                    _log(FtsIndexLogLevel.Information, "FTS " + report, null);
                    if (!report.Applicable) _ = Task.Run(() => ResetIndex("changed while running: " + report.Reason));
                });
                return true;
            }
        }

        private string? _pendingBuildReason;
        private bool _resetInFlight;

        /// <summary>Wipes the index and rebuilds it from scratch, in the background (progress is
        /// streamed). Ignored for an external index, during shutdown, or while a reset runs.</summary>
        public void ResetIndex(string reason = "reset requested")
        {
            if (_external || _shuttingDown) return;
            lock (_lock)
            {
                if (_resetInFlight)
                {
                    _log(FtsIndexLogLevel.Information, "FTS reset already in progress, ignoring the duplicate request", null);
                    return;
                }
                _resetInFlight = true;
                _pendingBuildReason = reason;
                // "indexing, 0%" from the moment the reset is asked (a stream opened now must
                // not close on a ready+idle snapshot)
                _isReady = false;
                _isIndexing = true;
                _buildFailed = false;
                _pct = 0; _processed = 0; _total = 0;
                _isMerging = false; _mergePassesDone = 0; _mergePassesExpected = 0;
            }
            NotifyProgress();
            _ = Task.Run(() =>
            {
                try { DoReset(); }
                catch (Exception ex) { _log(FtsIndexLogLevel.Error, "FTS index reset failed", ex); }
            });
        }

        private void DoReset()
        {
            try
            {
                Task? build;
                lock (_lock) { _buildCts?.Cancel(); build = _buildTask; }
                try { build?.Wait(); } catch (Exception ex) { _log(FtsIndexLogLevel.Debug, "FTS reset: prior build ended with an exception", ex); }
                foreach (var kv in _sessions)
                    try { kv.Value.Cts.Cancel(); } catch (ObjectDisposedException) { }
                for (int i = 0; i < 50 && !_sessions.IsEmpty; i++) Thread.Sleep(100);
                // One-shot searches (Search, LinesSearch) cannot be cancelled: wait for them to
                // finish before the engine they read is closed (new ones are refused while the
                // reset is in flight). Core disposes without waiting; this waits, logging once.
                bool logged = false;
                lock (_lock)
                {
                    while (_activeOneShotSearches > 0)
                    {
                        if (!logged && !Monitor.Wait(_lock, TimeSpan.FromSeconds(5)))
                        {
                            logged = true;
                            _log(FtsIndexLogLevel.Warning, "FTS reset: waiting for " + _activeOneShotSearches + " one-shot search(es) to finish", null);
                        }
                        else if (logged) Monitor.Wait(_lock, TimeSpan.FromSeconds(1));
                    }
                    _engine?.Dispose();
                    _engine = null;
                    _isReady = false;
                    _buildFailed = false;
                }
                try { if (Directory.Exists(_indexPath)) Directory.Delete(_indexPath, recursive: true); }
                catch (Exception ex) { _log(FtsIndexLogLevel.Error, "FTS reset: could not delete " + _indexPath, ex); }
            }
            finally
            {
                lock (_lock) { _resetInFlight = false; _isIndexing = false; }
                _log(FtsIndexLogLevel.Information, "FTS index reset, rebuilding from scratch", null);
                EnsureIndexing();
                NotifyProgress();
            }
        }

        /// <summary>Graceful shutdown: no new build from here on; the build in flight is
        /// cancelled and awaited (its last commit keeps it resumable), live searches cancelled.</summary>
        public void Shutdown()
        {
            Task? build;
            lock (_lock)
            {
                _shuttingDown = true;
                _resetInFlight = false;
                _buildCts?.Cancel();
                build = _buildTask;
            }
            foreach (var kv in _sessions)
                try { kv.Value.Cts.Cancel(); } catch (ObjectDisposedException) { }
            try { build?.Wait(TimeSpan.FromSeconds(25)); } catch { /* cancelled */ }
            _log(FtsIndexLogLevel.Information, "FTS build stopped for graceful shutdown", null);
        }

        public FtsIndexStatus Status()
        {
            EnsureIndexing();
            return Snapshot();
        }

        /// <summary>The one-shot search (ftsSearch). Ready = false while still building (or no
        /// DB). Never capped unless <paramref name="cap"/> &gt; 0. Words marked '+' or '=' get
        /// their related forms first.</summary>
        public FtsSearchResult Search(
            string query, int cap, int maxWordDistance, bool requireOrdered, int contextWords)
        {
            EnsureIndexing();
            if (SeforimDbFtsRelatedFormExpander.HasRelatedMarker(query)) query = RewriteRelated(query);
            var result = new FtsSearchResult();
            if (!HasDb || !_isReady) { result.Ready = false; return result; }
            result.Ready = true;
            if (string.IsNullOrWhiteSpace(query)) return result;

            // a reset in flight is about to close the engine: answer "not ready" instead
            lock (_lock)
            {
                if (_resetInFlight) { result.Ready = false; return result; }
                _activeOneShotSearches++;
            }
            try
            {
                var terms = new FtsTermTable();
                var unreadable = new SeforimDbFtsUnreadableRows();
                foreach (var batch in Searcher.SearchInBatches(
                             GetEngine(), query, maxWordDistance, requireOrdered, contextWords, WordOptions, terms,
                             unreadable: unreadable))
                {
                    result.Results.AddRange(batch.Hits);
                    result.SnippetTexts.AddRange(batch.SnippetTexts);
                    if (cap > 0 && result.Results.Count >= cap) break;
                }
                if (cap > 0 && result.Results.Count > cap)
                    result.Results.RemoveRange(cap, result.Results.Count - cap);
                result.Terms = terms.All();
                result.UnreadableRowCount = unreadable.Count;
                result.UnreadableLineIds = unreadable.LineIds();
                result.UnreadableError = unreadable.FirstError;
                if (unreadable.Count > 0)
                    _log(FtsIndexLogLevel.Warning, "FTS search: " + unreadable.Count + " rows unreadable (" + unreadable.FirstError + ")", null);
            }
            catch (Exception ex)
            {
                _log(FtsIndexLogLevel.Error, "FTS search failed", ex);
                result.Error = ex.Message;
            }
            finally
            {
                lock (_lock) { _activeOneShotSearches--; Monitor.PulseAll(_lock); }
            }
            return result;
        }

        /// <summary>
        /// Scoped substring candidates (the book view's in-book find: one range, the book; the
        /// commentary panel's: one range per commentator group): the lines inside
        /// <paramref name="ranges"/> that may hold <paramref name="query"/> as a substring.
        /// Not answered while the index is building or cannot answer exactly (issue G-K1): the
        /// caller then reads the lines itself.
        /// </summary>
        public FtsLinesSearchResult LinesSearch(string query, IReadOnlyList<FtsLineRange> ranges)
        {
            EnsureIndexing();
            var result = new FtsLinesSearchResult();
            if (!HasDb || !_isReady || string.IsNullOrWhiteSpace(query)) return result;
            lock (_lock)
            {
                if (_resetInFlight) return result;
                _activeOneShotSearches++;
            }
            try
            {
                var lineRanges = new List<(int, int, int)>(ranges.Count);
                foreach (var range in ranges) lineRanges.Add((range.BookId, range.FirstLineIndex, range.LastLineIndex));
                var candidates = GetEngine().SubstringCandidates(query, lineRanges);
                if (candidates == null) return result;
                result.Answered = true;
                result.BookIds = new int[candidates.Count];
                result.LineIndexes = new int[candidates.Count];
                result.LineIds = new long[candidates.Count];
                for (int i = 0; i < candidates.Count; i++)
                    (result.BookIds[i], result.LineIndexes[i], result.LineIds[i]) = candidates[i];
            }
            catch (Exception ex)
            {
                _log(FtsIndexLogLevel.Error, "FTS scoped search failed", ex);
            }
            finally
            {
                lock (_lock) { _activeOneShotSearches--; Monitor.PulseAll(_lock); }
            }
            return result;
        }

        /// <summary>
        /// Core's SnippetTexts (ftsSnippetTexts): the snippet texts for the rows a page shows,
        /// each line's own text, and for the lines the hit flagged short also their neighbour
        /// context, in one statement. Works while the index is still building.
        /// </summary>
        public List<FtsSnippetText> SnippetTexts(IReadOnlyList<long> lineIds, IReadOnlyList<long> shortLineIds)
        {
            if (!HasDb || (lineIds.Count == 0 && shortLineIds.Count == 0)) return new List<FtsSnippetText>();
            try
            {
                using var corpus = GetEngine().OpenCorpus();
                return corpus.FetchSnippetTexts(lineIds, shortLineIds);
            }
            catch (Exception ex)
            {
                _log(FtsIndexLogLevel.Error, "FTS snippet texts failed", ex);
                return new List<FtsSnippetText>();
            }
        }

        private string RewriteRelated(string query) =>
            _expander is null ? query : _expander.RewriteQuery(query, SeforimDbFtsRelatedFormExpander.PerTermLimit);

        //  Streaming search (ftsSearchStream)
        // Frames are pushed through emit until the search finishes. Backpressure: each batch
        // awaits emit before the next one is built. Results are never capped.

        /// <summary>An in-flight streaming search, tracked only for cancellation.</summary>
        private sealed class SearchSession
        {
            public readonly CancellationTokenSource Cts = new CancellationTokenSource();
        }

        private readonly ConcurrentDictionary<string, SearchSession> _sessions = new ConcurrentDictionary<string, SearchSession>();
        private int _searchCounter;
        private string? _currentSearchId;   // most-recent search; the next one supersedes it

        /// <summary>Run a search and push each built batch through <paramref name="emit"/>,
        /// ending with a <c>Done</c> frame.</summary>
        /// <param name="resumeAfterLineId">Resume: stream only the hits after this line, the
        /// last one the caller already holds from an earlier, unfinished run of the same search.
        /// 0 = from the start. A line that cannot be placed any more restarts the search
        /// (<see cref="FtsStreamChunk.Restarted"/> on the first frame).</param>
        public async Task StreamSearch(
            string query, int maxWordDistance, bool requireOrdered, int contextWords,
            Func<FtsStreamChunk, Task> emit, CancellationToken clientCt, long resumeAfterLineId = 0)
        {
            EnsureIndexing();
            if (SeforimDbFtsRelatedFormExpander.HasRelatedMarker(query))
            {
                try { await emit(new FtsStreamChunk { Stage = "rewrite" }); } catch { /* client gone */ }
                query = RewriteRelated(query);
            }
            if (!HasDb || !_isReady) { await emit(new FtsStreamChunk { Ready = false, Done = true }); return; }
            if (string.IsNullOrWhiteSpace(query)) { await emit(new FtsStreamChunk { Done = true }); return; }

            var session = new SearchSession();
            string id = "s" + Interlocked.Increment(ref _searchCounter);
            try
            {
                _sessions[id] = session;

                // A new search SUPERSEDES the previous in-flight one (latest search wins).
                var prevId = Interlocked.Exchange(ref _currentSearchId, id);
                if (prevId != null && _sessions.TryRemove(prevId, out var prev))
                {
                    // Cancel, do not dispose: the superseded search disposes its own CTS.
                    try { prev.Cts.Cancel(); } catch (ObjectDisposedException) { }
                }

                using var linked = CancellationTokenSource.CreateLinkedTokenSource(clientCt, session.Cts.Token);
                try
                {
                    long afterPosition = 0;
                    bool restarted = false;
                    if (resumeAfterLineId > 0)
                    {
                        long? position = GetEngine().PositionOfLine(resumeAfterLineId);
                        if (position is long found) afterPosition = found;
                        else restarted = true;
                    }
                    await emit(new FtsStreamChunk { Stage = "validate", Restarted = restarted });
                    // Stage changes ride alone only until the first results frame; after it the
                    // latest stage is folded into the next results frame.
                    bool resultsSent = false;
                    string? stageSinceLastResults = null;
                    var unreadable = new SeforimDbFtsUnreadableRows();
                    var terms = new FtsTermTable();
                    foreach (var batch in Searcher.SearchInBatches(
                                 GetEngine(), query, maxWordDistance, requireOrdered, contextWords, WordOptions, terms, linked.Token,
                                 stage =>
                                 {
                                     if (resultsSent) stageSinceLastResults = stage;
                                     else emit(new FtsStreamChunk { Stage = stage }).GetAwaiter().GetResult();
                                 }, afterPosition, unreadable))
                    {
                        resultsSent = true;
                        string? stage = stageSinceLastResults;
                        stageSinceLastResults = null;
                        await emit(new FtsStreamChunk
                        {
                            Results = batch.Hits, NewTerms = terms.TakeNewTerms(), SnippetTexts = batch.SnippetTexts,
                            Stage = stage, UnreadableLineIds = batch.UnreadableLineIds,
                        });
                    }
                    if (unreadable.Count > 0)
                        _log(FtsIndexLogLevel.Warning, "FTS search: " + unreadable.Count + " rows unreadable (" + unreadable.FirstError + ")", null);
                    await emit(new FtsStreamChunk { Done = true, UnreadableRowCount = unreadable.Count, UnreadableError = unreadable.FirstError });
                }
                catch (OperationCanceledException) { /* superseded, reset, or client left */ }
                catch (IOException) { /* client disconnected mid-stream: that IS the cancel signal */ }
                catch (Exception ex)
                {
                    _log(FtsIndexLogLevel.Error, "FTS streaming search failed", ex);
                    try { await emit(new FtsStreamChunk { Done = true, Error = ex.Message }); } catch { /* client gone */ }
                }
            }
            finally
            {
                _sessions.TryRemove(id, out _);
                Interlocked.CompareExchange(ref _currentSearchId, null, id);
                session.Cts.Dispose();
            }
        }

        //  Indexing-progress stream (ftsIndexProgressStream)

        private readonly List<SemaphoreSlim> _progressSubs = new List<SemaphoreSlim>();

        private void NotifyProgress()
        {
            lock (_progressSubs)
                foreach (var signal in _progressSubs)
                    if (signal.CurrentCount == 0) signal.Release();
        }

        /// <summary>Why the index last started building, and when; null until a build starts.</summary>
        public IndexBuildReason? LastBuildReason => _lastBuildReason;

        private void NoteBuildReason(string reason)
        {
            _lastBuildReason = IndexBuildReason.Now(reason);
            _log(FtsIndexLogLevel.Information, "FTS index building: " + reason, null);
        }

        private FtsIndexStatus Snapshot() => new FtsIndexStatus
        {
            BuildReason = _lastBuildReason,
            IsReady = _isReady,
            IsIndexing = _isIndexing,
            Percentage = Math.Round(_pct, 1),
            ProcessedChunks = _processed,
            TotalChunks = _total,
            DbMissing = !HasDb,
            Failed = _buildFailed,
            IsMerging = _isMerging,
            MergePassesDone = _mergePassesDone,
            MergePassesExpected = _mergePassesExpected,
        };

        /// <summary>Pushes the current status at once, then a fresh snapshot on every progress
        /// signal, until a terminal state (ready and idle, no DB, or failed).</summary>
        public async Task StreamIndexingProgress(Func<FtsIndexStatus, Task> emit, CancellationToken ct)
        {
            EnsureIndexing();
            var signal = new SemaphoreSlim(0, 1);
            lock (_progressSubs) _progressSubs.Add(signal);
            try
            {
                var last = Snapshot();
                await emit(last);
                while (!((last.IsReady && !last.IsIndexing) || last.DbMissing || last.Failed))
                {
                    await signal.WaitAsync(ct);
                    last = Snapshot();
                    await emit(last);
                }
            }
            catch (OperationCanceledException) { /* client left */ }
            catch (IOException) { /* client disconnected mid-stream */ }
            catch (Exception ex)
            {
                _log(FtsIndexLogLevel.Debug, "FTS progress stream ended on an unexpected error", ex);
            }
            finally
            {
                lock (_progressSubs) _progressSubs.Remove(signal);
                signal.Dispose();
            }
        }
    }

    /// <summary>How serious a coordinator message is (Core's FtsIndexLogLevel).</summary>
    public enum FtsIndexLogLevel
    {
        Debug,
        Information,
        Warning,
        Error,
    }
}
