using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>
    /// One search: every matching line (catalog order) and the snippet state —
    /// the groups' matched words (prepared once), the word-distance / order
    /// filters, and the search's term table. Not thread-safe: use one session
    /// per thread, or lock.
    /// </summary>
    public sealed unsafe class SearchSession : IDisposable
    {
        private readonly SearchHandle _handle;

        private readonly CancelTokenHandle? _cancel;
        private CancellationTokenRegistration _registration;

        internal SearchSession(SearchHandle handle, int contextWords)
            : this(handle, contextWords, null, default) { }

        internal SearchSession(SearchHandle handle, int contextWords, CancelTokenHandle? cancel,
                               CancellationTokenRegistration registration)
        {
            _cancel = cancel;
            _registration = registration;
            _handle = handle;
            ContextWords = contextWords;
            HitCount = (long)NativeMethods.se_search_hit_count(_handle);
        }

        /// <summary>Words of context per side this search locates with.</summary>
        public int ContextWords { get; }

        /// <summary>Number of matching lines (before the snippet filters).</summary>
        public long HitCount { get; }

        /// <summary>Hits [<paramref name="offset"/>, offset+count), in the search's order (SearchOptions.Order; catalog by default).</summary>
        public PlacedHit[] GetHits(long offset, int count)
        {
            if (count <= 0 || offset >= HitCount) return Array.Empty<PlacedHit>();
            count = (int)Math.Min(count, HitCount - offset);
            var raw = new ulong[count * 3];
            var next = new ulong[count];
            ulong copied;
            fixed (ulong* r = raw)
                copied = NativeMethods.se_search_copy_hits(_handle, (ulong)offset, (ulong)count, r);
            fixed (ulong* n = next)
                NativeMethods.se_search_copy_next_lines(_handle, (ulong)offset, (ulong)count, n);
            var hits = new PlacedHit[(int)copied];
            for (int i = 0; i < hits.Length; i++)
            {
                int b = i * 3;
                long? nextLine = next[i] == ulong.MaxValue ? null : (long?)next[i];
                hits[i] = new PlacedHit((long)raw[b], (long)raw[b + 1], (long)raw[b + 2], nextLine);
            }
            return hits;
        }

        /// <summary>Every hit, in the search's order.</summary>
        public IEnumerable<PlacedHit> AllHits(int pageSize = 4096)
        {
            for (long offset = 0; offset < HitCount; offset += pageSize)
                foreach (var hit in GetHits(offset, pageSize))
                    yield return hit;
        }

        /// <summary>
        /// Locates the snippets of a batch of matched lines' texts. Null entries
        /// are lines that do not pass (not all groups, wrong order, or too far
        /// apart). Offsets are into each text.
        /// </summary>
        public Snippet?[] LocateBatch(IReadOnlyList<string> texts)
        {
            int n = texts.Count;
            var results = new Snippet?[n];
            if (n == 0) return results;

            uint[] data = WithTexts(texts, (p, l) =>
                Interop.TakeBuffer(NativeMethods.se_search_locate_batch(_handle, p, l, (UIntPtr)n)));
            int at = 0;
            for (int i = 0; i < n; i++)
            {
                if (data[at++] == 0) continue;
                results[i] = Snippet.Read(data, ref at);
            }
            return results;
        }

        /// <summary>
        /// Locates a batch of hits given as their LINES: each hit is its matched line and,
        /// for a short window, its neighbour lines (document order), located over the
        /// non-empty texts joined by single spaces (offsets into that joined text, as
        /// FtsLib's JoinWithNeighbourLines). <see cref="HitLines.HitIndex"/> is the hit's
        /// index in this search's hits (-1 when unknown). On an index with positions the
        /// engine reads the matched words from the postings instead of searching every
        /// word of the text (faster, same snippets); otherwise it is <see cref="LocateBatch"/>
        /// on the joined texts. Cross-line pairs go through <see cref="LocatePairs"/>.
        /// </summary>
        public Snippet?[] LocateLines(IReadOnlyList<HitLines> hits)
        {
            int n = hits.Count;
            var results = new Snippet?[n];
            if (n == 0) return results;
            var hitIndexes = new long[n];
            var counts = new UIntPtr[n];
            var texts = new List<string>();
            var ids = new List<ulong>();
            for (int h = 0; h < n; h++)
            {
                hitIndexes[h] = hits[h].HitIndex;
                var lines = hits[h].Lines;
                counts[h] = (UIntPtr)lines.Count;
                foreach (var line in lines)
                {
                    ids.Add((ulong)line.LineId);
                    texts.Add(line.Text ?? string.Empty);
                }
            }
            var idArray = ids.ToArray();
            uint[] data;
            fixed (long* hp = hitIndexes)
            fixed (UIntPtr* cp = counts)
            fixed (ulong* ip = idArray)
            {
                long* hpp = hp;
                UIntPtr* cpp = cp;
                ulong* ipp = ip;
                data = WithTexts(texts, (p, l) =>
                    Interop.TakeBuffer(NativeMethods.se_search_locate_lines(_handle, hpp, cpp, (UIntPtr)n, ipp, p, l)));
            }
            int at = 0;
            for (int i = 0; i < n; i++)
            {
                if (data[at++] == 0) continue;
                results[i] = Snippet.Read(data, ref at);
            }
            return results;
        }

        /// <summary>
        /// <see cref="LocateBatch"/> for hits given with their next line's text (cross-line
        /// matching: null for a hit without <see cref="PlacedHit.NextLineId"/>). The line alone
        /// is tried first; a cross-line snippet has <see cref="Snippet.BreakAt"/> and offsets
        /// into <c>text + "\n" + next</c>.
        /// </summary>
        public Snippet?[] LocatePairs(IReadOnlyList<string> texts, IReadOnlyList<string?> nextTexts)
        {
            int n = texts.Count;
            var results = new Snippet?[n];
            if (n == 0) return results;
            uint[] data = WithPairs(texts, nextTexts, (p, l, np, nl) =>
                Interop.TakeBuffer(NativeMethods.se_search_locate_pairs(_handle, p, l, np, nl, (UIntPtr)n)));
            int at = 0;
            for (int i = 0; i < n; i++)
            {
                uint kind = data[at++];
                if (kind == 0) continue;
                int? brk = kind == 2 ? (int?)data[at++] : null;
                var s = Snippet.Read(data, ref at);
                s.BreakAt = brk;
                results[i] = s;
            }
            return results;
        }

        /// <summary><see cref="CountBatch"/> for hits given with their next line's text.</summary>
        public long CountBatchPairs(IReadOnlyList<string> texts, IReadOnlyList<string?> nextTexts)
        {
            long passed = WithPairs(texts, nextTexts, (p, l, np, nl) =>
                NativeMethods.se_search_count_batch_pairs(_handle, p, l, np, nl, (UIntPtr)texts.Count));
            if (passed < 0) throw Interop.Fail(Interop.LastError());
            return passed;
        }

        private delegate T PairsCall<T>(IntPtr* texts, UIntPtr* lens, IntPtr* nextTexts, UIntPtr* nextLens);

        private static T WithPairs<T>(IReadOnlyList<string> texts, IReadOnlyList<string?> nextTexts, PairsCall<T> call)
        {
            if (nextTexts.Count != texts.Count) throw new ArgumentException("one next text (or null) per text");
            int n = nextTexts.Count;
            var pins = new GCHandle[n];
            var ptrs = new IntPtr[n];
            var lens = new UIntPtr[n];
            try
            {
                for (int i = 0; i < n; i++)
                {
                    if (nextTexts[i] == null) continue;
                    byte[] bytes = Interop.ToUtf8(nextTexts[i]!);
                    if (bytes.Length == 0) bytes = new byte[1] { 0 };
                    pins[i] = GCHandle.Alloc(bytes, GCHandleType.Pinned);
                    ptrs[i] = pins[i].AddrOfPinnedObject();
                    lens[i] = (UIntPtr)(nextTexts[i]!.Length == 0 ? 0 : bytes.Length);
                }
                fixed (IntPtr* np = ptrs)
                fixed (UIntPtr* nl = lens)
                {
                    IntPtr* npp = np;
                    UIntPtr* nll = nl;
                    return WithTexts(texts, (p, l) => call(p, l, npp, nll));
                }
            }
            finally
            {
                foreach (var pin in pins)
                    if (pin.IsAllocated) pin.Free();
            }
        }

        private delegate T TextsCall<T>(IntPtr* texts, UIntPtr* lens);

        /// <summary>Pins the texts as UTF-8 and passes them as pointer + length arrays.</summary>
        private static T WithTexts<T>(IReadOnlyList<string> texts, TextsCall<T> call)
        {
            int n = texts.Count;
            var pins = new GCHandle[n];
            var ptrs = new IntPtr[n];
            var lens = new UIntPtr[n];
            try
            {
                for (int i = 0; i < n; i++)
                {
                    byte[] bytes = Interop.ToUtf8(texts[i]);
                    pins[i] = GCHandle.Alloc(bytes, GCHandleType.Pinned);
                    ptrs[i] = pins[i].AddrOfPinnedObject();
                    lens[i] = (UIntPtr)bytes.Length;
                }
                fixed (IntPtr* p = ptrs)
                fixed (UIntPtr* l = lens)
                    return call(p, l);
            }
            finally
            {
                foreach (var pin in pins)
                    if (pin.IsAllocated) pin.Free();
            }
        }

        /// <summary>
        /// True when the exact count needs the hits' texts (FtsLib's distance /
        /// order, proximity on an index without positions, the text-level
        /// negative): feed them with <see cref="CountBatch"/>. False: every hit
        /// passes and <see cref="GetCounts"/> is exact at once, no text read.
        /// </summary>
        public bool CountNeedsText
        {
            get
            {
                int r = NativeMethods.se_search_count_needs_text(_handle);
                if (r < 0) throw Interop.Fail(Interop.LastError());
                return r != 0;
            }
        }

        /// <summary>
        /// Counts the next <c>texts.Count</c> hits (hit order, continuing
        /// from the previous call) from their texts, across all cores: the
        /// lines that pass every filter are added to the exact totals. Returns
        /// how many passed.
        /// </summary>
        public long CountBatch(IReadOnlyList<string> texts)
        {
            long passed = WithTexts(texts, (p, l) =>
                NativeMethods.se_search_count_batch(_handle, p, l, (UIntPtr)texts.Count));
            if (passed < 0) throw Interop.Fail(Interop.LastError());
            return passed;
        }

        /// <summary>The exact totals so far: all lines and per book id
        /// (Otzaria's count / countByBook). <see cref="HitCounts.Exact"/> once
        /// every hit is counted.</summary>
        public HitCounts GetCounts()
        {
            var o = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_search_counts(_handle)))!;
            var books = new Dictionary<long, long>();
            foreach (var e in (List<object?>)o["books"]!)
            {
                var pair = (List<object?>)e!;
                books[(long)(double)pair[0]!] = (long)(double)pair[1]!;
            }
            return new HitCounts((long)(double)o["total"]!, books, (long)(double)o["validated"]!, (bool)o["exact"]!);
        }

        /// <summary>How the search was served: its <see cref="SearchInfo.Notices"/>
        /// (not errors; e.g. <see cref="SearchInfo.RelevanceNeedsPositions"/>) and
        /// whether a walk hit its cap (<see cref="SearchInfo.Truncated"/>).</summary>
        public SearchInfo GetInfo()
        {
            var o = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_search_info(_handle)))!;
            var notices = new List<string>();
            foreach (var n in (List<object?>)o["notices"]!) notices.Add((string)n!);
            return new SearchInfo(notices, (bool)o["truncated"]!);
        }

        // -- User regex, Otzaria's Line kind --

        /// <summary>The state of a user regex of Otzaria's Line kind (matched on each
        /// line's text); null for any other search. Not <see cref="LineRegexStatus.FullScan"/>:
        /// feed the candidates' texts (this session's hits, in order) to
        /// <see cref="LineMatchBatch"/>. FullScan: stream every line in scope
        /// (<see cref="LineRegexStatus.ScopeLines"/>, catalog order) to <see cref="LineScanBatch"/>.</summary>
        public LineRegexStatus? GetLineRegexStatus()
        {
            var o = Json.Parse(Interop.TakeString(NativeMethods.se_search_line_status(_handle))) as Dictionary<string, object?>;
            if (o == null) return null;
            return new LineRegexStatus((bool)o["full_scan"]!, (long)(double)o["scope_lines"]!, (long)(double)o["fed"]!,
                                       (long)(double)o["matched"]!, (bool)o["truncated"]!, (bool)o["done"]!);
        }

        /// <summary>Line regex, candidate path: matches the texts of the next
        /// <c>texts.Count</c> candidates (hit order, continuing from the previous call),
        /// across all cores. Per text: its matches as UTF-16 (Start, End) ranges (empty:
        /// no match, or past the 100,000-line cap).</summary>
        public (int Start, int End)[][] LineMatchBatch(IReadOnlyList<string> texts)
        {
            if (texts.Count == 0) return Array.Empty<(int, int)[]>();
            uint[] data = WithTexts(texts, (p, l) =>
                Interop.TakeBuffer(NativeMethods.se_search_line_match_batch(_handle, p, l, (UIntPtr)texts.Count)));
            return ReadRanges(data, texts.Count);
        }

        /// <summary>Line regex, full-scan path: matches lines the host streams itself
        /// (every line in scope, catalog order), each with its placement.</summary>
        public (int Start, int End)[][] LineScanBatch(IReadOnlyList<PlacedHit> lines, IReadOnlyList<string> texts)
        {
            if (lines.Count != texts.Count) throw new ArgumentException("one text per line");
            if (texts.Count == 0) return Array.Empty<(int, int)[]>();
            var raw = new ulong[lines.Count * 3];
            for (int i = 0; i < lines.Count; i++)
            {
                raw[i * 3] = (ulong)lines[i].Position;
                raw[i * 3 + 1] = (ulong)lines[i].LineId;
                raw[i * 3 + 2] = (ulong)lines[i].BookId;
            }
            uint[] data;
            fixed (ulong* r = raw)
            {
                ulong* rp = r;
                data = WithTexts(texts, (p, l) =>
                    Interop.TakeBuffer(NativeMethods.se_search_line_scan_batch(_handle, rp, p, l, (UIntPtr)texts.Count)));
            }
            return ReadRanges(data, texts.Count);
        }

        private static (int Start, int End)[][] ReadRanges(uint[] data, int n)
        {
            var result = new (int, int)[n][];
            int at = 0;
            for (int i = 0; i < n; i++)
            {
                int k = (int)data[at++];
                var ranges = new (int, int)[k];
                for (int j = 0; j < k; j++, at += 2) ranges[j] = ((int)data[at], (int)data[at + 1]);
                result[i] = ranges;
            }
            return result;
        }

        /// <summary>Line regex: the matching lines kept so far [offset, offset+count), in feed order.</summary>
        public PlacedHit[] GetLineHits(long offset, int count)
        {
            if (count <= 0) return Array.Empty<PlacedHit>();
            var raw = new ulong[count * 3];
            ulong copied;
            fixed (ulong* r = raw)
                copied = NativeMethods.se_search_copy_line_hits(_handle, (ulong)offset, (ulong)count, r);
            var hits = new PlacedHit[(int)copied];
            for (int i = 0; i < hits.Length; i++)
                hits[i] = new PlacedHit((long)raw[i * 3], (long)raw[i * 3 + 1], (long)raw[i * 3 + 2]);
            return hits;
        }

        /// <summary>Locates one matched line's snippet (see <see cref="LocateBatch"/>).</summary>
        public Snippet? Locate(string text) => LocateBatch(new[] { text })[0];

        /// <summary>Terms added to the term table since the previous call, in id
        /// order (ids continue from the previous call). A mark's termId indexes
        /// the concatenation of every call's result.</summary>
        public List<string> TakeNewTerms()
        {
            var list = (List<object?>)Json.Parse(Interop.TakeString(NativeMethods.se_search_take_new_terms(_handle)))!;
            var terms = new List<string>(list.Count);
            foreach (var o in list) terms.Add((string)o!);
            return terms;
        }

        public void Dispose()
        {
            _handle.Dispose();
            _registration.Dispose();   // waits for a running cancel callback
            _cancel?.Dispose();
        }
    }

    /// <summary>The state of a line-regex search (<see cref="SearchSession.GetLineRegexStatus"/>).</summary>
    public sealed class LineRegexStatus
    {
        public LineRegexStatus(bool fullScan, long scopeLines, long fed, long matched, bool truncated, bool done)
        {
            FullScan = fullScan; ScopeLines = scopeLines; Fed = fed; Matched = matched; Truncated = truncated; Done = done;
        }

        /// <summary>No selective fixed word: the host streams every line in scope.</summary>
        public bool FullScan { get; }
        /// <summary>Lines to feed: the candidates, or every live line in scope.</summary>
        public long ScopeLines { get; }
        /// <summary>Lines in scope fed so far.</summary>
        public long Fed { get; }
        /// <summary>Matching lines kept so far.</summary>
        public long Matched { get; }
        /// <summary>The 100,000-line cap was reached.</summary>
        public bool Truncated { get; }
        /// <summary>Every line in scope was fed, or the cap was reached.</summary>
        public bool Done { get; }
    }

    /// <summary>How a search was served (<see cref="SearchSession.GetInfo"/>).</summary>
    public sealed class SearchInfo
    {
        /// <summary>Notice: relevance order was asked for a phrase / proximity /
        /// multi-word fuzzy query on an index without positions; the hits are in
        /// catalog order.</summary>
        public const string RelevanceNeedsPositions = "relevance_needs_positions";

        public SearchInfo(IReadOnlyList<string> notices, bool truncated)
        {
            Notices = notices;
            Truncated = truncated;
        }

        /// <summary>Notices about the search (not errors), e.g. <see cref="RelevanceNeedsPositions"/>.</summary>
        public IReadOnlyList<string> Notices { get; }

        /// <summary>A walk (any word, alternative or negative word) hit its expansion
        /// cap (2000 per segment): terms beyond it were not searched.</summary>
        public bool Truncated { get; }
    }
}
