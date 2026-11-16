using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Engine.Native;

namespace SeforimSearch.Engine
{
    /// <summary>One segment file of the open Otzaria vector set.</summary>
    public sealed class VectorSegmentInfo
    {
        public string File { get; set; } = "";
        /// <summary>"base" or "delta".</summary>
        public string Kind { get; set; } = "";
        public int FromLibraryVersion { get; set; }
        public int ToLibraryVersion { get; set; }
        public long Slots { get; set; }
        public long SlotsDead { get; set; }
        public long Foreign { get; set; }
        public long ForeignUnresolved { get; set; }
        public long Size { get; set; }
    }

    /// <summary>The open Otzaria vector set (<see cref="SeforimSemantic.OpenVectors"/>, read-only).</summary>
    public sealed class VectorSetInfo
    {
        public string Path { get; set; } = "";
        public long Generation { get; set; }
        public bool RecoveredFromPrevious { get; set; }
        public int Dims { get; set; }
        public string Codec { get; set; } = "";
        public int LibraryVersion { get; set; }
        public string LibraryReleaseTag { get; set; } = "";
        public List<VectorSegmentInfo> Segments { get; set; } = new List<VectorSegmentInfo>();
        public long SlotsLive { get; set; }
        public long SlotsDead { get; set; }
        /// <summary>Books in the set.</summary>
        public long Books { get; set; }
        /// <summary>Books of the set the host mapped.</summary>
        public long BooksMapped { get; set; }
        public long BooksUnmapped { get; set; }
        public long BytesOnDisk { get; set; }
    }

    /// <summary>A semantic search request (Otzaria's exact scan).</summary>
    public sealed class SemanticOptions
    {
        /// <summary>How many lines.</summary>
        public int K { get; set; } = 10;
        /// <summary>Only these books (the lexical search's filter); null = all.</summary>
        public IReadOnlyList<long>? Books { get; set; }
        /// <summary>Scan threads; 0 = half the cores, at most 8.</summary>
        public int Threads { get; set; }
        /// <summary>Taken off a window past a line's first one (Otzaria's window penalty).</summary>
        public float WindowPenalty { get; set; } = 0.04f;
        /// <summary>Taken off an occurrence found through a neighbour's vector (Otzaria's alias penalty).</summary>
        public float AliasPenalty { get; set; } = 0.04f;

