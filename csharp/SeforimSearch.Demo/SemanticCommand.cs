using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;
using Microsoft.Data.Sqlite;
using SeforimSearch.Demo.Common;
using SeforimSearch.Demo.SeforimDb;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// Semantic search in the host (issue #18). The engine scans Otzaria's vector set; the
    /// seforim.db mapping and the query model live here.
    ///
    ///   semantic-search &lt;index dir&gt; --vectors &lt;set dir | file.oxv&gt; [--db path] [--model dir]
    ///                   (--line N | --lines a,b,c | --query text) [--k N] [--books a,b] [--threads N]
    ///                   [--hybrid] [--repeat N] [--no-verify] [--chunking-identity N]
    ///     Opens Otzaria's vector set read-only, maps each set book named id:&lt;bookId&gt; from its raw
    ///     rows (lineIndex, id order) and line_note rows with Otzaria's text check (--no-verify: the
    ///     unchecked ordinal mapping; --chunking-identity N, default model.json's, else the set's), embeds
    ///     each query with Otzaria's Meivin model and searches; prints open / mapping / per-query
    ///     latencies, the hits, mean and p95 search latency and the peak working set.
    ///   semantic-tokcheck &lt;golden.json&gt; [--model dir]
    ///     Compares the tokenizer with Otzaria's golden token ids (otzaria-semantic-search
    ///     tests/data/onnx_golden_vectors*.json, same tokenizer.json checksum).
    /// </summary>
    internal static class SemanticCommand
    {
        private static readonly string[] ValueOptions = { "--db", "--index", "--vectors", "--books", "--threads", "--repeat", "--query", "--line", "--lines", "--k", "--model", "--chunking-identity", "--report" };

        private static List<string> Positional(string[] args)
        {
            var list = new List<string>();
            for (int i = 1; i < args.Length; i++)
            {
                if (ValueOptions.Contains(args[i])) { i++; continue; }
                if (args[i].StartsWith("--", StringComparison.Ordinal)) continue;
                list.Add(args[i]);
            }
            return list;
        }

        internal static string Time(TimeSpan t) => t.TotalMilliseconds < 1000 ? $"{t.TotalMilliseconds:F0} ms" : $"{t.TotalMinutes:F2} min";

        private static string DefaultModelDir() =>
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "otzaria", "books", "meivin-qa491-onnx");

        // ---------------------------------------------------------------- mapping

        private static bool TryBookId(string name, out long id)
        {
            id = 0;
            return name.StartsWith("id:", StringComparison.Ordinal) && long.TryParse(name.Substring(3), out id) && id > 0;
        }

        /// <summary>The books of the opened set (name -> line count) read from its segment files'
        /// headers; segments that cannot be read are reported and skipped.</summary>
        private static Dictionary<string, long> SetBooks(string vectors, VectorSetInfo info)
        {
            var books = new Dictionary<string, long>(StringComparer.Ordinal);
            var files = new List<string>();
            if (File.Exists(vectors)) files.Add(vectors);
            else
                foreach (var seg in info.Segments)
                {
                    string[] tries = { seg.File, Path.Combine(vectors, seg.File), Path.Combine(vectors, "segments", Path.GetFileName(seg.File)) };
                    string? hit = tries.FirstOrDefault(File.Exists);
                    if (hit != null) files.Add(hit);
                    else Console.Error.WriteLine($"segment file not found: {seg.File}");
                }
            foreach (var f in files)
            {
                OxvInfo oi;
                try { oi = SeforimSemantic.ReadOxvInfo(f); }
                catch (SeforimEngineException e) { Console.Error.WriteLine($"segment header not read ({Path.GetFileName(f)}): {e.Message}"); continue; }
                foreach (var b in oi.Books)
                    books[b.Name] = books.TryGetValue(b.Name, out long n) ? Math.Max(n, b.Lines) : b.Lines;
            }
            return books;
        }

        /// <summary>Maps every <c>id:&lt;bookId&gt;</c> book of the set. Default: Otzaria's text check
        /// (<see cref="SeforimSemantic.MapVectorBookRows"/>: the book's raw rows in <c>lineIndex, id</c>
        /// order plus its <c>line_note</c> rows; a row holding a newline gives several lines). With
        /// <paramref name="verify"/> false: the old unchecked ordinal mapping (<see cref="SeforimSemantic.MapVectorBook"/>).
        /// Returns the books skipped.</summary>
        internal static long MapBooks(SeforimIndex index, SqliteConnection conn, string vectors, VectorSetInfo info, bool verify, ulong chunkingIdentity,
                                      List<(long Book, int Rows, int Notes, VectorBookVerify V)>? perBook = null)
        {
            var schema = SeforimDbSchema.Probe(conn);
            using var codec = SeforimDbLineContentCodec.Create(conn);
            var books = SetBooks(vectors, info);
            bool hasNotes = verify && SeforimDbSchema.TableExists(conn, "line_note");
            var noteCols = hasNotes ? NoteColumns(conn) : new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            long notIdName = 0, missing = 0, tooFew = 0, mapped = 0;
            var tot = new VectorBookVerify();
            foreach (var kv in books)
            {
                if (!TryBookId(kv.Key, out long bookId)) { notIdName++; continue; }
                var (lineIds, rows) = ReadBookRows(conn, schema, codec, bookId, verify);
                if (lineIds.Count == 0) { missing++; continue; }
                if (!verify)
                {
                    if (kv.Value > lineIds.Count) { tooFew++; continue; }
                    index.MapVectorBook(kv.Key, bookId, lineIds);
                    mapped++;
                    continue;
                }
                var notes = hasNotes ? ReadBookNotes(conn, noteCols, bookId) : new List<VectorBookNote>();
                var v = index.MapVectorBookRows(kv.Key, (ulong)bookId, lineIds.Select(x => (ulong)x).ToArray(), rows!.ToArray(), notes, chunkingIdentity);
                mapped++;
                perBook?.Add((bookId, lineIds.Count, notes.Count, v));
                tot.Lines += v.Lines; tot.Records += v.Records; tot.Verified += v.Verified; tot.Moved += v.Moved; tot.Unverified += v.Unverified;
            }
            Console.WriteLine($"books mapped {mapped:N0}; skipped: name not id:N {notIdName:N0}, missing from DB {missing:N0}" +
                              (verify ? "" : $", set lines > DB rows {tooFew:N0} (no text check)"));
            if (verify)
                Console.WriteLine($"text check: lines {tot.Lines:N0}, records {tot.Records:N0}, verified {tot.Verified:N0}, " +
                                  $"moved {tot.Moved:N0}, unverified {tot.Unverified:N0}");
            return notIdName + missing + tooFew;
        }

        /// <summary><c>semantic-verify &lt;scratch index dir&gt; --vectors &lt;set dir | .oxv&gt; [--chunking-identity N]
        /// [--report N]</c>: maps every book of the set with Otzaria's text check and prints the totals, the
        /// books with the most unverified records (rows, notes, lines, records, unverified) and the time.</summary>
        internal static int Verify(string db, string[] args)
        {
            var pos = Positional(args);
            string? vectors = Program.OptionOf(args, "--vectors");
            if (pos.Count == 0 || vectors == null) { Console.Error.WriteLine("usage: semantic-verify <scratch index dir> --vectors <set dir | .oxv>"); return 2; }
            int report = int.TryParse(Program.OptionOf(args, "--report"), out int rr) ? rr : 30;
            string? ciArg = Program.OptionOf(args, "--chunking-identity");
            ulong identity = ciArg != null ? ulong.Parse(ciArg) : 0;
            using var index = Directory.Exists(pos[0]) && Directory.EnumerateFileSystemEntries(pos[0]).Any() ? SeforimIndex.Open(pos[0]) : SeforimIndex.Create(pos[0]);
            var info = index.OpenVectors(vectors);
            using var conn = SqliteConnectionFactory.OpenCorpusRead(db);
            var per = new List<(long Book, int Rows, int Notes, VectorBookVerify V)>();
            var sw = Stopwatch.StartNew();
            MapBooks(index, conn, vectors, info, true, identity, per);
            Console.WriteLine($"mapping + verify: {Time(sw.Elapsed)}, books {per.Count:N0}");
            int bad = per.Count(b => b.V.Unverified > 0);
            Console.WriteLine($"books with unverified records: {bad:N0}; all records unverified: {per.Count(b => b.V.Records > 0 && b.V.Unverified == b.V.Records):N0}");
            foreach (var b in per.OrderByDescending(b => b.V.Unverified).Take(report))
                Console.WriteLine($"  book {b.Book,6}: rows {b.Rows,7:N0} notes {b.Notes,6:N0} lines {b.V.Lines,7:N0} records {b.V.Records,7:N0} " +
                                  $"verified {b.V.Verified,7:N0} moved {b.V.Moved,6:N0} unverified {b.V.Unverified,7:N0}");
            return 0;
        }

        /// <summary>The book's line ids in <c>lineIndex, id</c> order and (when <paramref name="withRows"/>)
        /// each row's decoded raw content as UTF-8 (NULL as empty).</summary>
        private static (List<long> Ids, List<byte[]>? Rows) ReadBookRows(SqliteConnection conn, SeforimDbSchema schema,
                                                                          SeforimDbLineContentCodec codec, long bookId, bool withRows)
        {
            var ids = new List<long>();
            var rows = withRows ? new List<byte[]>() : null;
            using var cmd = conn.CreateCommand();
            cmd.CommandText = withRows
                ? "SELECT l.id, " + SeforimDbLineContentSql.RawContentExpression(schema, "l") + " FROM line l" +
                  SeforimDbLineContentSql.ContentJoin(schema, "l") + " WHERE l.bookId = $b ORDER BY l.lineIndex, l.id"
                : "SELECT id FROM line WHERE bookId = $b ORDER BY lineIndex, id";
            cmd.Parameters.AddWithValue("$b", bookId);
            using var r = cmd.ExecuteReader();
            while (r.Read())
            {
                long id = r.GetInt64(0);
                ids.Add(id);
                if (rows != null) rows.Add(Encoding.UTF8.GetBytes(codec.DecodeColumn(r, 1, id) ?? ""));
            }
            return (ids, rows);
        }

        private static HashSet<string> NoteColumns(SqliteConnection conn)
        {
            var cols = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            using var cmd = conn.CreateCommand();
            cmd.CommandText = "PRAGMA table_info(line_note)";
            using var r = cmd.ExecuteReader();
            while (r.Read()) cols.Add(r.GetString(1));
            return cols;
        }

        /// <summary>The book's <c>line_note</c> rows with a body, in <c>lineIndex, noteIndex</c> order.</summary>
        private static List<VectorBookNote> ReadBookNotes(SqliteConnection conn, HashSet<string> cols, long bookId)
        {
            var notes = new List<VectorBookNote>();
            using var cmd = conn.CreateCommand();
            cmd.CommandText = "SELECT l.lineIndex, " + (cols.Contains("charPos") ? "n.charPos" : "NULL") + ", " +
                              (cols.Contains("hasMarker") ? "n.hasMarker" : "0") + ", n.body FROM line_note n JOIN line l ON l.id = n.lineId" +
                              " WHERE l.bookId = $b AND n.body IS NOT NULL ORDER BY l.lineIndex, l.id, n.noteIndex";
            cmd.Parameters.AddWithValue("$b", bookId);
            using var r = cmd.ExecuteReader();
            while (r.Read())
                notes.Add(new VectorBookNote
                {
                    LineIndex = r.GetInt64(0),
                    CharPos = r.IsDBNull(1) ? -1 : r.GetInt64(1),
                    HasMarker = !r.IsDBNull(2) && r.GetInt64(2) != 0,
                    Body = r.GetValue(3) is byte[] b ? Encoding.UTF8.GetString(b) : Convert.ToString(r.GetValue(3), System.Globalization.CultureInfo.InvariantCulture) ?? "",
                });
            return notes;
        }

        /// <summary><c>chunking_identity</c> of <c>&lt;model dir&gt;/model.json</c>, 0 (= the set's) when absent.</summary>
        private static ulong ModelChunkingIdentity(string modelDir)
        {
            string f = Path.Combine(modelDir, "model.json");
            if (!File.Exists(f)) return 0;
            try
            {
                using var doc = JsonDocument.Parse(File.ReadAllText(f, Encoding.UTF8));
                if (doc.RootElement.TryGetProperty("chunking_identity", out var e))
                    return e.ValueKind == JsonValueKind.String ? ulong.Parse(e.GetString()!) : e.GetUInt64();
            }
            catch (Exception ex) when (ex is JsonException || ex is FormatException || ex is InvalidOperationException || ex is OverflowException)
            {
                Console.Error.WriteLine($"model.json: chunking_identity not read ({ex.Message})");
            }
            return 0;
        }

        // ---------------------------------------------------------------- search

        public static int Search(string db, string[] args)
        {
            var pos = Positional(args);
            string? vectors = Program.OptionOf(args, "--vectors");
            if (pos.Count < 1 || vectors == null)
            {
                Console.Error.WriteLine("usage: semantic-search <index dir> --vectors <set dir|file.oxv> --db <seforim.db> --model <dir> " +
                                        "(--line N | --lines a,b,c | --query text) [--k N] [--books a,b] [--threads N] [--hybrid] [--repeat N] [--no-verify] [--chunking-identity N]");
                return 2;
            }
            int k = int.TryParse(Program.OptionOf(args, "--k"), out int kk) && kk > 0 ? kk : 10;
            int threads = int.TryParse(Program.OptionOf(args, "--threads"), out int tt) && tt > 0 ? tt : 0;
            int repeat = int.TryParse(Program.OptionOf(args, "--repeat"), out int rr) && rr > 0 ? rr : 1;
            string? booksArg = Program.OptionOf(args, "--books");
            long[]? books = booksArg == null ? null : booksArg.Split(',').Where(s => s.Trim().Length > 0).Select(s => long.Parse(s.Trim())).ToArray();
            bool hybrid = args.Contains("--hybrid");

            // queries: (source line id or 0, text)
            var queries = new List<(long Line, string Text)>();
            var lineIds = new List<long>();
            string? lineArg = Program.OptionOf(args, "--line");
            string? linesArg = Program.OptionOf(args, "--lines");
            string? queryArg = Program.OptionOf(args, "--query");
            if (lineArg != null) lineIds.Add(long.Parse(lineArg));
            if (linesArg != null) lineIds.AddRange(linesArg.Split(',').Where(s => s.Trim().Length > 0).Select(s => long.Parse(s.Trim())));
            using var conn = SqliteConnectionFactory.OpenCorpusRead(db);
            foreach (long id in lineIds)
            {
                string text = LineText(conn, id);
                if (text.Trim().Length == 0) { Console.WriteLine($"line {id}: no text, skipped"); continue; }
                queries.Add((id, text));
            }
            if (queryArg != null && queryArg.Trim().Length > 0) queries.Add((0, queryArg));
            if (queries.Count == 0) { Console.Error.WriteLine("no query: pass --line, --lines or --query"); return 2; }

            using var index = SeforimIndex.Open(pos[0]);
            var sw = Stopwatch.StartNew();
            var info = index.OpenVectors(vectors);
            Console.WriteLine($"open: {Time(sw.Elapsed)}");
            sw.Restart();
            string modelDir = Program.OptionOf(args, "--model") ?? DefaultModelDir();
            string? ciArg = Program.OptionOf(args, "--chunking-identity");
            ulong chunkingIdentity = ciArg != null ? ulong.Parse(ciArg) : ModelChunkingIdentity(modelDir);
            long skipped = MapBooks(index, conn, vectors, info, !args.Contains("--no-verify"), chunkingIdentity);
            info = index.VectorInfo()!;
            Console.WriteLine($"mapping + verify: {Time(sw.Elapsed)}, books mapped {info.BooksMapped:N0}, unmapped {info.BooksUnmapped:N0} (skipped {skipped:N0})");
            Console.WriteLine($"set: generation {info.Generation}, dims {info.Dims}, codec {info.Codec}, library {info.LibraryReleaseTag}, " +
                              $"slots_live {info.SlotsLive:N0}, slots_dead {info.SlotsDead:N0}, books {info.Books:N0}, " +
                              $"{info.BytesOnDisk / 1048576.0:N0} MB on disk, {info.Segments.Count} segments" + (info.RecoveredFromPrevious ? " (recovered)" : ""));
            foreach (var s in info.Segments)
                Console.WriteLine($"  segment {Path.GetFileName(s.File)}: {s.Kind}, v{s.FromLibraryVersion}-{s.ToLibraryVersion}, slots {s.Slots:N0}, " +
                                  $"dead {s.SlotsDead:N0}, foreign {s.Foreign:N0} (unresolved {s.ForeignUnresolved:N0}), {s.Size / 1048576.0:N1} MB");

            sw.Restart();
            using var embedder = new MeivinEmbedder(modelDir);
            Console.WriteLine($"model load: {Time(sw.Elapsed)}");
            var latencies = new List<double>();
            int withLine = 0, selfFirst = 0;
            var options = new SemanticOptions { K = k, Books = books, Threads = threads };
            foreach (var (line, text) in queries)
            {
                sw.Restart();
                long[] ids = embedder.QueryIds(text);
                float[] vector = embedder.Embed(ids);
                double embedMs = sw.Elapsed.TotalMilliseconds;
                SemanticResult sem = null!;
                double last = 0;
                for (int i = 0; i < repeat; i++)
                {
                    sw.Restart();
                    sem = index.SemanticSearch(vector, options);
                    last = sw.Elapsed.TotalMilliseconds;
                    latencies.Add(last);
                }
                string label = line != 0 ? $"line {line}" : "query";
                Console.WriteLine($"{label}: embed {embedMs:F0} ms ({ids.Length} tokens), search {last:F1} ms; vectors {sem.Vectors:N0}, " +
                                  $"records {sem.Records:N0}, unmapped {sem.Unmapped:N0}, scanned {sem.Scanned:N0}, threads {sem.Threads}");
                for (int i = 0; i < sem.Hits.Length; i++)
                {
                    var h = sem.Hits[i];
                    Console.WriteLine($"  {i + 1,3}. line {h.LineId,10}  book {h.BookId,6}  score {h.Score:F4}  chunk {h.Chunk & ~SemanticHit.Borrowed}" +
                                      ((h.Chunk & SemanticHit.Borrowed) != 0 ? " (alias)" : "") + (line != 0 && h.LineId == line ? "   <- source line" : ""));
                }
                if (line != 0)
                {
                    withLine++;
                    bool first = sem.Hits.Length > 0 && sem.Hits[0].LineId == line;
                    if (first) selfFirst++;
                    Console.WriteLine($"  source line first: {(first ? "yes" : "no")}");
                    if (!first) ExplainMiss(index, conn, vector, line, text, sem, threads);
                }
                if (hybrid)
                {
                    sw.Restart();
                    var so = new SearchOptions(text) { Order = ResultsOrder.Relevance };
                    if (books != null) so.Books = books.ToList();
                    using var session = index.Search(so);
                    var fused = session.FuseSemantic(text, sem);
                    Console.WriteLine($"  hybrid: {session.HitCount:N0} lexical hits, {fused.Length:N0} fused in {Time(sw.Elapsed)}");
                    foreach (var f in fused.Take(k))
                        Console.WriteLine($"       line {f.LineId,10}  book {f.BookId,6}  fused {f.Fused:F4}  lexical {f.Lexical?.ToString("F4") ?? "-"}  " +
                                          $"semantic {f.Semantic?.ToString("F4") ?? "-"}  {f.Source}");
                }
            }
            var sorted = latencies.OrderBy(x => x).ToList();
            double p95 = sorted[Math.Max(0, Math.Min(sorted.Count - 1, (int)Math.Ceiling(0.95 * sorted.Count) - 1))];
            Console.WriteLine($"search latency over {sorted.Count} runs: mean {sorted.Average():F1} ms, p95 {p95:F1} ms");
            if (withLine > 0) Console.WriteLine($"source line ranked first: {selfFirst} of {withLine}");
            Console.WriteLine($"peak working set: {Process.GetCurrentProcess().PeakWorkingSet64 / 1048576.0:N0} MB");
            return 0;
        }

        /// <summary>Why a line's own text did not rank it first: the query length, the own line's
        /// rank in the top 100 (all books) and in its own book's top 100 (absent there: its
        /// record failed the text check or the line has no vector), the top hit's book / line
        /// and whether its text equals the query (a tie: identical text elsewhere).</summary>
        private static void ExplainMiss(SeforimIndex index, SqliteConnection conn, float[] vector, long line, string text,
                                        SemanticResult sem, int threads)
        {
            static string Norm(string t) => string.Join(" ", t.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
            long ownBook;
            using (var cmd = conn.CreateCommand())
            {
                cmd.CommandText = "SELECT bookId FROM line WHERE id = $id";
                cmd.Parameters.AddWithValue("$id", line);
                ownBook = Convert.ToInt64(cmd.ExecuteScalar() ?? 0L);
            }
            int RankIn(SemanticResult r) => Array.FindIndex(r.Hits, h => h.LineId == line) + 1;
            var all = index.SemanticSearch(vector, new SemanticOptions { K = 100, Threads = threads });
            var own = index.SemanticSearch(vector, new SemanticOptions { K = 100, Threads = threads, Books = new[] { ownBook } });
            string query = Norm(text);
            var top = sem.Hits.Length > 0 ? sem.Hits[0] : null;
            bool same = top != null && Norm(LineText(conn, top.LineId)) == query;
            int rankAll = RankIn(all), rankOwn = RankIn(own);
            string verdict = same ? "tie (identical text elsewhere)"
                : rankOwn == 0 ? "own line has no verified vector (text check failed or not embedded)"
                : query.Length < 20 ? "short line (Otzaria embeds it with neighbour context)"
                : query.Length > 512 ? "long line (windows; the query is the whole line)"
                : "ranked below another text";
            Console.WriteLine($"  miss: query {query.Length} chars; own rank {(rankAll > 0 ? rankAll.ToString() : ">100")} of all, " +
                              $"{(rankOwn > 0 ? rankOwn.ToString() : "absent")} in own book {ownBook}; top hit book {top?.BookId} line {top?.LineId} " +
                              $"same text {(same ? "yes" : "no")}; verdict: {verdict}");
        }

        private static string LineText(SqliteConnection conn, long lineId)
        {
            var schema = SeforimDbSchema.Probe(conn);
            using var codec = SeforimDbLineContentCodec.Create(conn);
            using var cmd = conn.CreateCommand();
            cmd.CommandText = "SELECT " + SeforimDbLineContentSql.RawContentExpression(schema, "l") + " FROM line l" +
                              SeforimDbLineContentSql.ContentJoin(schema, "l") + " WHERE l.id = $id";
            cmd.Parameters.AddWithValue("$id", lineId);
            using var r = cmd.ExecuteReader();
            return r.Read() ? StripTags(codec.DecodeColumn(r, 0, lineId) ?? "") : "";
        }

        private static string StripTags(string s)
        {
            var sb = new StringBuilder(s.Length);
            bool inTag = false;
            foreach (char c in s)
            {
                if (c == '<') inTag = true;
                else if (c == '>') { inTag = false; sb.Append(' '); }
                else if (!inTag) sb.Append(c);
            }
            return sb.ToString();
        }

        // ---------------------------------------------------------------- tokenizer check

        public static int TokCheck(string[] args)
        {
            var pos = Positional(args);
            if (pos.Count < 1) { Console.Error.WriteLine("usage: semantic-tokcheck <golden.json> [--model dir]"); return 2; }
            var tok = UnigramTokenizer.Load(Path.Combine(Program.OptionOf(args, "--model") ?? DefaultModelDir(), "tokenizer.json"));
            using var doc = JsonDocument.Parse(File.ReadAllText(pos[0], Encoding.UTF8));
            int max = doc.RootElement.GetProperty("header").TryGetProperty("max_tokens", out var mt) ? mt.GetInt32() : 0;
            int ok = 0, bad = 0;
            foreach (var c in doc.RootElement.GetProperty("cases").EnumerateArray())
            {
                string name = c.GetProperty("name").GetString()!;
                string input = c.GetProperty("input").GetString()!;
                long[] want = c.GetProperty("token_ids").EnumerateArray().Select(e => e.GetInt64()).ToArray();
                long[] got = tok.Encode(input, max);
                if (got.SequenceEqual(want)) { ok++; continue; }
                bad++;
                int at = 0;
                while (at < Math.Min(got.Length, want.Length) && got[at] == want[at]) at++;
                Console.WriteLine($"DIFF {name}: got {got.Length} ids, want {want.Length}; first diff at {at}: " +
                                  $"got [{string.Join(",", got.Skip(at).Take(6))}] want [{string.Join(",", want.Skip(at).Take(6))}]");
            }
            Console.WriteLine($"tokenizer vs golden: {ok} identical, {bad} different");
            return bad == 0 ? 0 : 1;
        }
    }
}
