using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Data.Sqlite;
using SeforimSearch.Demo.SeforimDbFullTextSearch;
using SeforimSearch.Engine;

namespace SeforimSearch.Demo
{
    /// <summary>
    /// <c>selftest [--dir path]</c>: quick C# tests on a small seforim.db-shaped SQLite fixture
    /// (Zayit shape: <c>line.content</c> TEXT, ASCII words): the corpus pool (double Dispose,
    /// abandoned enumerator, concurrent rent, reopen on file change) and an end-to-end build +
    /// search through the FtsLib facade. Exit code 0 = all passed.
    /// </summary>
    internal static class SelfTest
    {
        private const int LinesPerBook = 1500;

        public static int Run(string[] args)
        {
            string dir = Program.OptionOf(args, "--dir")
                         ?? Path.Combine(Path.GetTempPath(), "seforim-selftest-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(dir);
            string db = Path.Combine(dir, "seforim.db");
            CreateFixture(db);
            var tests = new (string Name, Action<string, string> Body)[]
            {
                ("double dispose returns the connection once", DoubleDispose),
                ("abandoned enumerator does not keep a pooled connection", AbandonedEnumerator),
                ("concurrent rents get distinct connections", ConcurrentRent),
                ("file change reopens", FileChangeReopens),
                ("end to end: build tantivy11 + search in catalog order", EndToEnd),
            };
            int failed = 0;
            foreach (var (name, body) in tests)
            {
                var sw = System.Diagnostics.Stopwatch.StartNew();
                try
                {
                    body(db, dir);
                    Console.WriteLine($"ok    {name} ({sw.ElapsedMilliseconds} ms)");
                }
                catch (Exception e)
                {
                    failed++;
                    Console.WriteLine($"FAIL  {name}: {e.GetType().Name}: {e.Message}");
                }
            }
            Console.WriteLine(failed == 0 ? $"selftest: all {tests.Length} passed" : $"selftest: {failed} of {tests.Length} FAILED");
            SqliteConnection.ClearAllPools();
            try { Directory.Delete(dir, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
            return failed == 0 ? 0 : 1;
        }

        private static void Check(bool ok, string what)
        {
            if (!ok) throw new InvalidOperationException(what);
        }

        /// <summary>Two books in one category; book 2 comes first in catalog order (orderIndex
        /// 1) although its line ids are higher. Every third line has "beta".</summary>
        private static void CreateFixture(string path)
        {
            using var c = new SqliteConnection($"Data Source={path};Pooling=False");
            c.Open();
            using var tx = c.BeginTransaction();
            using (var cmd = c.CreateCommand())
            {
                cmd.Transaction = tx;
                cmd.CommandText = @"
CREATE TABLE category (id INTEGER PRIMARY KEY NOT NULL, parentId INTEGER, title TEXT NOT NULL,
  level INTEGER NOT NULL DEFAULT 0, orderIndex INTEGER NOT NULL DEFAULT 999);
CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, categoryId INTEGER NOT NULL, sourceId INTEGER NOT NULL DEFAULT 0,
  title TEXT NOT NULL, heRef TEXT, orderIndex INTEGER NOT NULL DEFAULT 999, totalLines INTEGER NOT NULL DEFAULT 0,
  isBaseBook INTEGER NOT NULL DEFAULT 0);
CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL,
  content TEXT NOT NULL, heRef TEXT, tocEntryId INTEGER);
CREATE TABLE author (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL);
CREATE TABLE book_author (bookId INTEGER NOT NULL, authorId INTEGER NOT NULL);
INSERT INTO category (id, parentId, title, level, orderIndex) VALUES (1, NULL, 'cat', 0, 1);
INSERT INTO book (id, categoryId, title, orderIndex) VALUES (1, 1, 'book one', 2), (2, 1, 'book two', 1);";
                cmd.ExecuteNonQuery();
            }
            using (var cmd = c.CreateCommand())
            {
                cmd.Transaction = tx;
                cmd.CommandText = "INSERT INTO line (id, bookId, lineIndex, content, heRef) VALUES ($id, $b, $i, $t, '')";
                var id = cmd.Parameters.Add("$id", SqliteType.Integer);
                var b = cmd.Parameters.Add("$b", SqliteType.Integer);
                var i = cmd.Parameters.Add("$i", SqliteType.Integer);
                var t = cmd.Parameters.Add("$t", SqliteType.Text);
                for (int book = 1; book <= 2; book++)
                    for (int k = 0; k < LinesPerBook; k++)
                    {
                        id.Value = (book - 1) * LinesPerBook + k + 1;
                        b.Value = book;
                        i.Value = k;
                        t.Value = k % 3 == 0 ? $"alpha beta w{k % 97}" : $"gamma alpha x{k % 31}";
                        cmd.ExecuteNonQuery();
                    }
            }
            tx.Commit();
        }

        private static void DoubleDispose(string db, string dir)
        {
            using var pool = new SeforimDbFtsCorpusPool(db);
            var a = pool.Rent();
            var first = a.Connection;
            Check(a.CountDocuments() == 2 * LinesPerBook, "count");
            a.Dispose();
            a.Dispose(); // harmless
            Check(pool.IdleCount == 1, "returned exactly once: idle " + pool.IdleCount);
            using var b = pool.Rent();
            using var c = pool.Rent();
            Check(ReferenceEquals(b.Connection, first), "the returned connection is reused");
            Check(!ReferenceEquals(b.Connection, c.Connection), "one connection handed out twice");
            bool threw = false;
            try { a.CountDocuments(); } catch (ObjectDisposedException) { threw = true; }
            Check(threw, "a disposed lease must refuse calls");
            a.Dispose(); // still harmless while b holds the connection
            Check(pool.IdleCount == 0, "a stale lease returned b's connection");
            Check(b.CountDocuments() == 2 * LinesPerBook, "b still works");
        }

        private static void AbandonedEnumerator(string db, string dir)
        {
            using var pool = new SeforimDbFtsCorpusPool(db);
            // a fully enumerated sequence: the connection is pooled
            using (var ok = pool.Rent())
                Check(ok.FetchPlacedDocuments(Enumerable.Range(1, 10)).Count() == 10, "fetch 10");
            Check(pool.IdleCount == 1, "pooled after a complete enumeration");

            var a = pool.Rent();
            var conn = a.Connection;
            var e = a.FetchPlacedDocuments(Enumerable.Range(1, 2000)).GetEnumerator();
            Check(e.MoveNext(), "first row");
            a.Dispose(); // the enumerator is abandoned, never disposed
            Check(pool.IdleCount == 0, "a connection with an open sequence must not be pooled");
            bool threw = false;
            try { e.MoveNext(); } catch (ObjectDisposedException) { threw = true; }
            Check(threw, "the abandoned enumerator must fail, not read a closed/pooled connection");
            using var b = pool.Rent();
            Check(!ReferenceEquals(b.Connection, conn), "the abandoned connection came back");
            Check(b.FetchPlacedDocuments(Enumerable.Range(1, 2000)).Count() == 2000, "fresh connection works");
            GC.KeepAlive(e);
        }

        private static void ConcurrentRent(string db, string dir)
        {
            using var pool = new SeforimDbFtsCorpusPool(db);
            const int n = 8;
            var held = new SeforimDbFtsCorpus[n];
            using var barrier = new Barrier(n);
            Parallel.For(0, n, new ParallelOptions { MaxDegreeOfParallelism = n }, i =>
            {
                held[i] = pool.Rent();
                Check(held[i].CountDocuments() == 2 * LinesPerBook, "count");
                barrier.SignalAndWait();
                Check(held[i].FetchPlacedDocuments(new[] { i + 1 }).Single().Id == i + 1, "fetch");
                barrier.SignalAndWait();
                held[i].Dispose();
            });
            Check(held.Select(h => h.Connection).Distinct().Count() == n, "connections shared while held");
            Check(pool.IdleCount == 4, "idle capped at 4: " + pool.IdleCount);
        }

        private static void FileChangeReopens(string db, string dir)
        {
            string copy = Path.Combine(dir, "changing.db");
            File.Copy(db, copy, true);
            using var pool = new SeforimDbFtsCorpusPool(copy);
            SeforimDbCorpusConnection before;
            using (var a = pool.Rent())
            {
                before = a.Connection;
                Check(a.CountDocuments() == 2 * LinesPerBook, "count before");
            }
            using (var w = new SqliteConnection($"Data Source={copy};Pooling=False"))
            {
                w.Open();
                using var cmd = w.CreateCommand();
                cmd.CommandText = "INSERT INTO line (id, bookId, lineIndex, content) VALUES (99999, 1, 99999, 'alpha new')";
                cmd.ExecuteNonQuery();
            }
            File.SetLastWriteTimeUtc(copy, DateTime.UtcNow.AddMinutes(1));
            using var b = pool.Rent();
            Check(!ReferenceEquals(b.Connection, before), "not reopened after the file changed");
            Check(b.CountDocuments() == 2 * LinesPerBook + 1, "the reopened connection sees the change");
        }

        private static void EndToEnd(string db, string dir)
        {
            string index = Path.Combine(dir, "index");
            using var engine = new SeforimDbFtsEngine(index, db);
            engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
            Check(engine.Index.IsBuilt, "built");
            string meta = File.ReadAllText(Path.Combine(index, "meta.json"));
            Check(meta.Contains("sort_by_field") && meta.Contains("\"position\""), "index not sorted by position");
            Check(FtsLib.Fts5.TantivySeforimIndex.IndexFormatVersion == "tantivy11", "format version");

            // catalog order: book 2 (orderIndex 1) first, then book 1, by lineIndex
            var expected = new List<int>();
            foreach (int book in new[] { 2, 1 })
                for (int k = 0; k < LinesPerBook; k += 3)
                    expected.Add((book - 1) * LinesPerBook + k + 1);

            for (int round = 0; round < 2; round++) // the second round reuses the pooled DB
            {
                var got = new List<int>();
                int snippets = 0;
                foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                             engine, "beta", int.MaxValue, false, 8, null, new FtsTermTable()))
                    foreach (var hit in batch.Hits)
                    {
                        got.Add(hit.LineId);
                        if (hit.SnippetMarks.Length > 0) snippets++;
                    }
                Check(got.SequenceEqual(expected), $"round {round}: hits {got.Count}, expected {expected.Count} in catalog order");
                Check(snippets == got.Count, "every hit has a marked snippet");
            }
        }
    }
}