        internal string ToJson()
        {
            var sb = new StringBuilder("{\"k\":").Append(K.ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"threads\":").Append(Math.Max(0, Threads).ToString(CultureInfo.InvariantCulture));
            sb.Append(",\"window_penalty\":").Append(WindowPenalty.ToString("R", CultureInfo.InvariantCulture));
            sb.Append(",\"alias_penalty\":").Append(AliasPenalty.ToString("R", CultureInfo.InvariantCulture));
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

    /// <summary>One semantic hit: one per line (its best occurrence), best first.</summary>
    public sealed class SemanticHit
    {
        /// <summary><see cref="Chunk"/> bit: found through a neighbour's vector (Otzaria's alias).</summary>
        public const uint Borrowed = 1u << 31;
        public long LineId { get; set; }
        public long BookId { get; set; }
        public float Score { get; set; }
        /// <summary>0 = the line's first window, n its n-th; may carry <see cref="Borrowed"/>.</summary>
        public uint Chunk { get; set; }
    }

    /// <summary>A semantic search's hits and what the scan did.</summary>
    public sealed class SemanticResult
    {
        public SemanticHit[] Hits { get; set; } = Array.Empty<SemanticHit>();
        /// <summary>Vectors scored.</summary>
        public long Vectors { get; set; }
        /// <summary>Line occurrences of the best vectors read.</summary>
        public long Records { get; set; }
        /// <summary>Records the host's mapping does not cover (dropped, never guessed).</summary>
        public long Unmapped { get; set; }
        public long Scanned { get; set; }
        public int Threads { get; set; }
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

    /// <summary>The host's mapping of one book of an Otzaria vector set (<see cref="SeforimSemantic.SetVectorMapping"/>).</summary>
    public sealed class OxvBookMap
    {
        public string Name { get; set; } = "";
        public long BookId { get; set; }
        /// <summary>The line id of each 0-based line ordinal (seforim.db rows in lineIndex, id order).</summary>
        public IReadOnlyList<long> LineIds { get; set; } = Array.Empty<long>();
    }

    /// <summary>One <c>line_note</c> row of a book (non-NULL body) for <see cref="SeforimSemantic.MapVectorBookRows"/>.</summary>
    public sealed class VectorBookNote
    {
        /// <summary>The <c>lineIndex</c> of the note's line.</summary>
        public long LineIndex { get; set; }
        /// <summary><c>charPos</c>, -1 when absent.</summary>
        public long CharPos { get; set; } = -1;
        public bool HasMarker { get; set; }
        public string Body { get; set; } = "";
    }

    /// <summary>The text check of one book (<see cref="SeforimSemantic.MapVectorBookRows"/>).</summary>
    public sealed class VectorBookVerify
    {
        public long Lines { get; set; }
        public long Records { get; set; }
        public long Verified { get; set; }
        public long Moved { get; set; }
        public long Unverified { get; set; }
    }

    /// <summary>
    /// Semantic search (issue #18), like Otzaria: the index opens Otzaria's own vector set
    /// read-only (a set directory such as <c>%APPDATA%\otzaria\vectors</c>, or one base <c>.oxv</c>)
    /// and runs an exact integer scan. The host maps each book of the set (<c>id:&lt;bookId&gt;</c>)
    /// to its line ids. Sync calls block; the <c>*Async</c> ones run on the thread pool and are
    /// cancelled through the native cancel token.
    /// </summary>
    public static unsafe class SeforimSemantic
    {
        /// <summary>Opens Otzaria's vector set read-only (replaces one opened before; the line mapping stays).</summary>
        public static VectorSetInfo OpenVectors(this SeforimIndex index, string path)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            byte[] p = Interop.ToUtf8(path ?? throw new ArgumentNullException(nameof(path)));
            fixed (byte* pp = p)
                return ParseSetInfo((Dictionary<string, object?>)Json.Parse(Interop.TakeString(
                    NativeMethods.se_vectors_open(index.Handle, pp, (UIntPtr)p.Length)))!);
        }

        /// <summary><see cref="OpenVectors"/> on the thread pool.</summary>
        public static Task<VectorSetInfo> OpenVectorsAsync(this SeforimIndex index, string path, CancellationToken ct = default) =>
            Task.Run(() => index.OpenVectors(path), ct);

        /// <summary>Forgets the vector set (the line mapping stays).</summary>
        public static void CloseVectors(this SeforimIndex index)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (NativeMethods.se_vectors_close(index.Handle) != 0) throw Interop.Fail(Interop.LastError());
        }

        /// <summary>Maps one book of the set (<paramref name="name"/>, <c>id:&lt;bookId&gt;</c> for a library
        /// book) to the host's lines: <paramref name="lineIds"/>[ordinal] for each 0-based line ordinal
        /// (seforim.db rows of the book in <c>lineIndex, id</c> order). Replaces that book's mapping;
        /// an empty list removes it.</summary>
        public static void MapVectorBook(this SeforimIndex index, string name, long bookId, IReadOnlyList<long> lineIds)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (lineIds == null) throw new ArgumentNullException(nameof(lineIds));
            byte[] n = Interop.ToUtf8(name ?? throw new ArgumentNullException(nameof(name)));
            var ids = new ulong[lineIds.Count];
            for (int i = 0; i < ids.Length; i++) ids[i] = (ulong)lineIds[i];
            fixed (byte* pn = n) fixed (ulong* pl = ids)
                if (NativeMethods.se_vectors_map_book(index.Handle, pn, (UIntPtr)n.Length, (ulong)bookId, pl, (UIntPtr)ids.Length) != 0)
                    throw Interop.Fail(Interop.LastError());
        }

        /// <summary>Maps one book WITH Otzaria's text check: <paramref name="rows"/> = the book's raw rows
        /// (UTF-8 content, seforim.db <c>lineIndex, id</c> order, NULL as empty), <paramref name="rowLineIds"/>
        /// their line ids, <paramref name="notes"/> its <c>line_note</c> rows (lineIndex, noteIndex order).
        /// Records whose recomputed chunk key fails are dropped and counted. <paramref name="chunkingIdentity"/>
        /// 0 = the set's (a single .oxv needs the model's).</summary>
        public static VectorBookVerify MapVectorBookRows(this SeforimIndex index, string name, ulong bookId, ulong[] rowLineIds,
                                                         byte[][] rows, IReadOnlyList<VectorBookNote>? notes, ulong chunkingIdentity = 0)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (rowLineIds == null) throw new ArgumentNullException(nameof(rowLineIds));
            if (rows == null) throw new ArgumentNullException(nameof(rows));
            if (rows.Length != rowLineIds.Length) throw new ArgumentException("rows and rowLineIds differ in length", nameof(rows));
            byte[] n = Interop.ToUtf8(name ?? throw new ArgumentNullException(nameof(name)));
            var offsets = new ulong[rows.Length + 1];
            long total = 0;
            for (int i = 0; i < rows.Length; i++) { offsets[i] = (ulong)total; total += rows[i]?.Length ?? 0; }
            offsets[rows.Length] = (ulong)total;
            var content = new byte[Math.Max(1, total)];
            for (int i = 0; i < rows.Length; i++)
                if (rows[i] != null) Buffer.BlockCopy(rows[i], 0, content, (int)offsets[i], rows[i].Length);
            var sb = new StringBuilder("[");
            if (notes != null)
                for (int i = 0; i < notes.Count; i++)
                {
                    var t = notes[i];
                    sb.Append(i == 0 ? "{" : ",{")
                      .Append("\"line_index\":").Append(t.LineIndex.ToString(CultureInfo.InvariantCulture))
                      .Append(",\"char_pos\":").Append(t.CharPos.ToString(CultureInfo.InvariantCulture))
                      .Append(",\"has_marker\":").Append(t.HasMarker ? "true" : "false")
                      .Append(",\"body\":").Append(JsonString(t.Body ?? "")).Append('}');
                }
            byte[] nj = Interop.ToUtf8(sb.Append(']').ToString());
            ulong[] ids = rowLineIds.Length == 0 ? new ulong[1] : rowLineIds;
            fixed (byte* pn = n) fixed (ulong* pl = ids) fixed (byte* pc = content) fixed (ulong* po = offsets) fixed (byte* pj = nj)
            {
                var obj = (Dictionary<string, object?>)Json.Parse(Interop.TakeString(NativeMethods.se_vectors_map_book_rows(
                    index.Handle, pn, (UIntPtr)n.Length, bookId, pl, (UIntPtr)rowLineIds.Length, pc, (UIntPtr)total, po,
                    pj, (UIntPtr)nj.Length, chunkingIdentity)))!;
                return new VectorBookVerify
                {
                    Lines = L(obj["lines"]), Records = L(obj["records"]), Verified = L(obj["verified"]),
                    Moved = L(obj["moved"]), Unverified = L(obj["unverified"]),
                };
            }
        }

