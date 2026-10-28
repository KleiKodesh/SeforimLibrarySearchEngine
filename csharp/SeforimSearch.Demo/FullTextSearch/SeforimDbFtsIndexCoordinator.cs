using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Threading;
using System.Threading.Tasks;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The streaming-search part of KitveiHakodesh Core's <c>SeforimDbFtsIndexCoordinator</c>
    /// (ftsSearchStream), ported step for step: <see cref="StreamSearch"/> with the same
    /// signature, frames (<see cref="FtsStreamChunk"/>), supersession, resume and Restarted.
    /// The index build / reset / progress parts of Core's coordinator are not here: the demo
    /// builds with its <c>build</c> command, so the coordinator is handed an engine and is
    /// ready when that engine's index is built.
    /// </summary>
    public sealed class SeforimDbFtsIndexCoordinator
    {
        private readonly SeforimDbFtsEngine? _engine;
        private readonly Action<FtsIndexLogLevel, string, Exception?> _log;

        public SeforimDbFtsIndexCoordinator(SeforimDbFtsEngine? engine,
            Action<FtsIndexLogLevel, string, Exception?>? log = null)
        {
            _engine = engine;
            _log = log ?? ((_, _, _) => { });
        }

        public SeforimDbFtsSearcher Searcher { get; } = new SeforimDbFtsSearcher();

        /// <summary>Demo only: Otzaria's per-word options for the streamed searches (the CLI's
        /// --prefixes etc.). Null = Core's behaviour (FtsLib's ketiv rules, quotes ignored).
        /// Core's searcher has no such parameter, so this is a property, never a signature change.</summary>
        public SeforimSearch.Engine.WalkOptions? WordOptions { get; set; }

        public bool HasDb => _engine != null;
        private bool _isReady => _engine != null && _engine.Index.IsBuilt;

        private SeforimDbFtsEngine GetEngine() => _engine ?? throw new InvalidOperationException("no database");

        // ── Streaming search (ftsSearchStream) ───────────────────────────────────────
        // Frames are pushed through emit until the search finishes. Backpressure: each batch
        // awaits emit before the next one is built. Results are never capped.

        /// <summary>An in-flight streaming search, tracked only for cancellation (supersession).</summary>
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
            Func<FtsStreamChunk, Task> emit, CancellationToken clientCt, int resumeAfterLineId = 0)
        {
            // Core: EnsureIndexing() and the related-forms rewrite ("rewrite" stage) come here.
            // The demo has no background build and no related-form expander (see README).
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
                    await RunSearchCoreAsync(query, maxWordDistance, requireOrdered, contextWords, afterPosition,
                        linked.Token,
                        (built, newTerms, snippetTexts) =>
                        {
                            resultsSent = true;
                            string? stage = stageSinceLastResults;
                            stageSinceLastResults = null;
                            return emit(new FtsStreamChunk { Results = built, NewTerms = newTerms, SnippetTexts = snippetTexts, Stage = stage });
                        },
                        stage =>
                        {
                            if (resultsSent) stageSinceLastResults = stage;
                            else emit(new FtsStreamChunk { Stage = stage }).GetAwaiter().GetResult();
                        });
                    await emit(new FtsStreamChunk { Done = true });
                }
                catch (OperationCanceledException) { /* superseded, or client left */ }
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

        /// <summary>Core's RunSearchCoreAsync: each finished batch goes to
        /// <paramref name="onBatch"/> with the term strings it introduced. A fresh
        /// <see cref="FtsTermTable"/> per search, so a resumed stream numbers its terms from 0
        /// again (the receiver maps them; Core does the same).</summary>
        private async Task RunSearchCoreAsync(
            string query, int maxWordDistance, bool requireOrdered, int contextWords, long afterPosition,
            CancellationToken ct, Func<List<FtsHit>, List<string>, List<FtsSnippetText>, Task> onBatch,
            Action<string>? onStage = null)
        {
            var terms = new FtsTermTable();
            foreach (var batch in Searcher.SearchInBatches(
                         GetEngine(), query, maxWordDistance, requireOrdered, contextWords, WordOptions, terms, ct, onStage, afterPosition))
            {
                await onBatch(batch.Hits, terms.TakeNewTerms(), batch.SnippetTexts);
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
