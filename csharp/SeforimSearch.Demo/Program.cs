using System;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using SeforimSearch.Demo.SeforimDbFullTextSearch;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// Demo console:
    ///   build  [--db path] [--index dir] [--max-lines N] [--no-positions]
    ///   search [--db path] [--index dir] [--prefixes] [--suffixes] [--distance N] query...
    /// The seforim.db path defaults to KitveiHakodesh's resolution order
    /// (registry choice, then Otzaria, then Zayit).
    /// </summary>
    internal static class Program
    {
        private static int Main(string[] args)
        {
            Console.OutputEncoding = Encoding.UTF8;
            if (args.Length == 0)
            {
                Console.Error.WriteLine("usage: build|search [--db path] [--index dir] ...");
                return 2;
            }
            string command = args[0];
            string? db = Option(args, "--db") ?? ResolveSeforimDb();
            string index = Option(args, "--index") ?? Path.Combine(AppContext.BaseDirectory, "TantivyIndex");
            if (db == null || !File.Exists(db))
            {
                Console.Error.WriteLine("seforim.db not found; pass --db");
                return 2;
            }
            Console.Error.WriteLine($"db: {db}\nindex: {index}\nengine: {SeforimIndex.NativeVersion}  runtime: {System.Runtime.InteropServices.RuntimeInformation.FrameworkDescription}");

            using var engine = new SeforimDbFtsEngine(index, db);
            if (command == "build") return Build(engine, args);
            if (command == "search") return Search(engine, args);
            if (command == "dump") return Dump(engine, args);
            Console.Error.WriteLine("unknown command " + command);
            return 2;
        }

        private static int Build(SeforimDbFtsEngine engine, string[] args)
        {
            long maxLines = long.TryParse(Option(args, "--max-lines"), out var m) ? m : 0;
            var options = new IndexOptions { Positions = !args.Contains("--no-positions") };
            var sw = Stopwatch.StartNew();
            long lastShown = 0;
            engine.Index.BuildLines((done, total) =>
            {
                if (done - lastShown >= 100_000 || done == total)
                {
                    Console.Error.WriteLine($"  indexed {done:N0} / {total:N0}  ({sw.Elapsed.TotalSeconds:F1}s)");
                    lastShown = done;
                }
            }, indexOptions: options, maxLines: maxLines);
            var state = engine.Index.ReadBuildState();
            Console.Error.WriteLine($"build: {state?.LinesDone:N0} lines, complete={state?.Complete} in {sw.Elapsed.TotalSeconds:F1}s");
            return 0;
        }

        private static int Search(SeforimDbFtsEngine engine, string[] args)
        {
            var words = args.Skip(1).Where((a, i) => !a.StartsWith("--", StringComparison.Ordinal)
                && (i == 0 || !new[] { "--db", "--index", "--distance" }.Contains(args[i]))).ToArray();
            string query = string.Join(" ", words);
            int distance = int.TryParse(Option(args, "--distance"), out var d) ? d : int.MaxValue;
            var wordOptions = new WalkOptions
            {
                IgnoreQuotes = !args.Contains("--exact-quotes"),
                GrammarPrefixes = args.Contains("--prefixes"),
                GrammarSuffixes = args.Contains("--suffixes"),
            };

            var terms = new FtsTermTable();
            var sw = Stopwatch.StartNew();
            TimeSpan? first = null;
            int total = 0, shown = 0;
            foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                         engine, query, distance, requireOrdered: false, contextWords: 8, wordOptions, terms))
            {
                first ??= sw.Elapsed;
                total += batch.Hits.Count;
                foreach (var hit in batch.Hits)
                {
                    if (shown >= 10) break;
                    shown++;
                    string joined = string.Join(" ", hit.SnippetLineIds
                        .Where(batch.LineTexts.ContainsKey).Select(id => batch.LineTexts[id]));
                    Console.WriteLine($"line {hit.LineId} book {hit.BookId} toc {hit.TocEntryId} dist {hit.WordDistance}: " +
                                      Render(joined, hit));
                }
            }
            Console.Error.WriteLine($"search: {total:N0} hits in {sw.Elapsed.TotalMilliseconds:F0} ms " +
                                    $"(first batch {first?.TotalMilliseconds:F0} ms); terms: {string.Join(", ", terms.All())}");
            return 0;
        }

        /// <summary>
        /// Parity dump (same format as tools/parity/FtsLibDump): for each query in
        /// --queries (one per line, optional "\t&lt;maxWordDistance&gt;"), writes
        /// --out/&lt;n&gt;.jsonl with a header line and one JSON line per hit.
        /// </summary>
        private static int Dump(SeforimDbFtsEngine engine, string[] args)
        {
            string queries = Option(args, "--queries") ?? throw new ArgumentException("--queries");
            string outDir = Option(args, "--out") ?? throw new ArgumentException("--out");
            int maxHits = int.TryParse(Option(args, "--max-hits"), out var mh) ? mh : 2000;
            Directory.CreateDirectory(outDir);
            int n = 0;
            foreach (var raw in File.ReadAllLines(queries, Encoding.UTF8))
            {
                if (string.IsNullOrWhiteSpace(raw) || raw.StartsWith("#", StringComparison.Ordinal)) continue;
                var parts = raw.Split('\t');
                string query = parts[0];
                int distance = parts.Length > 1 ? int.Parse(parts[1]) : int.MaxValue;
                var terms = new FtsTermTable();
                var sw = Stopwatch.StartNew();
                int count = 0;
                using (var w = new StreamWriter(Path.Combine(outDir, n + ".jsonl"), false, new UTF8Encoding(false)))
                {
                    w.WriteLine("{\"query\":" + JsonStr(query) + ",\"distance\":" + distance + "}");
                    foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                                 engine, query, distance, false, SeforimDbFtsSearcher.DefaultContextWords, null, terms))
                    {
                        var all = terms.All();
                        foreach (var h in batch.Hits)
                        {
                            if (count++ >= maxHits) break;
                            var sb = new StringBuilder();
                            sb.Append("{\"line\":").Append(h.LineId).Append(",\"book\":").Append(h.BookId)
                              .Append(",\"toc\":").Append(h.TocEntryId).Append(",\"dist\":").Append(h.WordDistance)
                              .Append(",\"start\":").Append(h.SnippetStart).Append(",\"end\":").Append(h.SnippetEnd)
                              .Append(",\"flags\":").Append(h.SnippetFlags)
                              .Append(",\"lines\":[").Append(string.Join(",", h.SnippetLineIds)).Append("],\"marks\":[");
                            for (int i = 0; i + 2 < h.SnippetMarks.Length; i += 3)
                            {
                                if (i > 0) sb.Append(',');
                                sb.Append('[').Append(h.SnippetMarks[i]).Append(',').Append(h.SnippetMarks[i + 1]).Append(',')
                                  .Append(JsonStr(all[h.SnippetMarks[i + 2]])).Append(']');
                            }
                            w.WriteLine(sb.Append("]}").ToString());
                        }
                        if (count >= maxHits) break;
                    }
                }
                Console.Error.WriteLine($"[tantivy] #{n} \"{query}\" dist={distance}: {count} hits in {sw.ElapsedMilliseconds} ms");
                n++;
            }
            return 0;
        }

        private static string JsonStr(string s)
        {
            var sb = new StringBuilder("\"");
            foreach (char c in s)
            {
                if (c == '"') sb.Append("\\\"");
                else if (c == '\\') sb.Append("\\\\");
                else if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
                else sb.Append(c);
            }
            return sb.Append('"').ToString();
        }

        /// <summary>Plain-text rendering of a descriptor: tags stripped, marks in [ ].</summary>
        private static string Render(string text, FtsHit hit)
        {
            var sb = new StringBuilder();
            if ((hit.SnippetFlags & FtsHit.SnippetFlagLeadingEllipsis) != 0) sb.Append('…');
            int pos = hit.SnippetStart, end = Math.Min(hit.SnippetEnd, text.Length);
            for (int i = 0; i + 2 < hit.SnippetMarks.Length; i += 3)
            {
                int s = hit.SnippetMarks[i], l = hit.SnippetMarks[i + 1];
                if (s < pos || s >= end) continue;
                sb.Append(StripTags(text.Substring(pos, s - pos))).Append('[')
                  .Append(StripTags(text.Substring(s, Math.Min(l, end - s)))).Append(']');
                pos = Math.Min(s + l, end);
            }
            if (pos < end) sb.Append(StripTags(text.Substring(pos, end - pos)));
            if ((hit.SnippetFlags & FtsHit.SnippetFlagTrailingEllipsis) != 0) sb.Append('…');
            return sb.ToString();
        }

        private static string StripTags(string s)
        {
            var sb = new StringBuilder(s.Length);
            bool inTag = false;
            foreach (char c in s)
            {
                if (c == '<') inTag = true;
                else if (c == '>') inTag = false;
                else if (!inTag) sb.Append(c);
            }
            return sb.ToString();
        }

        private static string? Option(string[] args, string name)
        {
            int i = Array.IndexOf(args, name);
            return i >= 0 && i + 1 < args.Length ? args[i + 1] : null;
        }

        /// <summary>KitveiHakodesh's order: Otzaria, then Zayit (registry choice omitted
        /// to stay cross-platform; pass --db to override).</summary>
        private static string? ResolveSeforimDb()
        {
            string appData = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
            foreach (var p in new[]
                     {
                         Path.Combine(appData, "otzaria", "books", "seforim.db"),
                         Path.Combine(appData, "io.github.kdroidfilter.seforimapp", "databases", "seforim.db"),
                     })
                if (File.Exists(p)) return p;
            return null;
        }
    }
}