        /// <summary>Maps several books at once (each as <see cref="MapVectorBook"/>); <paramref name="replace"/>
        /// drops every book mapped before.</summary>
        public static void SetVectorMapping(this SeforimIndex index, IEnumerable<OxvBookMap> books, bool replace)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            if (books == null) throw new ArgumentNullException(nameof(books));
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
            fixed (byte* pj = json)
                if (NativeMethods.se_vectors_set_mapping(index.Handle, pj, (UIntPtr)json.Length, replace ? 1 : 0) != 0)
                    throw Interop.Fail(Interop.LastError());
        }

        /// <summary>The open vector set; null when none is open.</summary>
        public static VectorSetInfo? VectorInfo(this SeforimIndex index)
        {
            if (index == null) throw new ArgumentNullException(nameof(index));
            return Json.Parse(Interop.TakeString(NativeMethods.se_vectors_info(index.Handle))) is Dictionary<string, object?> obj
                ? ParseSetInfo(obj) : null;
        }

        private static VectorSetInfo ParseSetInfo(Dictionary<string, object?> obj)
        {
            var segments = new List<VectorSegmentInfo>();
            foreach (var o in (List<object?>)obj["segments"]!)
            {
                var s = (Dictionary<string, object?>)o!;
                segments.Add(new VectorSegmentInfo
                {
                    File = (string)s["file"]!,
                    Kind = (string)s["kind"]!,
                    FromLibraryVersion = (int)L(s["from_library_version"]),
                    ToLibraryVersion = (int)L(s["to_library_version"]),
                    Slots = L(s["slots"]),
                    SlotsDead = L(s["slots_dead"]),
                    Foreign = L(s["foreign"]),
                    ForeignUnresolved = L(s["foreign_unresolved"]),
                    Size = L(s["size"]),
                });
            }
            return new VectorSetInfo
            {
                Path = (string)obj["path"]!,
                Generation = L(obj["generation"]),
                RecoveredFromPrevious = (bool)obj["recovered_from_previous"]!,
                Dims = (int)L(obj["dims"]),
                Codec = (string)obj["codec"]!,
                LibraryVersion = (int)L(obj["library_version"]),
                LibraryReleaseTag = (string)obj["library_release_tag"]!,
                Segments = segments,
                SlotsLive = L(obj["slots_live"]),
                SlotsDead = L(obj["slots_dead"]),
                Books = L(obj["books"]),
                BooksMapped = L(obj["books_mapped"]),
                BooksUnmapped = L(obj["books_unmapped"]),
                BytesOnDisk = L(obj["bytes_on_disk"]),
            };
        }

        /// <summary>The K lines closest to the query vector (Otzaria's query model): an exact scan of
        /// every live vector of the set (or the filter's books), mapped to the host's lines.</summary>
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
                        Vectors = L(obj["vectors"]),
                        Records = L(obj["records"]),
                        Unmapped = L(obj["unmapped"]),
                        Scanned = L(obj["scanned"]),
                        Threads = (int)L(obj["threads"]),
                        Generation = L(obj["generation"]),
                        Json = text,
                    };
                }
            });
        }

        /// <summary>Hybrid ranking: EVERY hit of the finished lexical <paramref name="session"/>
        /// fused with <paramref name="semantic"/> (from <see cref="SemanticSearch"/> with the same book
        /// filter), Otzaria-style: the lexical side is the BM25 of Otzaria's exact query (any search order); a streamed search must be
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

        private static long L(object? v) => (long)(double)v!;
    }
}
