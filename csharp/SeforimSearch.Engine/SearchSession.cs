using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
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

        internal SearchSession(SearchHandle handle, int contextWords)
        {
            _handle = handle;
            ContextWords = contextWords;
            HitCount = (long)NativeMethods.se_search_hit_count(_handle);
        }

        /// <summary>Words of context per side this search locates with.</summary>
        public int ContextWords { get; }

        /// <summary>Number of matching lines (before the snippet filters).</summary>
        public long HitCount { get; }

        /// <summary>Hits [<paramref name="offset"/>, offset+count), catalog order.</summary>
        public PlacedHit[] GetHits(long offset, int count)
        {
            if (count <= 0 || offset >= HitCount) return Array.Empty<PlacedHit>();
            count = (int)Math.Min(count, HitCount - offset);
            var raw = new ulong[count * 5];
            ulong copied;
            fixed (ulong* r = raw)
                copied = NativeMethods.se_search_copy_hits(_handle, (ulong)offset, (ulong)count, r);
            var hits = new PlacedHit[(int)copied];
            for (int i = 0; i < hits.Length; i++)
            {
                int b = i * 5;
                hits[i] = new PlacedHit((long)raw[b], (long)raw[b + 1], (long)raw[b + 2], (long)raw[b + 3], (long)raw[b + 4]);
            }
            return hits;
        }

        /// <summary>Every hit, catalog order.</summary>
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
                uint[] data;
                fixed (IntPtr* p = ptrs)
                fixed (UIntPtr* l = lens)
                    data = Interop.TakeBuffer(NativeMethods.se_search_locate_batch(_handle, p, l, (UIntPtr)n));

                int at = 0;
                for (int i = 0; i < n; i++)
                {
                    if (data[at++] == 0) continue;
                    results[i] = Snippet.Read(data, ref at);
                }
                return results;
            }
            finally
            {
                foreach (var pin in pins)
                    if (pin.IsAllocated) pin.Free();
            }
        }

        /// <summary>Locates one matched line's snippet (see <see cref="LocateBatch"/>).</summary>
        public Snippet? Locate(string text) => LocateBatch(new[] { text })[0];

        /// <summary>
        /// Re-locates a SHORT window over <c>previous + ' ' + text + ' ' + next</c>
        /// (empty sides dropped). Offsets are into that joined text. Score and word
        /// distance are kept from <paramref name="matchedLine"/>. Null if the
        /// re-locate fails (keep the original snippet then).
        /// </summary>
        public Snippet? LocateWithNeighbours(string previous, string text, string next, Snippet matchedLine)
        {
            byte[] p = Interop.ToUtf8(previous), t = Interop.ToUtf8(text), n = Interop.ToUtf8(next);
            uint[] data;
            fixed (byte* pp = p.Length == 0 ? new byte[1] : p)
            fixed (byte* tp = t.Length == 0 ? new byte[1] : t)
            fixed (byte* np = n.Length == 0 ? new byte[1] : n)
                data = Interop.TakeBuffer(NativeMethods.se_search_locate_with_neighbours(_handle,
                    pp, (UIntPtr)p.Length, tp, (UIntPtr)t.Length, np, (UIntPtr)n.Length,
                    (uint)matchedLine.Score, (uint)matchedLine.WordDistance));
            if (data.Length == 0 || data[0] == 0) return null;
            int at = 1;
            return Snippet.Read(data, ref at);
        }

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

        public void Dispose() => _handle.Dispose();
    }
}
