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
    ///   build  [--db path] [--index dir] [--max-lines N] [--no-positions] [--keep-marks]
    ///   search [--db path] [--index dir] [--prefixes] [--suffixes] [--distance N] query...
    ///   selftest [--dir path]   (C# corpus-pool and end-to-end tests on a small fixture)
    ///   count  [--db path] [--index dir] [--prefixes] [--suffixes] [--distance N] [--ordered] [--regex] [--order catalog|relevance|generation] [--cancel-after MS] query...
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
            if (command == "selftest") return SelfTest.Run(args);
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
            if (command == "count") return Count(engine, args);
            Console.Error.WriteLine("unknown command " + command);
            return 2;
        }

        private static int Build(SeforimDbFtsEngine engine, string[] args)
        {
            long maxLines = long.TryParse(Option(args, "--max-lines"), out var m) ? m : 0;
            var options = new IndexOptions { Positions = !args.Contains("--no-positions"), KeepMarks = args.Contains("--keep-marks") };
            var sw = Stopwatch.StartNew();
            long lastShown = 0;
            var stages = new List<(string Name, TimeSpan Time)>();
            engine.Index.BuildLines((done, total) =>
            {
                if (done - lastShown >= 100_000 || done == total)
                {
                    Console.Error.WriteLine($"  indexed {done:N0} / {total:N0}  ({sw.Elapsed.TotalSeconds:F1}s)");
                    lastShown = done;
                }
            }, indexOptions: options, maxLines: maxLines, onStage: (name, t) =>
            {
                stages.Add((name, t));
                Console.Error.WriteLine($"  stage: {name}: {t.TotalSeconds:F1}s");
            });
            var state = engine.Index.ReadBuildState();
            Console.Error.WriteLine($"build: {state?.LinesDone:N0} lines, complete={state?.Complete} in {sw.Elapsed.TotalSeconds:F1}s");
            string? dir = Option(args, "--index");
            if (dir != null && Directory.Exists(dir))
            {
                var files = new DirectoryInfo(dir).GetFiles();
                long total = files.Sum(f => f.Length);
                Console.Error.WriteLine($"index size: {total / 1e6:F1} MB in {files.Length} files");
                foreach (var g in files.GroupBy(f => f.Extension).OrderByDescending(g => g.Sum(f => f.Length)))
                    Console.Error.WriteLine($"  {g.Key,-10} {g.Sum(f => f.Length) / 1e6,9:F1} MB");
            }
            return 0;
        }

        private static int Search(SeforimDbFtsEngine engine, string[] args)
        {
            var words = args.Skip(1).Where((a, i) => !a.StartsWith("--", StringComparison.Ordinal)
                && (i == 0 || !new[] { "--db", "--index", "--distance", "--repeat" }.Contains(args[i]))).ToArray();
            string query = string.Join(" ", words);
            int distance = int.TryParse(Option(args, "--distance"), out var d) ? d : int.MaxValue;
            var wordOptions = new WalkOptions
            {
                IgnoreQuotes = !args.Contains("--exact-quotes"),
                ExactQuotes = args.Contains("--exact-quotes"),
                GrammarPrefixes = args.Contains("--prefixes"),
                GrammarSuffixes = args.Contains("--suffixes"),
            };

            // First batch = from the search call to the first page (256 hits with line text
            // and snippet) in the caller's hands, the index already open (the same definition
            // as FtsLibDump search and the Otzaria driver; see tools/report/search_report.py).
            // The seforim.db connection is opened (pooled) inside the first search.
            // --repeat N: N searches in this process (search 1 = cold, then warm).
            var openSw = Stopwatch.StartNew();
            bool built = engine.Index.IsBuilt;
            Console.Error.WriteLine($"open: index {openSw.Elapsed.TotalMilliseconds:F0} ms (built={built})");
            int repeat = int.TryParse(Option(args, "--repeat"), out var rp) && rp > 1 ? rp : 1;
            for (int run = 1; run <= repeat; run++)
            {
            var terms = new FtsTermTable();
            var sw = Stopwatch.StartNew();
            TimeSpan? first = null;
            int total = 0, shown = run == 1 ? 0 : 10;
            foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                         engine, query, distance, requireOrdered: false, contextWords: 8, wordOptions, terms))
            {
                first ??= sw.Elapsed;
                total += batch.Hits.Count;
                foreach (var hit in batch.Hits)
                {
                    if (shown >= 10) break;
                    shown++;
                    // The seeded snippet text (Core's FtsSnippetText): the matched line, plus
                    // plain neighbour context for a short window (presentation only).
                    var text = batch.SnippetTexts.Find(t => t.LineId == hit.LineId);
                    string rendered = text == null ? "" : Render(text.Text, hit);
                    if (text != null && (hit.SnippetFlags & FtsHit.SnippetFlagShort) != 0)
                        rendered = WithContext(rendered, text, hit, 8);
                    Console.WriteLine($"line {hit.LineId} book {hit.BookId} dist {hit.WordDistance}: " + rendered);
                }
            }
            if (FtsLib.Fts5.HostTiming.Enabled) Console.Error.WriteLine(FtsLib.Fts5.HostTiming.Report());
            Console.Error.WriteLine($"search: {total:N0} hits in {sw.Elapsed.TotalMilliseconds:F0} ms " +
                                    $"(first batch {first?.TotalMilliseconds:F0} ms); terms: {string.Join(", ", terms.All())}");
            }
            return 0;
        }

        /// <summary>Exact totals (all lines and per book) for one query, timed.</summary>
        private static int Count(SeforimDbFtsEngine engine, string[] args)
        {
            var words = args.Skip(1).Where((a, i) => !a.StartsWith("--", StringComparison.Ordinal)
                && (i == 0 || !new[] { "--db", "--index", "--distance", "--cancel-after", "--order" }.Contains(args[i]))).ToArray();
            string query = string.Join(" ", words);
            int distance = int.TryParse(Option(args, "--distance"), out var d) ? d : int.MaxValue;
            var options = new FtsLib.Fts5.TantivySearchOptions
            {
                WordOptions = new WalkOptions
                {
                    IgnoreQuotes = !args.Contains("--exact-quotes"),
                    ExactQuotes = args.Contains("--exact-quotes"),
                    GrammarPrefixes = args.Contains("--prefixes"),
                    GrammarSuffixes = args.Contains("--suffixes"),
                },
                MaxWordDistance = distance >= int.MaxValue ? (int?)null : distance,
                RequireOrdered = args.Contains("--ordered"),
                UserRegex = args.Contains("--regex"),
                Order = Option(args, "--order") switch
                {
                    "relevance" => ResultsOrder.Relevance,
                    "generation" => ResultsOrder.Generation,
                    _ => ResultsOrder.Catalog,
                },
            };
            // --cancel-after N: cancel the count after N ms (cancellation smoke test)
            using var cts = int.TryParse(Option(args, "--cancel-after"), out var ca)
                ? new System.Threading.CancellationTokenSource(ca) : new System.Threading.CancellationTokenSource();
            var sw = Stopwatch.StartNew();
            HitCounts counts;
            SearchInfo info;
            try { counts = engine.Index.CountByBook(query, options, cts.Token, 4096, out info); }
            catch (OperationCanceledException)
            {
                Console.Error.WriteLine($"count: cancelled after {sw.Elapsed.TotalMilliseconds:F0} ms");
                return 3;
            }
            Console.Error.WriteLine($"count: {counts.Total:N0} lines in {counts.ByBook.Count:N0} books " +
                                    $"(hits validated {counts.Validated:N0}, exact={counts.Exact}) in " +
                                    (sw.Elapsed.TotalSeconds < 1 ? $"{sw.Elapsed.TotalMilliseconds:F0} ms" : $"{sw.Elapsed.TotalMinutes:F2} min"));
            foreach (var notice in info.Notices)
                Console.Error.WriteLine($"notice: {notice}");
            if (info.Truncated)
                Console.Error.WriteLine("truncated: a walk hit its expansion cap");
            foreach (var kv in counts.ByBook.OrderByDescending(kv => kv.Value).Take(5))
                Console.WriteLine($"book {kv.Key}: {kv.Value:N0}");
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
            // --ketiv: FtsLib's expandKetiv through the facade (ktiv with FtsLib's rules).
            var dumpWordOptions = args.Contains("--ketiv")
                ? new WalkOptions { IgnoreQuotes = true, KtivMaleHaser = true, KtivRules = KtivRules.FtsLib }
                : null;
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
                                 engine, query, distance, false, SeforimDbFtsSearcher.DefaultContextWords, dumpWordOptions, terms))
                    {
                        var all = terms.All();
                        foreach (var h in batch.Hits)
                        {
                            if (count++ >= maxHits) break;
                            var sb = new StringBuilder();
                            sb.Append("{\"line\":").Append(h.LineId).Append(",\"book\":").Append(h.BookId)
                              .Append(",\"dist\":").Append(h.WordDistance)
                              .Append(",\"start\":").Append(h.SnippetStart).Append(",\"end\":").Append(h.SnippetEnd)
                              .Append(",\"flags\":").Append(h.SnippetFlags)
                              .Append(",\"marks\":[");
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

        /// <summary>The page's withContext (KitveiHakodesh fullTextSearchSnippets.ts): plain
        /// context for a short window, the last <paramref name="words"/> words of Before and the
        /// first of After, stripped like the window, with "…" where the context is cut. A side
        /// the window itself already cut (its own "…") gets no context.</summary>
        private static string WithContext(string window, FtsSnippetText text, FtsHit hit, int words)
        {
            int flags = hit.SnippetFlags;
            string result = window;
            if (text.Before.Length > 0 && (flags & FtsHit.SnippetFlagLeadingEllipsis) == 0)
            {
                var all = StripTags(text.Before).Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
                var kept = all.Skip(Math.Max(0, all.Length - words)).ToArray();
                if (kept.Length > 0) result = (kept.Length < all.Length ? "…" : "") + string.Join(" ", kept) + " " + result;
            }
            if (text.After.Length > 0 && (flags & FtsHit.SnippetFlagTrailingEllipsis) == 0)
            {
                var all = StripTags(text.After).Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
                var kept = all.Take(words).ToArray();
                if (kept.Length > 0) result = result + " " + string.Join(" ", kept) + (kept.Length < all.Length ? "…" : "");
            }
            return result;
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

        internal static string? OptionOf(string[] args, string name) => Option(args, name);

        private static string? Option(string[] args, string name)
        {
            int i = Array.IndexOf(args, name);
            return i >= 0 && i + 1 < args.Length ? args[i + 1] : null;
        }

        /// <summary>KitveiHakodesh's order: Otzaria, then Zayit (registry choice omitted
        /// to stay cross-platform; pass --db to override). Each app's data folder per OS:
        /// %APPDATA% (Windows), ~/Library/Application Support (macOS),
        /// ~/.local/share (Linux); the same list as tools/bench/get_db.py.</summary>
        private static string? ResolveSeforimDb()
        {
            string home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            var bases = new List<string> { Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData) };
            if (!string.IsNullOrEmpty(home))
            {
                bases.Add(Path.Combine(home, "Library", "Application Support"));
                bases.Add(Path.Combine(home, ".local", "share"));
            }
            foreach (string b in bases)
            {
                if (string.IsNullOrEmpty(b)) continue;
                foreach (var p in new[]
                         {
                             Path.Combine(b, "otzaria", "books", "seforim.db"),
                             Path.Combine(b, "io.github.kdroidfilter.seforimapp", "databases", "seforim.db"),
                         })
                    if (File.Exists(p)) return p;
            }
            return null;
        }
    }
}
