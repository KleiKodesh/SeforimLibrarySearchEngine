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
            ulong copied;
            fixed (ulong* r = raw)
                copied = NativeMethods.se_search_copy_hits(_handle, (ulong)offset, (ulong)count, r);
            var hits = new PlacedHit[(int)copied];
            for (int i = 0; i < hits.Length; i++)
            {
                int b = i * 3;
                hits[i] = new PlacedHit((long)raw[b], (long)raw[b + 1], (long)raw[b + 2]);
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
