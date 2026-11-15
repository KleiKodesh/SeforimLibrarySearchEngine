using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Data.Sqlite;
using SeforimSearch.Demo.SeforimDbFullTextSearch;
using SnippetMatchMode = FtsLib.Snippets.SnippetMatchMode;

namespace SeforimSearch.Demo
{
    internal static partial class SelfTest
    {
        /// <summary>Issue G-K1 on the fixture: FtsLib's FindSubstringCandidates (ranges in
        /// FtsLib's 1-based positions, its two row orders), Core's SubstringCandidates (book,
        /// lineIndex, line id) and the declines.</summary>
        private static void SubstringFind(string db, string dir)
        {
            string index = Path.Combine(dir, "index-substring");
            using var engine = new SeforimDbFtsEngine(index, db);
            engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
            var books = engine.BookPositions();
            long b1 = books[1].First, b2 = books[2].First;
            Check(b2 == 1 && b1 == LinesPerBook + 1, $"catalog: book 2 first ({b2}, {b1})");

            // "eta" is inside "beta" only: every third line (lineIndex % 3 == 0)
            var one = engine.SubstringCandidates("eta", new[] { (1, 0, 10) })!;
            Check(one.Select(c => c.LineIndex).SequenceEqual(new[] { 0, 3, 6, 9 }), "one book range");
            Check(one.All(c => c.BookId == 1 && c.LineId == c.LineIndex + 1), "book 1 line ids");
            // two words AND (any order), fragments of any length (no affix budget)
            Check(engine.SubstringCandidates("ph et", new[] { (1, 0, 10) })!.Count == 4, "two fragments");
            Check(engine.SubstringCandidates("amm", new[] { (2, 0, 5) })!.Select(c => c.LineIndex).SequenceEqual(new[] { 1, 2, 4, 5 }), "gamma lines");
            Check(engine.SubstringCandidates("zzz", new[] { (1, 0, 10) })!.Count == 0, "no term holds it: answered, empty");
            Check(engine.SubstringCandidates("eta", new[] { (7, 0, 10) })!.Count == 0, "unknown book: nothing");
            // the commentary panel: many one-line ranges over both books
            var many = new List<(int, int, int)>();
            for (int i = 0; i < 20; i++) many.Add((1 + i % 2, i * 3, i * 3));
            var m = engine.SubstringCandidates("eta", many)!;
            Check(m.Count == 20 && m.All(c => c.LineIndex % 3 == 0), "many ranges: " + m.Count);
            // FtsLib's order: few ranges = range by range; many = one ascending pass
            var few = engine.Index.FindSubstringCandidates("eta", new[] { (b1, b1 + 5), (b2, b2 + 5) }, 4000, CancellationToken.None)!;
            Check(few.Select(f => f.Range).SequenceEqual(new[] { 0, 0, 1, 1 }), "few ranges: range order");
            var ranges = new List<(long, long)>();
            for (int i = 19; i >= 0; i--) ranges.Add((b2 + i * 3, b2 + i * 3 + 1));
            var pass = engine.Index.FindSubstringCandidates("eta", ranges, 4000, CancellationToken.None)!;
            Check(pass.Count == 20 && pass.Select(f => f.Position).SequenceEqual(pass.Select(f => f.Position).OrderBy(x => x))
                  && pass[0].Range == 19, "many ranges: one ascending pass, rows given to their range");
            // declines (null): a word with no indexable term; a fragment over the cap ("a" is in 3 terms)
            Check(engine.SubstringCandidates("!!!", new[] { (1, 0, 10) }) == null, "no indexable term: declined");
            Check(engine.Index.FindSubstringCandidates("a", new[] { (b1, b1 + 9) }, 2, CancellationToken.None) == null, "over the cap: declined");
            Check(engine.Index.FindSubstringCandidates("a", new[] { (b1, b1 + 9) }, 3, CancellationToken.None)!.Count == 10, "at the cap: answered");
        }

