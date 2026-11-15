using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>One vector of a line from the host's embedding model (the engine runs no model).
    /// A long line may have several (chunks); the line scores as its best chunk.</summary>
    public sealed class LineVector
    {
        /// <summary><see cref="Chunk"/> bit: a vector borrowed from a neighbour (Otzaria's alias).</summary>
        public const uint Borrowed = 1u << 31;
        public long LineId { get; set; }
        public long BookId { get; set; }
        /// <summary>0 = the line's first (or only) vector; 1, 2, ... its further chunks; may carry <see cref="Borrowed"/>.</summary>
        public uint Chunk { get; set; }
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
        /// <summary>Vectors (chunks) in the store.</summary>
        public long Vectors { get; set; }
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
        /// <summary>Vectors (a long line may have several chunks).</summary>
        public long Vectors { get; set; }
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
        /// <summary>A filter leaving at most this many vectors is an exact scan.</summary>
        public long ExactThreshold { get; set; } = 20000;
        /// <summary>Taken off a chunk past a line's first one before its best chunk is chosen (Otzaria's window penalty).</summary>
        public float ChunkPenalty { get; set; } = 0.04f;

        internal string ToJson()
        {
            var sb = new StringBuilder("{\"k\":").Append(K.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"oversample\":").Append(Oversample.ToString("R", CultureInfo.InvariantCulture));
            sb.Append(",\"exact_threshold\":").Append(Math.Max(0, ExactThreshold).ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"chunk_penalty\":").Append(ChunkPenalty.ToString("R", CultureInfo.InvariantCulture));
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
        /// <summary>The chunk that scored the line (0 = its first vector).</summary>
        public uint Chunk { get; set; }
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

    /// <summary>What an Otzaria vector file holds (<see cref="SeforimSemantic.ReadOxvInfo"/>).</summary>
    public sealed class OxvInfo
    {
        public int Format { get; set; }
        public string Kind { get; set; } = "";
        public int Dims { get; set; }
        public string Codec { get; set; } = "";
        public string LibraryReleaseTag { get; set; } = "";
        public int FromLibraryVersion { get; set; }
        public int ToLibraryVersion { get; set; }
        public long Slots { get; set; }
        public List<OxvBookInfo> Books { get; set; } = new List<OxvBookInfo>();
    }

    /// <summary>One book of an Otzaria vector file.</summary>
    public sealed class OxvBookInfo
    {
        /// <summary>Otzaria's index file path: <c>id:&lt;bookId&gt;</c> for a library book.</summary>
        public string Name { get; set; } = "";
        /// <summary>Records (line occurrences of a vector, chunks included).</summary>
        public long Records { get; set; }
        /// <summary>Highest line ordinal + 1: the line count a mapping must cover.</summary>
        public long Lines { get; set; }
    }

    /// <summary>The host's mapping of one book of an Otzaria vector file.</summary>
    public sealed class OxvBookMap
    {
        public string Name { get; set; } = "";
        public long BookId { get; set; }
        /// <summary>The line id of each 0-based line ordinal (seforim.db rows in lineIndex, id order).</summary>
        public IReadOnlyList<long> LineIds { get; set; } = Array.Empty<long>();
    }

    /// <summary>What <see cref="SeforimSemantic.ImportOxv"/> did.</summary>
    public sealed class OxvImportReport
    {
        public long BooksImported { get; set; }
        public List<string> BooksNotInFile { get; set; } = new List<string>();
        public long BooksUnmapped { get; set; }
        public long Records { get; set; }
        public long Vectors { get; set; }
        public long Lines { get; set; }
        /// <summary>Records past the host's line list (dropped).</summary>
        public long OutOfRange { get; set; }
        public long Duplicates { get; set; }
        public long Generation { get; set; }
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
            var chunks = new uint[n];
            var floats = new float[checked(n * dims)];
            for (int i = 0; i < n; i++)
            {
                var v = update.Add[i];
                if (v.Vector.Length != dims)
                    throw new ArgumentException($"line {v.LineId} has {v.Vector.Length} dims, the update {dims}");
                lines[i] = (ulong)v.LineId;
                books[i] = (ulong)v.BookId;
                chunks[i] = v.Chunk;
                Array.Copy(v.Vector, 0, floats, i * dims, dims);
            }
            var rb = ToU64(update.RemoveBooks);
            var rl = ToU64(update.RemoveLines);
            return WithToken(ct, token =>
            {
                fixed (ulong* prb = rb) fixed (ulong* prl = rl) fixed (ulong* pl = lines) fixed (ulong* pb = books) fixed (uint* pc = chunks) fixed (float* pv = floats)
                {
                    var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_vectors_update(
                        index.Handle, (uint)dims, prb, (UIntPtr)rb.Length, prl, (UIntPtr)rl.Length, pl, pb, pc, pv, (UIntPtr)n, token)))!;
                    return new VectorUpdateStats
                    {
                        Generation = L(obj["generation"]),
                        Lines = L(obj["lines"]),
                        Vectors = L(obj["vectors"]),
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
            return new VectorInfo { Dims = (int)L(obj["dims"]), Lines = L(obj["lines"]), Vectors = L(obj["vectors"]), Books = L(obj["books"]),
                                    Generation = L(obj["generation"]) };
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
                        hits[i] = new SemanticHit { LineId = L(h["line_id"]), BookId = L(h["book_id"]), Score = (float)(double)h["score"]!,
                                                 Chunk = (uint)L(h["chunk"]) };
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

        /// <summary>Expands Otzaria's downloaded vector file (<c>.oxv.zst</c>) to a plain <c>.oxv</c>; returns the bytes written.</summary>
        public static long ExpandOxv(string source, string destination)
        {
            byte[] s = Interop.ToUtf8(source ?? throw new ArgumentNullException(nameof(source)));
            byte[] d = Interop.ToUtf8(destination ?? throw new ArgumentNullException(nameof(destination)));
            fixed (byte* ps = s) fixed (byte* pd = d)
            {
                long n = NativeMethods.se_oxv_expand(ps, (UIntPtr)s.Length, pd, (UIntPtr)d.Length);
                if (n < 0) throw Interop.Fail(Interop.LastError());
                return n;
            }
        }

        /// <summary>What an Otzaria vector file (<c>.oxv</c>, base segment) holds: its books (names as
        /// Otzaria's index files them, <c>id:&lt;bookId&gt;</c> for a library book) and their line counts.</summary>
        public static OxvInfo ReadOxvInfo(string path)
        {
            byte[] p = Interop.ToUtf8(path ?? throw new ArgumentNullException(nameof(path)));
            fixed (byte* pp = p)
            {
                var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_oxv_info(pp, (UIntPtr)p.Length)))!;
                var books = new List<OxvBookInfo>();
                foreach (var o in (List<object?>)obj["books"]!)
                {
                    var b = (Dictionary<string, object?>)o!;
                    books.Add(new OxvBookInfo { Name = (string)b["name"]!, Records = L(b["records"]), Lines = L(b["lines"]) });
                }
                return new OxvInfo
                {
                    Format = (int)L(obj["format"]),
                    Kind = (string)obj["kind"]!,
                    Dims = (int)L(obj["dims"]),
                    Codec = (string)obj["codec"]!,
                    LibraryReleaseTag = (string)obj["library_release_tag"]!,
                    FromLibraryVersion = (int)L(obj["from_library_version"]),
                    ToLibraryVersion = (int)L(obj["to_library_version"]),
                    Slots = L(obj["slots"]),
                    Books = books,
                };
            }
        }

        /// <summary>Imports an Otzaria vector file into the index's vector sidecar by the host's line ids
        /// (<paramref name="books"/>: per book of the file, its book id and the line id of each 0-based line
        /// ordinal). Every chunk of a mapped line is kept; each mapped book replaces its old vectors; one
        /// store write per call, so pass books in batches.</summary>
        public static OxvImportReport ImportOxv(this SeforimIndex index, string path, IEnumerable<OxvBookMap> books,
                                                CancellationToken ct = default)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (books == null) throw new ArgumentNullException(nameof(books));
            byte[] p = Interop.ToUtf8(path ?? throw new ArgumentNullException(nameof(path)));
            var sb = new StringBuilder("[");
            bool first = true;
            foreach (var m in books)
            {
                sb.Append(first ? "{" : ",{");
                first = false;
                sb.Append("\"name\":").Append(JsonString(m.Name));
                sb.Append(",\"book_id\":").Append(m.BookId.ToString(CultureInfo.InvariantCulture)).Append(",\"line_ids\":[");
                for (int i = 0; i < m.LineIds.Count; i++)
                    sb.Append(i == 0 ? "" : ",").Append(m.LineIds[i].ToString(CultureInfo.InvariantCulture));
                sb.Append("]}");
            }
            byte[] json = Interop.ToUtf8(sb.Append(']').ToString());
            return WithToken(ct, token =>
            {
                fixed (byte* pp = p) fixed (byte* pj = json)
                {
                    var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_vectors_import_oxv(
                        index.Handle, pp, (UIntPtr)p.Length, pj, (UIntPtr)json.Length, token)))!;
                    var missing = new List<string>();
                    foreach (var n in (List<object?>)obj["books_not_in_file"]!) missing.Add((string)n!);
                    return new OxvImportReport
                    {
                        BooksImported = L(obj["books_imported"]),
                        BooksNotInFile = missing,
                        BooksUnmapped = L(obj["books_unmapped"]),
                        Records = L(obj["records"]),
                        Vectors = L(obj["vectors"]),
                        Lines = L(obj["lines"]),
                        OutOfRange = L(obj["out_of_range"]),
                        Duplicates = L(obj["duplicates"]),
                        Generation = L(obj["generation"]),
                    };
                }
            });
        }

        /// <summary><see cref="ImportOxv"/> on the thread pool.</summary>
        public static Task<OxvImportReport> ImportOxvAsync(this SeforimIndex index, string path, IEnumerable<OxvBookMap> books,
                                                           CancellationToken ct = default) =>
            Task.Run(() => index.ImportOxv(path, books, ct), ct);

        private static string JsonString(string s)
        {
            var sb = new StringBuilder("\"");
            foreach (char c in s ?? "")
            {
                if (c == '"' || c == '\\') sb.Append('\\').Append(c);
                else if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                else sb.Append(c);
            }
            return sb.Append('"').ToString();
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
