using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// Non-blocking search (issue #16): the sync calls run on the thread pool,
    /// so a UI thread never blocks on the engine. The <see cref="CancellationToken"/>
    /// is wired to the native cancel token: cancelling stops a running search at
    /// its next check and every later call of its session
    /// (<see cref="OperationCanceledException"/>). A session is not thread-safe:
    /// await each call before the next one on the same session.
    /// </summary>
    public static class SeforimAsyncExtensions
    {
        /// <summary><see cref="SeforimIndex.Search(SearchOptions, CancellationToken)"/> on the thread pool.</summary>
        public static Task<SearchSession> SearchAsync(this SeforimIndex index, SearchOptions options,
                                                      CancellationToken ct = default)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (options == null) throw new ArgumentNullException(nameof(options));
            return Task.Run(() => index.Search(options, ct), ct);
        }

        /// <summary>
        /// Runs a search and streams every hit in pages of <paramref name="pageSize"/>,
        /// each page fetched on the thread pool when the consumer asks for it. The
        /// session is disposed when the enumeration ends: completed, faulted,
        /// cancelled or abandoned (<c>break</c> / <c>DisposeAsync</c>).
        /// </summary>
        public static async IAsyncEnumerable<PlacedHit[]> SearchBatchesAsync(this SeforimIndex index,
            SearchOptions options, int pageSize = 4096, [EnumeratorCancellation] CancellationToken ct = default)
        {
            if (pageSize <= 0) throw new ArgumentOutOfRangeException(nameof(pageSize));
            using var session = await index.SearchAsync(options, ct).ConfigureAwait(false);
            await foreach (var page in session.HitBatchesAsync(pageSize, ct).ConfigureAwait(false))
                yield return page;
        }

        /// <summary>Every hit of the session, paged, each page fetched on the thread pool.</summary>
        public static async IAsyncEnumerable<PlacedHit[]> HitBatchesAsync(this SearchSession session,
            int pageSize = 4096, [EnumeratorCancellation] CancellationToken ct = default)
        {
            if (session == null) throw new ArgumentNullException(nameof(session));
            if (pageSize <= 0) throw new ArgumentOutOfRangeException(nameof(pageSize));
            for (long o = 0; o < session.HitCount; o += pageSize)
            {
                var page = await session.GetHitsAsync(o, pageSize, ct).ConfigureAwait(false);
                if (page.Length == 0) yield break;
                yield return page;
            }
        }

        /// <summary><see cref="SearchSession.GetHits"/> on the thread pool.</summary>
        public static Task<PlacedHit[]> GetHitsAsync(this SearchSession session, long offset, int count,
                                                     CancellationToken ct = default) =>
            Run(() => session.GetHits(offset, count), ct);

        /// <summary><see cref="SearchSession.LocateBatch"/> on the thread pool.</summary>
        public static Task<Snippet?[]> LocateBatchAsync(this SearchSession session, IReadOnlyList<string> texts,
                                                        CancellationToken ct = default) =>
            Run(() => session.LocateBatch(texts), ct);

        /// <summary><see cref="SearchSession.LocateLines"/> on the thread pool.</summary>
        public static Task<Snippet?[]> LocateLinesAsync(this SearchSession session, IReadOnlyList<HitLines> hits,
                                                        CancellationToken ct = default) =>
            Run(() => session.LocateLines(hits), ct);

        /// <summary><see cref="SearchSession.LocatePairs"/> on the thread pool.</summary>
        public static Task<Snippet?[]> LocatePairsAsync(this SearchSession session, IReadOnlyList<string> texts,
                                                        IReadOnlyList<string?> nextTexts, CancellationToken ct = default) =>
            Run(() => session.LocatePairs(texts, nextTexts), ct);

        /// <summary><see cref="SearchSession.CountBatch"/> on the thread pool.</summary>
        public static Task<long> CountBatchAsync(this SearchSession session, IReadOnlyList<string> texts,
                                                 CancellationToken ct = default) =>
            Run(() => session.CountBatch(texts), ct);

        /// <summary><see cref="SearchSession.CountBatchPairs"/> on the thread pool.</summary>
        public static Task<long> CountBatchPairsAsync(this SearchSession session, IReadOnlyList<string> texts,
                                                      IReadOnlyList<string?> nextTexts, CancellationToken ct = default) =>
            Run(() => session.CountBatchPairs(texts, nextTexts), ct);

        /// <summary><see cref="SearchSession.GetCounts"/> on the thread pool.</summary>
        public static Task<HitCounts> GetCountsAsync(this SearchSession session, CancellationToken ct = default) =>
            Run(session.GetCounts, ct);

        private static Task<T> Run<T>(Func<T> call, CancellationToken ct)
        {
            if (ct.IsCancellationRequested) return Task.FromCanceled<T>(ct);
            return Task.Run(call, ct);
        }
    }
}