        private static FtsIndexStatus WaitTerminal(SeforimDbFtsIndexCoordinator c)
        {
            FtsIndexStatus? last = null;
            using var cts = new CancellationTokenSource(TimeSpan.FromMinutes(3));
            c.StreamIndexingProgress(s => { last = s; return Task.CompletedTask; }, cts.Token).GetAwaiter().GetResult();
            Check(!cts.IsCancellationRequested, "progress stream reached a terminal state");
            return last!;
        }

        private static void WaitIdle(SeforimDbFtsIndexCoordinator c)
        {
            var until = DateTime.UtcNow.AddMinutes(3);
            while (c.IsBusy || !c.Status().IsReady)
            {
                if (DateTime.UtcNow > until) throw new TimeoutException("coordinator still busy: " + c.DescribeResidentState());
                Thread.Sleep(50);
            }
        }

        /// <summary>Core's coordinator surface on a copy of the fixture: background build with
        /// progress, status, one-shot search (both match modes), streamed search, lines search,
        /// idle release, DB change (rebuild), reset, shutdown.</summary>
        private static void CoordinatorLifecycle(string db, string dir)
        {
            string copy = Path.Combine(dir, "coordinator.db");
            File.Copy(db, copy, overwrite: true);
            string index = Path.Combine(dir, "index-coordinator");
            var logs = new List<string>();
            var c = new SeforimDbFtsIndexCoordinator(copy, index, false, (level, msg, ex) => { lock (logs) logs.Add(level + " " + msg); });
            Check(new SeforimDbFtsIndexCoordinator(Path.Combine(dir, "missing.db"), index + "-x", false, (_, _, _) => { }).Status().DbMissing,
                  "no DB: DbMissing");

            var early = c.Search("beta", 0, int.MaxValue, false, 8);   // starts the build; may not be ready yet
            var status = WaitTerminal(c);
            Check(status.IsReady && !status.IsIndexing && !status.Failed && status.Percentage == 100, "built: " + status.Percentage);
            Check(c.LastBuildReason?.Reason == "no index yet", "reason: " + c.LastBuildReason?.Reason);
            Check(status.BuildReason == c.LastBuildReason && status.TotalChunks == 2 * LinesPerBook, "status fields");
            WaitIdle(c);

            var r = c.Search("beta", 0, int.MaxValue, false, 8);
            Check(r.Ready && r.Error == null && r.Results.Count == 2 * LinesPerBook / 3 && r.Terms.Count > 0, "one-shot: " + r.Results.Count);
            Check(c.Search("beta", 7, int.MaxValue, false, 8).Results.Count == 7, "cap");
            var all = c.Search("alpha", 5, int.MaxValue, false, 8, SnippetMatchMode.AllMatches);
            Check(all.Results.Count == 5 && all.Results.All(h => h.SnippetStart == 0 && h.SnippetMarks.Length >= 3), "AllMatches: whole line marked");
            var frames = new List<FtsStreamChunk>();
            c.StreamSearch("beta", int.MaxValue, false, 8, f => { frames.Add(f); return Task.CompletedTask; }, CancellationToken.None,
                           0, SnippetMatchMode.BestWindow).GetAwaiter().GetResult();
            Check(frames.SelectMany(f => f.Results).Count() == r.Results.Count && frames.Last().Done, "stream = one-shot");

            var lines = c.LinesSearch("eta", new[] { new FtsLineRange { BookId = 1, FirstLineIndex = 0, LastLineIndex = 10 } });
            Check(lines.Answered && lines.LineIndexes.SequenceEqual(new[] { 0, 3, 6, 9 }) && lines.BookIds.All(b => b == 1)
                  && lines.LineIds.SequenceEqual(new long[] { 1, 4, 7, 10 }), "LinesSearch");
            Check(!c.LinesSearch("!!!", new[] { new FtsLineRange { BookId = 1, FirstLineIndex = 0, LastLineIndex = 10 } }).Answered, "LinesSearch declined");

            Check(!c.IsBusy, "idle");
            Check(c.DescribeResidentState().Contains("engine=created"), c.DescribeResidentState());
            Check(c.ReleaseIdleResources(), "released");
            Check(c.DescribeResidentState().Contains("engine=none"), "engine dropped");
            Check(c.Search("beta", 0, int.MaxValue, false, 8).Results.Count == r.Results.Count, "search after release");
            Check(!c.RebuildIfDbChanged(), "unchanged DB: nothing to do");

            // the DB changes under the running service: a new word appears
            using (var conn = new SqliteConnection($"Data Source={copy};Pooling=False"))
            {
                conn.Open();
                using var cmd = conn.CreateCommand();
                cmd.CommandText = $"INSERT INTO line (id, bookId, lineIndex, content, heRef) VALUES (9001, 1, {LinesPerBook}, 'omega alpha', '')";
                cmd.ExecuteNonQuery();
            }
            Check(c.RebuildIfDbChanged(), "changed DB: work started");
            WaitIdle(c);
            Check(c.LastBuildReason!.Reason.StartsWith("changed while running", StringComparison.Ordinal), "reason: " + c.LastBuildReason.Reason);
            Check(c.Search("omega", 0, int.MaxValue, false, 8).Results.Select(h => h.LineId).SequenceEqual(new long[] { 9001 }), "the new line is found");

            c.ResetIndex("test reset");
            c.ResetIndex("duplicate");   // ignored while the first runs
            WaitIdle(c);
            Check(c.LastBuildReason!.Reason == "test reset", "reset reason: " + c.LastBuildReason.Reason);
            Check(c.Search("omega", 0, int.MaxValue, false, 8).Results.Count == 1, "search after reset");
            c.Shutdown();
            c.ResetIndex("after shutdown");   // ignored
            Check(c.LastBuildReason!.Reason == "test reset", "no reset after shutdown");
            Check(early.Ready || !early.Ready, "early search returned");
            c.ReleaseIdleResources();
        }

