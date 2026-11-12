using System;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Threading;
using SeforimSearch.Demo.SeforimDbFullTextSearch;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// <c>update [--wait] [--watch]</c> (issue #29): the per-book index update a host runs on app
    /// start and whenever seforim.db changes (<see cref="FtsLib.Fts5.TantivySeforimIndex.UpdateChangedBooks"/>):
    /// books whose <c>book.contentHash</c> changed, new and removed books are re-indexed, then the
    /// engine's background full merge starts. <c>--wait</c> waits for that merge and prints its
    /// time. <c>--watch</c> stays running and updates again whenever the DB file stamp (size and
    /// write time of the file and its WAL) changes. When a per-book update cannot serve the change
    /// (no complete index, format change, no contentHash) it says so: run <c>build</c>.
    /// </summary>
    internal static class UpdateCommand
    {
        public static int Run(SeforimDbFtsEngine engine, string db, string[] args)
        {
            bool wait = args.Contains("--wait"), watch = args.Contains("--watch");
            using var cts = new CancellationTokenSource();
            Console.CancelKeyPress += (_, e) => { e.Cancel = true; cts.Cancel(); };
            string stamp = FileStamp(db);
            int code = Once(engine, wait, cts.Token);
            while (watch && !cts.IsCancellationRequested)
            {
                cts.Token.WaitHandle.WaitOne(2000);
                string now = FileStamp(db);
                if (now == stamp) continue;
                stamp = now;
                Console.Error.WriteLine("seforim.db changed");
                code = Once(engine, wait, cts.Token);
            }
            return code;
        }

        private static int Once(SeforimDbFtsEngine engine, bool wait, CancellationToken ct)
        {
            try
            {
                var report = engine.Index.UpdateChangedBooks(ct, line => Console.Error.WriteLine(line));
                engine.InvalidateBookPositions();
                Console.Error.WriteLine("update: " + report);
                if (!report.Applicable) return 3;
                if (wait && report.MergeStarted)
                {
                    var sw = Stopwatch.StartNew();
                    engine.Index.WaitBackgroundMerge();
                    Console.Error.WriteLine($"background merge done: {Time(sw.Elapsed)}");
                }
                return 0;
            }
            catch (OperationCanceledException)
            {
                Console.Error.WriteLine("update cancelled (the remaining books are redone at the next update)");
                return 1;
            }
        }

        private static string Time(TimeSpan t) => t.TotalSeconds < 1 ? $"{t.TotalMilliseconds:F0} ms" : $"{t.TotalMinutes:F2} min";

        private static string FileStamp(string db)
        {
            string Part(string p)
            {
                var f = new FileInfo(p);
                return f.Exists ? $"{f.Length}:{f.LastWriteTimeUtc.Ticks}" : "-";
            }
            return Part(db) + "|" + Part(db + "-wal");
        }
    }
}
