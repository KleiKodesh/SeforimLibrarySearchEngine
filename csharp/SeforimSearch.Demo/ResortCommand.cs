using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using SeforimSearch.Demo.SeforimDbFullTextSearch;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// <c>resort [--simulate-move-tenth]</c>: re-sorts the index into the catalog order
    /// seforim.db gives NOW (the books' tree order), without re-indexing
    /// (<see cref="SeforimIndexWriter.ResortAndClose"/>). Use it when books moved in the tree.
    /// <c>--simulate-move-tenth</c> (benchmark): instead moves one contiguous tenth of the
    /// books (40-50 % of the catalog) to the end, a realistic order change on an unchanged DB.
    /// Prints progress, then the time, the process peak working set, the peak disk of the
    /// index directory (old + new segment exist together until the publish) and the size
    /// after. Ctrl+C cancels (the index keeps its old order). Run it on a copy when only
    /// measuring.
    /// </summary>
    internal static class ResortCommand
    {
        public static int Run(SeforimDbFtsEngine engine, string db, string indexDir, string[] args)
        {
            List<long> order;
            using (var corpus = new SeforimDbFtsCorpus(db))
                order = corpus.CatalogBookOrder();
            if (args.Contains("--simulate-move-tenth"))
            {
                int a = order.Count * 4 / 10, b = order.Count * 5 / 10;
                var moved = order.GetRange(a, b - a);
                order.RemoveRange(a, b - a);
                order.AddRange(moved);
                Console.Error.WriteLine($"simulated order change: books {a:N0}..{b:N0} of {order.Count:N0} moved to the end");
            }
            Console.Error.WriteLine($"resort: {order.Count:N0} books in the new order");

            long sizeBefore = DirSize(indexDir), peakDisk = sizeBefore;
            using var cts = new CancellationTokenSource();
            Console.CancelKeyPress += (_, e) => { e.Cancel = true; cts.Cancel(); };
            var sw = Stopwatch.StartNew();
            using var diskSampler = new Timer(_ =>
            {
                long now = DirSize(indexDir);
                long seen;
                do { seen = Interlocked.Read(ref peakDisk); }
                while (now > seen && Interlocked.CompareExchange(ref peakDisk, now, seen) != seen);
            }, null, 0, 250);
            ResortStage lastStage = ResortStage.NotStarted;
            double lastShown = 0;
            var progress = new SyncProgress(p =>
            {
                double s = sw.Elapsed.TotalSeconds;
                if (p.Stage == lastStage && s - lastShown < 5) return;
                lastStage = p.Stage;
                lastShown = s;
                string part = p.Total > 0 ? $" {p.Done:N0} / {p.Total:N0} ({100.0 * p.Done / p.Total:F0} %)" : "";
                Console.Error.WriteLine($"  {Time(sw.Elapsed)}  {p.Stage}{part}");
            });
            long lines;
            try
            {
                var writer = engine.Index.OpenWriter(64_000_000, 1);
                lines = writer.ResortAndClose(order, progress, cts.Token);
            }
            catch (OperationCanceledException)
            {
                Console.Error.WriteLine($"resort: cancelled after {Time(sw.Elapsed)}; the index keeps its old order");
                return 1;
            }
            sw.Stop();
            using (var stopped = new ManualResetEvent(false))
                if (diskSampler.Dispose(stopped)) stopped.WaitOne();
            long sizeAfter = DirSize(indexDir);
            peakDisk = Math.Max(peakDisk, sizeAfter);
            using var self = Process.GetCurrentProcess();
            Console.Error.WriteLine($"resort: {lines:N0} lines in {Time(sw.Elapsed)}");
            Console.Error.WriteLine($"process peak working set: {self.PeakWorkingSet64 / 1e6:F1} MB");
            Console.Error.WriteLine($"index dir: before {sizeBefore / 1e6:F1} MB, peak {peakDisk / 1e6:F1} MB, after {sizeAfter / 1e6:F1} MB");
            return 0;
        }

        /// <summary>ms below a second, minutes otherwise (the repo's report units).</summary>
        private static string Time(TimeSpan t) =>
            t.TotalSeconds < 1 ? $"{t.TotalMilliseconds:F0} ms" : $"{t.TotalMinutes:F2} min";

        private static long DirSize(string dir)
        {
            long total = 0;
            try
            {
                foreach (var f in new DirectoryInfo(dir).EnumerateFiles())
                {
                    try { total += f.Length; } catch (IOException) { } // removed meanwhile
                }
            }
            catch (IOException) { }
            return total;
        }

        /// <summary>Reports on the reporting thread (the native call's timer), in order.</summary>
        private sealed class SyncProgress : IProgress<ResortProgress>
        {
            private readonly Action<ResortProgress> _report;
            public SyncProgress(Action<ResortProgress> report) { _report = report; }
            public void Report(ResortProgress value) { lock (this) _report(value); }
        }
    }
}
