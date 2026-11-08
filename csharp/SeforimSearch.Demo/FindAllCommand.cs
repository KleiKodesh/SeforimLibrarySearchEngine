using System;
using System.Diagnostics;
using System.IO;
using System.Linq;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// <c>findall [--page file]</c> (issues #14 / #23): quick checks of
    /// <see cref="TextMatcher.FindAllMatches"/> and, given a page (UTF-8 text, e.g. the one
    /// <c>hebrew-fts/examples/find_all_bench.rs</c> writes), its time per call from C#
    /// (UTF-8 encode + FFI + result copy included). Exit code 0 = checks passed.
    /// </summary>
    internal static class FindAllCommand
    {
        public static int Run(string[] args)
        {
            const string a = "אב", b = "גד";
            string t = $"{a} {b} {a} <b>{b}</b> {a}";
            int failed = 0;
            void Check(bool ok, string what) { if (!ok) { failed++; Console.WriteLine("FAIL " + what); } }
            Check(TextMatcher.FindAllMatches(t, new[] { a }).Select(m => m.Start).SequenceEqual(new[] { 0, 6, 19 }), "repeated word");
            Check(TextMatcher.FindAllMatches(t, new[] { a, b }).Select(m => m.Term).SequenceEqual(new[] { 0, 1, 0, 1, 0 }), "ordinals");
            Check(TextMatcher.FindAllMatches(t, new[] { $"{a} {b}" }, FindMatchOptions.Phrase).Select(m => m.Start)
                  .SequenceEqual(new[] { 0, 3, 6, 12 }), "phrase");
            Check(TextMatcher.FindAllMatches("x אב\"ג", new[] { "אבג" }, FindMatchOptions.IgnoreQuotes)
                  .Select(m => (m.Start, m.Length)).SequenceEqual(new[] { (2, 4) }), "quotes ignored");
            Check(TextMatcher.FindAllMatches("אָב x", new[] { a }).Single().Length == 3, "marks ignored");
            Check(TextMatcher.FindAllMatches("", new[] { a }).Length == 0, "empty text");
            Console.WriteLine(failed == 0 ? "findall: checks passed" : $"findall: {failed} FAILED");

            string? page = Program.OptionOf(args, "--page");
            if (page != null)
            {
                string text = File.ReadAllText(page);
                Console.WriteLine($"page: {text.Length} UTF-16 units");
                void Time(string label, Func<int> f)
                {
                    double best = double.MaxValue, total = 0; int n = 0; const int rounds = 30;
                    for (int i = 0; i < rounds; i++)
                    {
                        var sw = Stopwatch.StartNew();
                        n = f();
                        double ms = sw.Elapsed.TotalMilliseconds;
                        best = Math.Min(best, ms); total += ms;
                    }
                    Console.WriteLine($"{label,-40} {n,7} matches  best {best:F2} ms  mean {total / rounds:F2} ms");
                }
                const string word = "משה";
                Time("engine FindAllMatches (FFI, one call)", () => TextMatcher.FindAllMatches(text, new[] { word }).Length);
                Time("engine FindAllMatches, quotes ignored", () => TextMatcher.FindAllMatches(text, new[] { word }, FindMatchOptions.IgnoreQuotes).Length);
                Time("engine FindAllMatches, 2-word phrase", () => TextMatcher.FindAllMatches(text, new[] { "אל העם" }, FindMatchOptions.Phrase).Length);
            }
            return failed == 0 ? 0 : 1;
        }
    }
}
