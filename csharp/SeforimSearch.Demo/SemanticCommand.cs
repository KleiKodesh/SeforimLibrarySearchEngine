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
    /// Semantic search in the host (issue #18). The engine only stores and searches vectors; the
    /// seforim.db mapping and the query model live here.
    ///
    ///   semantic-import &lt;index dir&gt; &lt;file.oxv | file.oxv.zst&gt; [--db path] [--books N] [--max-lines N]
    ///                   [--book-ids 1,2,3] [--batch 200] [--no-newline-check]
    ///     Maps each file book named id:&lt;bookId&gt; to its line ids (ordinal = row of
    ///     SELECT id FROM line WHERE bookId=? ORDER BY lineIndex, id) and imports it.
    ///     --books N: the first N books of the file; --max-lines N: only books that lie whole in
    ///     the first N lines of the catalog order (the books a `build --max-lines N` indexed).
    ///   semantic-search &lt;index dir&gt; [query...] [--db path] [--line lineId] [--k 10] [--hybrid] [--model dir]
    ///     Embeds the query (or the text of line --line) with Otzaria's Meivin model and searches.
    ///   semantic-tokcheck &lt;golden.json&gt; [--model dir]
    ///     Compares the tokenizer with Otzaria's golden token ids (otzaria-semantic-search
    ///     tests/data/onnx_golden_vectors*.json, same tokenizer.json checksum).
    /// </summary>
    internal static class SemanticCommand
    {
        private static readonly string[] ValueOptions = { "--db", "--index", "--books", "--max-lines", "--book-ids", "--batch", "--line", "--lines", "--k", "--model" };

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

        // ---------------------------------------------------------------- import

        public static int Import(string db, string[] args)
        {
            var pos = Positional(args);
            if (pos.Count < 2) { Console.Error.WriteLine("usage: semantic-import <index dir> <file.oxv|file.oxv.zst> [--books N] [--max-lines N] [--book-ids a,b] [--batch 200]"); return 2; }
            string indexDir = pos[0], oxv = pos[1];
            var total = Stopwatch.StartNew();
            if (oxv.EndsWith(".zst", StringComparison.OrdinalIgnoreCase))
            {
                string plain = oxv.Substring(0, oxv.Length - 4);
                if (!File.Exists(plain))
                {
                    var esw = Stopwatch.StartNew();
                    long bytes = SeforimSemantic.ExpandOxv(oxv, plain);
                    Console.Error.WriteLine($"expanded {bytes / 1e6:F1} MB in {Time(esw.Elapsed)}");
                }
                oxv = plain;
            }
            var info = SeforimSemantic.ReadOxvInfo(oxv);
            Console.Error.WriteLine($"oxv: format {info.Format}, {info.Kind}, {info.Dims} dims, {info.Codec}, library {info.LibraryReleaseTag} " +
                                    $"(v{info.FromLibraryVersion}-{info.ToLibraryVersion}), {info.Books.Count} books, {info.Slots:N0} slots");
            IEnumerable<OxvBookInfo> books = info.Books;
            if (int.TryParse(Program.OptionOf(args, "--books"), out int nBooks) && nBooks > 0) books = books.Take(nBooks);
            HashSet<long>? only = null;
            string? idList = Program.OptionOf(args, "--book-ids");
            if (idList != null) only = new HashSet<long>(idList.Split(',').Select(long.Parse));
            bool checkNewlines = !args.Contains("--no-newline-check");
            int batchSize = int.TryParse(Program.OptionOf(args, "--batch"), out int bs) && bs > 0 ? bs : 200;

            using var conn = SqliteConnectionFactory.OpenCorpusRead(db);
            if (long.TryParse(Program.OptionOf(args, "--max-lines"), out long maxLines) && maxLines > 0)
            {
                var whole = BooksWithinFirstLines(conn, maxLines);
                only = only == null ? whole : new HashSet<long>(only.Where(whole.Contains));
                Console.Error.WriteLine($"--max-lines {maxLines:N0}: {whole.Count} books lie whole in the catalog's first lines");
            }
            if (only != null) books = books.Where(b => TryBookId(b.Name, out long id) && only.Contains(id));

            var schema = SeforimDbSchema.Probe(conn);
            using var codec = SeforimDbLineContentCodec.Create(conn);
            using var index = SeforimIndex.Open(indexDir);

            long notIdName = 0, missing = 0, tooFew = 0, splitRows = 0, mapped = 0, dbRowsMore = 0;
            var sum = new OxvImportReport();
            var batch = new List<OxvBookMap>();
            var mapTime = new Stopwatch();
            var importTime = new Stopwatch();
            void Flush()
            {
                if (batch.Count == 0) return;
                importTime.Start();
                var r = index.ImportOxv(oxv, batch);
                importTime.Stop();
                sum.BooksImported += r.BooksImported;
                sum.BooksUnmapped += r.BooksUnmapped;
                sum.BooksNotInFile.AddRange(r.BooksNotInFile);
                sum.Records += r.Records;
                sum.Vectors += r.Vectors;
                sum.Lines += r.Lines;
                sum.OutOfRange += r.OutOfRange;
                sum.Duplicates += r.Duplicates;
                sum.Generation = r.Generation;
                batch.Clear();
                Console.Error.WriteLine($"  imported {sum.BooksImported:N0} books, {sum.Vectors:N0} vectors ({Time(total.Elapsed)})");
            }
            foreach (var b in books)
            {
                if (!TryBookId(b.Name, out long bookId)) { notIdName++; continue; }
                mapTime.Start();
                var (lineIds, hasNewline) = ReadBookLines(conn, schema, codec, bookId, checkNewlines);
                mapTime.Stop();
                if (lineIds.Count == 0) { missing++; continue; }
                if (b.Lines > lineIds.Count) { tooFew++; continue; }
                if (hasNewline) { splitRows++; continue; }
                if (lineIds.Count > b.Lines) dbRowsMore++;
                mapped++;
                batch.Add(new OxvBookMap { Name = b.Name, BookId = bookId, LineIds = lineIds });
                if (batch.Count >= batchSize) Flush();
            }
            Flush();
            Console.WriteLine($"books mapped: {mapped:N0} (of them {dbRowsMore:N0} with more DB rows than the file's lines)");
            Console.WriteLine($"books skipped: name not id:N {notIdName:N0}, missing from DB {missingOr(missing)}, " +
                              $"file lines > DB rows {tooFew:N0}, a row with a newline {splitRows:N0}" + (checkNewlines ? "" : " (not checked)"));
            Console.WriteLine($"import: books {sum.BooksImported:N0}, unmapped {sum.BooksUnmapped:N0}, not in file {sum.BooksNotInFile.Count:N0}, " +
                              $"records {sum.Records:N0}, vectors {sum.Vectors:N0}, lines {sum.Lines:N0}, out_of_range {sum.OutOfRange:N0}, " +
                              $"duplicates {sum.Duplicates:N0}, generation {sum.Generation}");
            Console.WriteLine($"time: total {Time(total.Elapsed)}, mapping (SQLite) {Time(mapTime.Elapsed)}, import {Time(importTime.Elapsed)}");
            Console.WriteLine($"peak working set: {System.Diagnostics.Process.GetCurrentProcess().PeakWorkingSet64 / 1048576.0:N0} MB");
            return 0;

            static string missingOr(long n) => n.ToString("N0");
        }

        private static bool TryBookId(string name, out long id)
        {
            id = 0;
            return name.StartsWith("id:", StringComparison.Ordinal) && long.TryParse(name.Substring(3), out id) && id > 0;
        }

        /// <summary>The book's line ids by 0-based ordinal (Otzaria's order), and whether a row holds
        /// a newline (Otzaria splits such rows when it embeds, so the file's ordinals would shift).</summary>
        private static (List<long> Ids, bool HasNewline) ReadBookLines(SqliteConnection conn, SeforimDbSchema schema,
                                                                        SeforimDbLineContentCodec codec, long bookId, bool checkNewlines)
        {
            var ids = new List<long>();
            bool newline = false;
            using var cmd = conn.CreateCommand();
            cmd.CommandText = checkNewlines
                ? "SELECT l.id, " + SeforimDbLineContentSql.RawContentExpression(schema, "l") + " FROM line l" +
                  SeforimDbLineContentSql.ContentJoin(schema, "l") + " WHERE l.bookId = $b ORDER BY l.lineIndex, l.id"
                : "SELECT id FROM line WHERE bookId = $b ORDER BY lineIndex, id";
            cmd.Parameters.AddWithValue("$b", bookId);
            using var r = cmd.ExecuteReader();
            while (r.Read())
            {
                long id = r.GetInt64(0);
                ids.Add(id);
                if (checkNewlines && !newline)
                {
                    string? text = codec.DecodeColumn(r, 1, id);
                    if (text != null && text.IndexOf('\n') >= 0) newline = true;
                }
            }
            return (ids, newline);
        }

        /// <summary>Books lying whole in the first <paramref name="maxLines"/> lines of the catalog order
        /// (what a <c>build --max-lines</c> indexed; virtual notes books are not counted, so with notes
        /// the last book or two may be only partly indexed).</summary>
        private static HashSet<long> BooksWithinFirstLines(SqliteConnection conn, long maxLines)
        {
            var order = SeforimDbBookTreeOrder.Load(conn);
            var counts = new Dictionary<long, long>();
            using (var cmd = conn.CreateCommand())
            {
                cmd.CommandText = "SELECT bookId, COUNT(*) FROM line GROUP BY bookId";
                using var r = cmd.ExecuteReader();
                while (r.Read()) counts[r.GetInt64(0)] = r.GetInt64(1);
            }
            var set = new HashSet<long>();
            long acc = 0;
            foreach (var kv in order.OrderBy(kv => kv.Value))
            {
                acc += counts.TryGetValue(kv.Key, out long c) ? c : 0;
                if (acc > maxLines) break;
                set.Add(kv.Key);
            }
            return set;
        }

        // ---------------------------------------------------------------- search

        public static int Search(string db, string[] args)
        {
            // --lines a,b,c: one model load, each line's text as a query, with a recall check of the
            // graph against the exact scan (CI measurements)
            string? many = Program.OptionOf(args, "--lines");
            if (many != null) return SearchMany(db, args, many);
            var pos = Positional(args);
            if (pos.Count < 1) { Console.Error.WriteLine("usage: semantic-search <index dir> [query...] [--line lineId] [--k 10] [--hybrid]"); return 2; }
            string indexDir = pos[0];
            string query = string.Join(" ", pos.Skip(1));
            if (long.TryParse(Program.OptionOf(args, "--line"), out long lineId))
            {
                using var conn = SqliteConnectionFactory.OpenCorpusRead(db);
                var schema = SeforimDbSchema.Probe(conn);
                using var codec = SeforimDbLineContentCodec.Create(conn);
                using var cmd = conn.CreateCommand();
                cmd.CommandText = "SELECT " + SeforimDbLineContentSql.RawContentExpression(schema, "l") + " FROM line l" +
                                  SeforimDbLineContentSql.ContentJoin(schema, "l") + " WHERE l.id = $id";
                cmd.Parameters.AddWithValue("$id", lineId);
                using var r = cmd.ExecuteReader();
                if (!r.Read()) { Console.Error.WriteLine("line not found"); return 2; }
                query = StripTags(codec.DecodeColumn(r, 0, lineId) ?? "");
                Console.WriteLine($"query = text of line {lineId} ({query.Length} chars)");
            }
            if (query.Trim().Length == 0) { Console.Error.WriteLine("empty query"); return 2; }
            int k = int.TryParse(Program.OptionOf(args, "--k"), out int kk) && kk > 0 ? kk : 10;
            string modelDir = Program.OptionOf(args, "--model") ?? DefaultModelDir();

            var sw = Stopwatch.StartNew();
            using var embedder = new MeivinEmbedder(modelDir);
            var load = sw.Elapsed;
            sw.Restart();
            long[] ids = embedder.QueryIds(query);
            float[] vector = embedder.Embed(ids);
            var embed = sw.Elapsed;
            using var index = SeforimIndex.Open(indexDir);
            sw.Restart();
            var sem = index.SemanticSearch(vector, new SemanticOptions { K = k });
            var search = sw.Elapsed;
            Console.WriteLine($"model load {Time(load)}, embed {Time(embed)} ({ids.Length} tokens), semantic search {Time(search)} " +
                              $"(exact={sem.Exact}, allowed {sem.Allowed:N0})");
            for (int i = 0; i < sem.Hits.Length; i++)
            {
                var h = sem.Hits[i];
                Console.WriteLine($"{i + 1,3}. line {h.LineId,10}  book {h.BookId,6}  chunk {h.Chunk,3}  score {h.Score:F4}" +
                                  (h.LineId == lineId ? "   <- the query line" : ""));
            }
            if (args.Contains("--hybrid"))
            {
                sw.Restart();
                using var session = index.Search(new SearchOptions(query) { Order = ResultsOrder.Relevance });
                var fused = session.FuseSemantic(query, sem);
                Console.WriteLine($"hybrid: {session.HitCount:N0} lexical hits, {fused.Length:N0} fused in {Time(sw.Elapsed)}");
                foreach (var f in fused.Take(k))
                    Console.WriteLine($"     line {f.LineId,10}  book {f.BookId,6}  fused {f.Fused:F4}  lexical {f.Lexical?.ToString("F4") ?? "-"}  " +
                                      $"semantic {f.Semantic?.ToString("F4") ?? "-"}  {f.Source}");
            }
            return 0;
        }

        private static string LineText(string db, long lineId)
        {
            using var conn = SqliteConnectionFactory.OpenCorpusRead(db);
            var schema = SeforimDbSchema.Probe(conn);
            using var codec = SeforimDbLineContentCodec.Create(conn);
            using var cmd = conn.CreateCommand();
            cmd.CommandText = "SELECT " + SeforimDbLineContentSql.RawContentExpression(schema, "l") + " FROM line l" +
                              SeforimDbLineContentSql.ContentJoin(schema, "l") + " WHERE l.id = $id";
            cmd.Parameters.AddWithValue("$id", lineId);
            using var r = cmd.ExecuteReader();
            return r.Read() ? StripTags(codec.DecodeColumn(r, 0, lineId) ?? "") : "";
        }

        private static int SearchMany(string db, string[] args, string many)
        {
            var pos = Positional(args);
            if (pos.Count < 1) { Console.Error.WriteLine("usage: semantic-search <index dir> --lines a,b,c [--k 10]"); return 2; }
            int k = int.TryParse(Program.OptionOf(args, "--k"), out int kk) && kk > 0 ? kk : 10;
            using var embedder = new MeivinEmbedder(Program.OptionOf(args, "--model") ?? DefaultModelDir());
            using var index = SeforimIndex.Open(pos[0]);
            int queries = 0, self1 = 0, selfK = 0;
            double recall = 0, graphMs = 0, exactMs = 0, embedMs = 0;
            foreach (var part in many.Split(','))
            {
                if (!long.TryParse(part.Trim(), out long lineId)) continue;
                string text = LineText(db, lineId);
                if (text.Trim().Length == 0) continue;
                var sw = Stopwatch.StartNew();
                float[] vector = embedder.Embed(embedder.QueryIds(text));
                embedMs += sw.Elapsed.TotalMilliseconds;
                sw.Restart();
                var graph = index.SemanticSearch(vector, new SemanticOptions { K = k, ExactThreshold = 0 });
                graphMs += sw.Elapsed.TotalMilliseconds;
                sw.Restart();
                var exact = index.SemanticSearch(vector, new SemanticOptions { K = k, ExactThreshold = long.MaxValue });
                exactMs += sw.Elapsed.TotalMilliseconds;
                var want = new HashSet<long>(exact.Hits.Select(h => h.LineId));
                double r = want.Count == 0 ? 1 : graph.Hits.Count(h => want.Contains(h.LineId)) / (double)want.Count;
                int rank = Array.FindIndex(graph.Hits, h => h.LineId == lineId) + 1;
                queries++;
                recall += r;
                if (rank == 1) self1++;
                if (rank >= 1) selfK++;
                Console.WriteLine($"line {lineId,10}: own rank {(rank == 0 ? "-" : rank.ToString())}, top score {(graph.Hits.Length > 0 ? graph.Hits[0].Score : 0):F4}, " +
                                  $"recall@{k} {r:F2}");
            }
            if (queries == 0) { Console.Error.WriteLine("no query lines"); return 2; }
            Console.WriteLine($"queries {queries}: own line rank 1 in {self1}, in top {k} in {selfK}; mean recall@{k} graph vs exact {recall / queries:F3}; " +
                              $"mean embed {embedMs / queries:F0} ms, graph search {graphMs / queries:F0} ms, exact scan {exactMs / queries:F0} ms");
            return 0;
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