        /// <summary>Core's related-forms rewrite on a tiny artifact (Hebrew from escapes only).</summary>
        private static void RelatedForms(string db, string dir)
        {
            string artifact = Path.Combine(dir, "expansion-routed.db");
            string word = "אב", form = "אבג", syn = "דה";
            using (var conn = new SqliteConnection($"Data Source={artifact};Pooling=False"))
            {
                conn.Open();
                using var cmd = conn.CreateCommand();
                cmd.CommandText = "CREATE TABLE fold (surface TEXT, lemma TEXT, weight REAL, source TEXT, PRIMARY KEY(surface, lemma));" +
                                  "CREATE TABLE exp (lemma TEXT, rank INTEGER, form TEXT, channel TEXT, source TEXT, PRIMARY KEY(lemma, rank));" +
                                  "INSERT INTO fold VALUES ($w, $w, 1.0, 'x');" +
                                  "INSERT INTO exp VALUES ($w, 1, $f, 'infl', 'x'), ($w, 2, $s, 'syn', 'tanach');";
                cmd.Parameters.AddWithValue("$w", word);
                cmd.Parameters.AddWithValue("$f", form);
                cmd.Parameters.AddWithValue("$s", syn);
                cmd.ExecuteNonQuery();
            }
            var x = new SeforimDbFtsRelatedFormExpander(artifact);
            Check(x.IsAvailable && SeforimDbFtsRelatedFormExpander.HasRelatedMarker(word + "+"), "marker");
            Check(!SeforimDbFtsRelatedFormExpander.HasRelatedMarker(word), "no marker");
            Check(x.RewriteQuery(word + "+") == word + "+ | " + form, "inflections only");
            Check(x.RewriteQuery(word + "=") == word + "= | " + syn, "synonyms only");
            Check(x.RewriteQuery(word + "+=") == word + "+= | " + form + " | " + syn, "both");
            Check(new SeforimDbFtsRelatedFormExpander(Path.Combine(dir, "none.db")).RewriteQuery(word + "+") == word + "+", "no artifact: unchanged");
        }
    }
}
