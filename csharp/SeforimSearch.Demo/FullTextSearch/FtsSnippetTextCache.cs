using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The page side of KitveiHakodesh's snippet model (vue-frontend fullTextSearchSnippets.ts
    /// <c>ensureSnippets</c> / <c>seedSnippetTexts</c>, FullTextSearchResultsList.vue's lookahead):
    /// snippet texts by line id, seeded from the texts that rode on the stream
    /// (<see cref="FtsStreamChunk.SnippetTexts"/>, the first
    /// <see cref="SeforimDbFtsSearcher.SeedSnippetTextHitCount"/> hits), the rest fetched for the
    /// rows a page shows (<see cref="SeforimDbFtsIndexCoordinator.SnippetTexts"/>), at most
    /// <see cref="FetchBatch"/> ids per request, a short row's neighbour context in the same
    /// request. LRU (<see cref="TextCap"/>); a line already in flight is awaited, never asked twice.
    /// </summary>
    public sealed class FtsSnippetTextCache
    {
        /// <summary>Texts kept (the page's TEXT_CAP).</summary>
        public const int TextCap = 1500;
        /// <summary>Ids per request (the page's FETCH_BATCH).</summary>
        public const int FetchBatch = 200;
        /// <summary>Rows fetched ahead of / behind the rows shown (the results list's lookahead).</summary>
        public const int LookAhead = 60, LookBehind = 20;

        private readonly Func<IReadOnlyList<long>, IReadOnlyList<long>, Task<List<FtsSnippetText>>> _fetch;
        private readonly int _cap;
        private readonly object _lock = new object();
        // LRU: the list holds line ids, most recent last
        private readonly Dictionary<long, (FtsSnippetText? Text, bool WithContext, LinkedListNode<long> Node)> _texts =
            new Dictionary<long, (FtsSnippetText?, bool, LinkedListNode<long>)>();
        private readonly LinkedList<long> _order = new LinkedList<long>();
        private readonly Dictionary<long, Task> _inFlight = new Dictionary<long, Task>();
        private int _requests;

        public FtsSnippetTextCache(Func<IReadOnlyList<long>, IReadOnlyList<long>, Task<List<FtsSnippetText>>> fetch,
                                   int capacity = TextCap)
        {
            _fetch = fetch ?? throw new ArgumentNullException(nameof(fetch));
            _cap = capacity;
        }

        /// <summary>Requests sent so far (each at most <see cref="FetchBatch"/> ids).</summary>
        public int Requests { get { lock (_lock) return _requests; } }

        /// <summary>Seeds texts that rode on a stream frame. A text with neighbours was read as
        /// short; one without may be either (a short hit then asks once more, as the page).</summary>
        public void Seed(IEnumerable<FtsSnippetText>? texts)
        {
            if (texts == null) return;
            lock (_lock)
                foreach (var t in texts)
                    Put(t.LineId, t, t.Before.Length > 0 || t.After.Length > 0);
        }

        /// <summary>The cached text a hit renders from, or null when it must be fetched (a line
        /// cached plain is a miss for a short hit). A line with no text row reads empty.</summary>
        public FtsSnippetText? TextOf(long lineId, bool isShort)
        {
            lock (_lock)
            {
                if (!_texts.TryGetValue(lineId, out var e)) return null;
                _order.Remove(e.Node);
                _order.AddLast(e.Node);
                if (isShort && !e.WithContext) return null;
                return e.Text ?? new FtsSnippetText { LineId = lineId };
            }
        }

        private bool Has(long lineId, bool isShort) =>
            _texts.TryGetValue(lineId, out var e) && (!isShort || e.WithContext);

        /// <summary>Completes once every hit's text is cached: the missing ones fetched in
        /// requests of at most <see cref="FetchBatch"/> ids, the ones in flight awaited.</summary>
        public async Task Ensure(IEnumerable<FtsHit> hits)
        {
            var list = hits as IReadOnlyList<FtsHit> ?? hits.ToList();
            await EnsureOnce(list).ConfigureAwait(false);
            // a short row that waited on a PLAIN read of its line (in flight, or the same line
            // also asked plain) still lacks its context: ask once more for those
            List<FtsHit> again;
            lock (_lock) again = list.Where(h => IsShort(h) && !Has(h.LineId, true) && _texts.ContainsKey(h.LineId)).ToList();
            if (again.Count > 0) await EnsureOnce(again).ConfigureAwait(false);
        }

        private Task EnsureOnce(IReadOnlyList<FtsHit> hits)
        {
            var waits = new HashSet<Task>();
            var missing = new List<FtsHit>();
            var seen = new HashSet<long>();
            var requests = new List<(List<long>, List<long>, TaskCompletionSource<bool>)>();
            lock (_lock)
            {
                foreach (var hit in hits)
                {
                    bool isShort = IsShort(hit);
                    if (Has(hit.LineId, isShort)) continue;
                    if (!seen.Add(hit.LineId))
                    {
                        // the same line as plain and short: ask it short (the text covers both)
                        if (isShort) { int k = missing.FindIndex(m => m.LineId == hit.LineId); if (k >= 0) missing[k] = hit; }
                        continue;
                    }
                    if (_inFlight.TryGetValue(hit.LineId, out var pending)) { waits.Add(pending); continue; }
                    missing.Add(hit);
                }
                for (int i = 0; i < missing.Count; i += FetchBatch)
                {
                    var part = missing.Skip(i).Take(FetchBatch).ToList();
                    var plain = part.Where(h => !IsShort(h)).Select(h => h.LineId).ToList();
                    var shorts = part.Where(IsShort).Select(h => h.LineId).ToList();
                    _requests++;
                    // registered as in flight BEFORE the fetch starts (it may finish synchronously)
                    var done = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
                    foreach (var h in part) _inFlight[h.LineId] = done.Task;
                    waits.Add(done.Task);
                    requests.Add((plain, shorts, done));
                }
            }
            foreach (var (plain, shorts, done) in requests) _ = FetchInto(plain, shorts, done);
            return Task.WhenAll(waits);
        }

        /// <summary><see cref="Ensure"/> for the rows shown, <paramref name="first"/> ..
        /// first + <paramref name="count"/> of <paramref name="rows"/> (every hit known so far, in
        /// display order), plus <see cref="LookAhead"/> rows ahead and <see cref="LookBehind"/>
        /// behind, in the same requests (the shown rows first).</summary>
        public Task EnsureAround(IReadOnlyList<FtsHit> rows, int first, int count)
        {
            int shownEnd = Math.Min(rows.Count, first + count);
            int from = Math.Max(0, first - LookBehind), to = Math.Min(rows.Count, first + count + LookAhead);
            var wanted = new List<FtsHit>(to - from);
            for (int i = Math.Min(first, rows.Count); i < shownEnd; i++) wanted.Add(rows[i]);
            for (int i = from; i < to; i++) wanted.Add(rows[i]);
            return Ensure(wanted);
        }

        private async Task FetchInto(List<long> plain, List<long> shorts, TaskCompletionSource<bool> done)
        {
            try
            {
                var rows = await _fetch(plain, shorts).ConfigureAwait(false);
                var byId = new Dictionary<long, FtsSnippetText>();
                foreach (var r in rows) byId[r.LineId] = r;
                lock (_lock)
                {
                    // an id the service did not return has no line: kept as empty, not asked again.
                    // An empty answer (the coordinator logs and swallows a failed read, as Core) is
                    // not cached: those rows are asked again next time.
                    if (rows.Count > 0)
                    {
                        foreach (long id in plain) Put(id, byId.TryGetValue(id, out var t) ? t : null, false);
                        foreach (long id in shorts) Put(id, byId.TryGetValue(id, out var t) ? t : null, true);
                    }
                    foreach (long id in plain) _inFlight.Remove(id);
                    foreach (long id in shorts) _inFlight.Remove(id);
                }
                done.SetResult(true);
            }
            catch (Exception ex)
            {
                lock (_lock)
                {
                    foreach (long id in plain) _inFlight.Remove(id);
                    foreach (long id in shorts) _inFlight.Remove(id);
                }
                done.SetException(ex);
            }
        }

        private static bool IsShort(FtsHit hit) => (hit.SnippetFlags & FtsHit.SnippetFlagShort) != 0;

        private void Put(long lineId, FtsSnippetText? text, bool withContext)
        {
            if (_texts.TryGetValue(lineId, out var old))
            {
                _order.Remove(old.Node);
                // a plain read never downgrades a text cached with context
                if (old.WithContext && !withContext) { text = old.Text; withContext = true; }
            }
            var node = _order.AddLast(lineId);
            _texts[lineId] = (text, withContext, node);
            while (_texts.Count > _cap)
            {
                long oldest = _order.First!.Value;
                _order.RemoveFirst();
                _texts.Remove(oldest);
            }
        }
    }
}
