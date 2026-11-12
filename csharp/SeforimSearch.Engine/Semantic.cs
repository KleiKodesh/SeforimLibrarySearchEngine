using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>One line's vector from the host's embedding model (the engine runs no model).</summary>
    public sealed class LineVector
    {
        public long LineId { get; set; }
        public long BookId { get; set; }
        public float[] Vector { get; set; } = Array.Empty<float>();
    }

    /// <summary>One vector-sidecar write: removals (whole books, single lines), then additions.
    /// A line added again replaces its vector; an updated book = <see cref="RemoveBooks"/> + its lines.</summary>
    public sealed class VectorUpdate
    {
        public int Dims { get; set; }
        public List<long> RemoveBooks { get; } = new List<long>();
        public List<long> RemoveLines { get; } = new List<long>();
        public List<LineVector> Add { get; } = new List<LineVector>();
    }

    /// <summary>What a vector write did.</summary>
    public sealed class VectorUpdateStats
    {
        public long Generation { get; set; }
        public long Lines { get; set; }
        public long Books { get; set; }
        public long Removed { get; set; }
        public long Added { get; set; }
        /// <summary>True: the HNSW graph was built afresh (first build, or many removals).</summary>
        public bool Rebuilt { get; set; }
    }

    /// <summary>The vector sidecar's size.</summary>
    public sealed class VectorInfo
    {
        public int Dims { get; set; }
        public long Lines { get; set; }
        public long Books { get; set; }
        public long Generation { get; set; }
    }

    /// <summary>A semantic search request.</summary>
    public sealed class SemanticOptions
    {
        /// <summary>How many lines (a ranked cut list).</summary>
        public int K { get; set; } = 10;
        /// <summary>Only these books (the lexical search's filter); null = all.</summary>
        public IReadOnlyList<long>? Books { get; set; }
        /// <summary>HNSW candidates = K * Oversample, rescored with the full vectors.</summary>
        public float Oversample { get; set; } = 3f;
        /// <summary>A filter leaving at most this many lines is an exact scan.</summary>
        public long ExactThreshold { get; set; } = 20000;

        internal string ToJson()
        {
            var sb = new StringBuilder("{\"k\":").Append(K.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"oversample\":").Append(Oversample.ToString("R", CultureInfo.InvariantCulture));
            sb.Append(",\"exact_threshold\":").Append(Math.Max(0, ExactThreshold).ToString(CultureInfo.InvariantCulture));
            if (Books != null)
            {
                sb.Append(",\"books\":[");
                for (int i = 0; i < Books.Count; i++)
                    sb.Append(i == 0 ? "" : ",").Append(Books[i].ToString(CultureInfo.InvariantCulture));
                sb.Append(']');
            }
            return sb.Append('}').ToString();
        }
    }

    /// <summary>One semantic hit: cosine similarity, best first, ties by line id.</summary>
    public sealed class SemanticHit
    {
        public long LineId { get; set; }
        public long BookId { get; set; }
        public float Score { get; set; }
    }

    /// <summary>A semantic search's hits and how it was served.</summary>
    public sealed class SemanticResult
    {
        public SemanticHit[] Hits { get; set; } = Array.Empty<SemanticHit>();
        /// <summary>True: exact scan of the filtered lines; false: HNSW + rescore.</summary>
        public bool Exact { get; set; }
        public long Allowed { get; set; }
        public long Generation { get; set; }
        internal string Json { get; set; } = "{\"hits\":[]}";
    }

    /// <summary>Hybrid fusion strategy.</summary>
    public enum FusionStrategy { Weighted, Rrf }

    /// <summary>How to fuse lexical and semantic results (Otzaria's fusion).</summary>
    public sealed class FusionOptions
    {
        public FusionStrategy Strategy { get; set; } = FusionStrategy.Weighted;
        /// <summary>Weighted: lexical weight 0..1; null = Otzaria's table by query type.</summary>
        public float? Alpha { get; set; }
        /// <summary>Weighted: BM25 saturation k (x / (k + x)).</summary>
        public float Bm25K { get; set; } = 10f;
        /// <summary>RRF: k (1 / (k + rank)).</summary>
        public int RrfK { get; set; } = 60;

        internal string ToJson() => Strategy == FusionStrategy.Rrf
            ? "{\"strategy\":\"rrf\",\"k\":" + RrfK.ToString(CultureInfo.InvariantCulture) + "}"
            : "{\"strategy\":\"weighted\",\"bm25_k\":" + Bm25K.ToString("R", CultureInfo.InvariantCulture)
              + (Alpha.HasValue ? ",\"alpha\":" + Alpha.Value.ToString("R", CultureInfo.InvariantCulture) : "") + "}";
    }

    /// <summary>One hybrid result.</summary>
    public sealed class FusedHit
    {
        public long LineId { get; set; }
        public long BookId { get; set; }
        /// <summary>The lexical hit's catalog position; null for a semantic-only line.</summary>
        public long? Position { get; set; }
        public float Fused { get; set; }
        public float? Lexical { get; set; }
        public float? Semantic { get; set; }
        /// <summary>"lexical", "semantic" or "both".</summary>
        public string Source { get; set; } = "";
    }

    /// <summary>
    /// Semantic search (issue #18): the vector sidecar of an index, semantic-only search
    /// and hybrid fusion with a lexical search. Sync calls block; the <c>*Async</c> ones
    /// run on the thread pool and are cancelled through the native cancel token.
    /// </summary>
    public static unsafe class SeforimSemantic
    {
        /// <summary>Applies a vector write and publishes a new generation (background priority).</summary>
        public static VectorUpdateStats UpdateVectors(this SeforimIndex index, VectorUpdate update, CancellationToken ct = default)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (update == null) throw new ArgumentNullException(nameof(update));
            int dims = update.Dims;
            int n = update.Add.Count;
            var lines = new ulong[n];
            var books = new ulong[n];
            var floats = new float[checked(n * dims)];
            for (int i = 0; i < n; i++)
            {
                var v = update.Add[i];
                if (v.Vector.Length != dims)
                    throw new ArgumentException($"line {v.LineId} has {v.Vector.Length} dims, the update {dims}");
                lines[i] = (ulong)v.LineId;
                books[i] = (ulong)v.BookId;
                Array.Copy(v.Vector, 0, floats, i * dims, dims);
            }
            var rb = ToU64(update.RemoveBooks);
            var rl = ToU64(update.RemoveLines);
            return WithToken(ct, token =>
            {
                fixed (ulong* prb = rb) fixed (ulong* prl = rl) fixed (ulong* pl = lines) fixed (ulong* pb = books) fixed (float* pv = floats)
                {
                    var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_vectors_update(
                        index.Handle, (uint)dims, prb, (UIntPtr)rb.Length, prl, (UIntPtr)rl.Length, pl, pb, pv, (UIntPtr)n, token)))!;
                    return new VectorUpdateStats
                    {
                        Generation = L(obj["generation"]),
                        Lines = L(obj["lines"]),
                        Books = L(obj["books"]),
                        Removed = L(obj["removed"]),
                        Added = L(obj["added"]),
                        Rebuilt = (bool)obj["rebuilt"]!,
                    };
                }
            });
        }

        /// <summary>The vector sidecar's size; null when the index has no vectors.</summary>
        public static VectorInfo? GetVectorInfo(this SeforimIndex index)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (!(Json.Parse(Interop.TakeString(NativeMethods.se_vectors_info(index.Handle))) is Dictionary<string, object?> obj))
                return null;
            return new VectorInfo { Dims = (int)L(obj["dims"]), Lines = L(obj["lines"]), Books = L(obj["books"]), Generation = L(obj["generation"]) };
        }

        /// <summary>The K lines closest (cosine) to the query vector (same model as the stored vectors).</summary>
        public static SemanticResult SemanticSearch(this SeforimIndex index, float[] query, SemanticOptions? options = null,
                                                    CancellationToken ct = default)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (query == null) throw new ArgumentNullException(nameof(query));
            byte[] json = Interop.ToUtf8((options ?? new SemanticOptions()).ToJson());
            return WithToken(ct, token =>
            {
                fixed (float* q = query) fixed (byte* j = json)
                {
                    string text = Interop.TakeString(NativeMethods.se_semantic_search(index.Handle, q, (UIntPtr)query.Length, j,
                                                                                      (UIntPtr)json.Length, token));
                    var obj = (Dictionary<string, object?>)Json.Parse(text)!;
                    var list = (List<object?>)obj["hits"]!;
                    var hits = new SemanticHit[list.Count];
                    for (int i = 0; i < hits.Length; i++)
                    {
                        var h = (Dictionary<string, object?>)list[i]!;
                        hits[i] = new SemanticHit { LineId = L(h["line_id"]), BookId = L(h["book_id"]), Score = (float)(double)h["score"]! };
                    }
                    return new SemanticResult
                    {
                        Hits = hits,
                        Exact = (bool)obj["exact"]!,
                        Allowed = L(obj["allowed"]),
                        Generation = L(obj["generation"]),
                        Json = text,
                    };
                }
            });
        }

        /// <summary>Hybrid ranking: EVERY hit of the finished lexical <paramref name="session"/>
        /// fused with <paramref name="semantic"/> (from <see cref="SemanticSearch"/> with the same book
        /// filter). Weighted fusion needs the search in relevance order; a streamed search must be
        /// pulled to the end. <paramref name="query"/> is the lexical query text (alpha by query type).</summary>
        public static FusedHit[] FuseSemantic(this SearchSession session, string query, SemanticResult semantic,
                                              FusionOptions? fusion = null)
        {
            if (session == null) throw new ArgumentNullException(nameof(session));
            if (semantic == null) throw new ArgumentNullException(nameof(semantic));
            byte[] q = Interop.ToUtf8(query ?? "");
            byte[] s = Interop.ToUtf8(semantic.Json);
            byte[] f = Interop.ToUtf8((fusion ?? new FusionOptions()).ToJson());
            fixed (byte* pq = q) fixed (byte* ps = s) fixed (byte* pf = f)
            {
                var list = (List<object?>)Json.Parse(Interop.TakeString(NativeMethods.se_search_fuse_semantic(
                    session.Handle, pq, (UIntPtr)q.Length, ps, (UIntPtr)s.Length, pf, (UIntPtr)f.Length)))!;
                var hits = new FusedHit[list.Count];
                for (int i = 0; i < hits.Length; i++)
                {
                    var h = (Dictionary<string, object?>)list[i]!;
                    hits[i] = new FusedHit
                    {
                        LineId = L(h["line_id"]),
                        BookId = L(h["book_id"]),
                        Position = h["position"] == null ? (long?)null : L(h["position"]),
                        Fused = (float)(double)h["fused"]!,
                        Lexical = h["lexical"] == null ? (float?)null : (float)(double)h["lexical"]!,
                        Semantic = h["semantic"] == null ? (float?)null : (float)(double)h["semantic"]!,
                        Source = (string)h["source"]!,
                    };
                }
                return hits;
            }
        }

        /// <summary><see cref="UpdateVectors"/> on the thread pool.</summary>
        public static Task<VectorUpdateStats> UpdateVectorsAsync(this SeforimIndex index, VectorUpdate update, CancellationToken ct = default) =>
            Task.Run(() => index.UpdateVectors(update, ct), ct);

        /// <summary><see cref="SemanticSearch"/> on the thread pool.</summary>
        public static Task<SemanticResult> SemanticSearchAsync(this SeforimIndex index, float[] query, SemanticOptions? options = null,
                                                               CancellationToken ct = default) =>
            Task.Run(() => index.SemanticSearch(query, options, ct), ct);

        /// <summary><see cref="FuseSemantic"/> on the thread pool.</summary>
        public static Task<FusedHit[]> FuseSemanticAsync(this SearchSession session, string query, SemanticResult semantic,
                                                         FusionOptions? fusion = null, CancellationToken ct = default) =>
            Task.Run(() => session.FuseSemantic(query, semantic, fusion), ct);

        private static T WithToken<T>(CancellationToken ct, Func<CancelTokenHandle, T> call)
        {
            ct.ThrowIfCancellationRequested();
            if (!ct.CanBeCanceled)
                using (var none = new CancelTokenHandle()) return call(none);
            using var token = NativeMethods.se_cancel_token_new();
            using var registration = ct.Register(s => NativeMethods.se_cancel_token_cancel(((CancelTokenHandle)s!).DangerousGetHandle()), token);
            return call(token);
        }

        private static ulong[] ToU64(List<long> values)
        {
            var a = new ulong[values.Count];
            for (int i = 0; i < a.Length; i++) a[i] = (ulong)values[i];
            return a;
        }

        private static long L(object? v) => (long)(double)v!;
    }
}
