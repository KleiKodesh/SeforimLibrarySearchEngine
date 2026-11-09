using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using SeforimSearch.Demo.SeforimDbFullTextSearch;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// <c>load --queries file [--searchers N] [--rounds R] [--commit-every MS] [--update-lines K]
    /// [--update-from P]</c> (issue #31): the speed benchmark's LOAD mode. N searcher threads
    /// each run every query of the file (one per line, <c>query[TAB]distance</c>) R times, through
    /// the same streaming path as <c>search</c> (one coordinator per thread: the host's own
    /// supersession is per coordinator), while ONE writer on the same index re-indexes K lines
    /// (catalog positions from P, the same documents the build writes: delete + add) and
    /// commits every MS ms. Searches see each commit as soon as it lands (the writer reloads the
    /// index's shared reader); a running search keeps its snapshot.
    ///
    /// Output (stderr, one line per event, parsed by tools/report/search_report.py --load):
    ///   load-search q=&lt;n&gt; searcher=&lt;s&gt; round=&lt;r&gt; hits=&lt;h&gt; first=&lt;ms&gt; total=&lt;ms&gt; [error=...]
    ///   load-commit n=&lt;i&gt; lines=&lt;k&gt; ms=&lt;ms&gt;
    ///   load: done in &lt;ms&gt; ms, &lt;searches&gt; searches, &lt;commits&gt; commits, &lt;errors&gt; errors
    /// The updates CHANGE the index (more segments until a merge): run it on a copy
    /// (search_report.py --load copies the index first). Exit code 0 = no search or commit failed
    /// and the writer committed at least once while the searches ran.
    /// </summary>
    internal static class LoadCommand
    {
        public static int Run(SeforimDbFtsEngine engine, string[] args)
        {
            string? queriesFile = Program.OptionOf(args, "--queries");
            if (queriesFile == null || !File.Exists(queriesFile))
            {
                Console.Error.WriteLine("load: --queries <file> is required");
                return 2;
            }
            int searchers = IntOption(args, "--searchers", 4, 1);
            int rounds = IntOption(args, "--rounds", 3, 1);
            int commitEvery = IntOption(args, "--commit-every", 1000, 1);
            int updateLines = IntOption(args, "--update-lines", 100, 1);
            long updateFrom = long.TryParse(Program.OptionOf(args, "--update-from"), out var uf) && uf >= 0 ? uf : 0;
            var queries = File.ReadAllLines(queriesFile)
                .Where(l => l.Trim().Length > 0 && !l.StartsWith("#", StringComparison.Ordinal))
                .Select(l =>
                {
                    int tab = l.IndexOf('\t');
                    string q = tab < 0 ? l : l.Substring(0, tab);
                    int dist = tab >= 0 && int.TryParse(l.Substring(tab + 1).Trim(), out var d) ? d : int.MaxValue;
                    return (Query: string.Join(" ", q.Split(new[] { ' ' }, StringSplitOptions.RemoveEmptyEntries)), Distance: dist);
                })
                .ToList();
            if (!engine.Index.IsBuilt)
            {
                Console.Error.WriteLine("load: index not built");
                return 1;
            }
            var docs = engine.Index.ReadLineDocuments(updateFrom, updateLines);
            if (docs.Count == 0)
            {
                Console.Error.WriteLine($"load: no lines at position {updateFrom} to update");
                return 1;
            }
            Console.Error.WriteLine($"load: {searchers} searchers x {rounds} rounds x {queries.Count} queries; " +
                                    $"writer: {docs.Count} lines from position {updateFrom}, commit every {commitEvery} ms");

            var output = new object();
            void Emit(string line) { lock (output) Console.Error.WriteLine(line); }
            int errors = 0, searches = 0, commits = 0;
            var wall = Stopwatch.StartNew();
            using var stop = new CancellationTokenSource();

            var writerThread = new Thread(() =>
            {
                try
                {
                    using var writer = engine.Index.OpenWriter(30_000_000, 1);
                    while (!stop.Token.WaitHandle.WaitOne(commitEvery))
                    {
                        var sw = Stopwatch.StartNew();
                        foreach (var d in docs)
                        {
                            writer.Delete(d.LineId);
                            writer.Add(d);
                        }
                        engine.Index.CommitKeepingBuildState(writer);
                        int n = Interlocked.Increment(ref commits);
                        Emit($"load-commit n={n} lines={docs.Count} ms={sw.Elapsed.TotalMilliseconds:F1}");
                    }
                }
                catch (Exception e)
                {
                    Interlocked.Increment(ref errors);
                    Emit("load-commit error=" + OneLine(e.Message));
                }
            }) { IsBackground = true, Name = "load-writer" };
            writerThread.Start();

            var threads = Enumerable.Range(0, searchers).Select(s => new Thread(() =>
            {
                var coordinator = new SeforimDbFtsIndexCoordinator(engine);
                for (int round = 1; round <= rounds; round++)
                    for (int k = 0; k < queries.Count; k++)
                    {
                        int qi = (k + s) % queries.Count;  // searchers start on different queries
                        var (hits, first, total, error) = SearchOnce(coordinator, queries[qi].Query, queries[qi].Distance);
                        Interlocked.Increment(ref searches);
                        if (error != null) Interlocked.Increment(ref errors);
                        Emit($"load-search q={qi} searcher={s} round={round} hits={hits} " +
                             $"first={(first is double f ? f.ToString("F1") : "-")} total={total:F1}" +
                             (error != null ? " error=" + OneLine(error) : ""));
                    }
            }) { IsBackground = true, Name = "load-search-" + s }).ToList();
            threads.ForEach(t => t.Start());
            threads.ForEach(t => t.Join());
            stop.Cancel();
            writerThread.Join();
            Console.Error.WriteLine($"load: done in {wall.Elapsed.TotalMilliseconds:F0} ms, {searches} searches, {commits} commits, {errors} errors");
            if (commits == 0)
            {
                // nothing was measured under load: the searches finished before the first commit
                Console.Error.WriteLine("load: the writer never committed during the searches; lower --commit-every or raise --rounds");
                return 1;
            }
            return errors == 0 ? 0 : 1;
        }

        /// <summary>One streamed search, timed as the <c>search</c> command times it: first batch
        /// = from the call to the first results frame, total = the whole call.</summary>
        private static (int Hits, double? FirstMs, double TotalMs, string? Error) SearchOnce(
            SeforimDbFtsIndexCoordinator coordinator, string query, int distance)
        {
            var sw = Stopwatch.StartNew();
            double? first = null;
            int hits = 0;
            string? error = null;
            coordinator.StreamSearch(query, distance, requireOrdered: false, contextWords: 8, chunk =>
            {
                if (!chunk.Ready) error = "index not built";
                if (chunk.Error != null) error = chunk.Error;
                if (chunk.Results.Count > 0 && first == null) first = sw.Elapsed.TotalMilliseconds;
                hits += chunk.Results.Count;
                return Task.CompletedTask;
            }, CancellationToken.None).GetAwaiter().GetResult();
            return (hits, first, sw.Elapsed.TotalMilliseconds, error);
        }

        private static int IntOption(string[] args, string name, int fallback, int min) =>
            int.TryParse(Program.OptionOf(args, name), out var v) && v >= min ? v : fallback;

        private static string OneLine(string s) => s.Replace('\r', ' ').Replace('\n', ' ');
    }
}
