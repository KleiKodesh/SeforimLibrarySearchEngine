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
                ("WAL commit (data_version) reopens on rent", DataVersionReopens),
                ("codec: dictionary appended via WAL is picked up", CodecReloadsNewDictionary),
                ("codec: base64 text vs base64 frame, errors name the line", CodecBase64AndErrors),
                ("end to end: build tantivy16 + search in catalog order", EndToEnd),
                ("stream: chunks, resume, Restarted, cancellation (Core's StreamSearch)", Stream),
                ("async engine API (issue #16): batches = sync hits, cancel, abandon, errors", AsyncEngine),
                ("async open (issue #34): OpenAsync opens and searches, cancelled before open", AsyncOpen),
                ("FM sidecar auto-repair (issue #28): background build, switchable", FmAutoRepair),
                ("second writer (issue #31): lock error on the same and on another handle", SecondWriter),
                ("load mode (issue #31): concurrent searches while a writer commits", LoadMode),
                ("native ABI check (issue #17): loaded = expected, mismatch names both and the path", AbiCheck),
                ("notes books (issue #13): ids, order, search, snippet, hit -> source, unreadable, BOM, guard", NotesBooks),
                ("per-book update (issue #29): changed / grown / removed books, crash mid-update, background merge", BookUpdates),
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
            // --keep: leave the fixture (seforim.db + indexes) for manual smoke runs
            if (!args.Contains("--keep"))
                try { Directory.Delete(dir, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
            return failed == 0 ? 0 : 1;
        }

        private static void BookUpdates(string fixtureDb, string dir)
        {
            string db = Path.Combine(dir, "updates.db");
            File.Copy(fixtureDb, db, true);
            void Sql(string sql)
            {
                using var c = new SqliteConnection($"Data Source={db};Pooling=False");
                c.Open();
                using var cmd = c.CreateCommand();
                cmd.CommandText = sql;
                cmd.ExecuteNonQuery();
            }
            Sql("ALTER TABLE book ADD COLUMN contentHash INTEGER; UPDATE book SET contentHash = id * 1000;");
            string index = Path.Combine(dir, "update-index");
            List<long> Hits(SeforimDbFtsEngine e, string q, long after = 0)
            {
                var got = new List<long>();
                foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                             e, q, int.MaxValue, false, 8, null, new FtsTermTable(), afterPosition: after))
                    foreach (var hit in batch.Hits) got.Add(hit.LineId);
                return got;
            }
            // the fixture's beta lines of a book (every third line), in reading order
            List<long> Beta(int book) =>
                Enumerable.Range(0, LinesPerBook).Where(k => k % 3 == 0).Select(k => (long)((book - 1) * LinesPerBook + k + 1)).ToList();
            void Resumes(SeforimDbFtsEngine e, List<long> expected)
            {
                // positions = the DB's current numbering: a resume after the n-th hit gives the rest
                e.InvalidateBookPositions();
                foreach (int n in new[] { 1, expected.Count / 2, expected.Count - 1 })
                {
                    long? after = e.PositionOfLine(expected[n - 1]);
                    Check(after.HasValue, "resume line placed");
                    var rest = Hits(e, "beta", after!.Value);
                    Check(expected.Skip(n).SequenceEqual(rest), $"resume after hit {n}: {rest.Count} != {expected.Count - n}");
                }
            }
            // book 1 with line 2 (index 1) turned into a beta line
            List<long> Book1() { var b = Beta(1); b.Insert(1, 2); return b; }

            using (var engine = new SeforimDbFtsEngine(index, db))
            {
                engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
                var r = engine.Index.UpdateChangedBooks();
                Check(r.Applicable && r.Changed + r.Added + r.Removed == 0, "adopt: " + r);
                Check(engine.Index.ReadBuildState()!.Books != null, "book hashes recorded");
                r = engine.Index.UpdateChangedBooks();
                Check(r.Applicable && r.Changed == 0 && !r.MergeStarted, "nothing changed: " + r);

                // book 1 changed, same line count: plain merge, positions kept, book 2 untouched
                Sql("UPDATE line SET content = 'omega beta' WHERE id = 2; UPDATE book SET contentHash = 1001 WHERE id = 1;");
                r = engine.Index.UpdateChangedBooks();
                Check(r.Changed == 1 && !r.Repack && r.LinesWritten == LinesPerBook, "changed book: " + r);
                engine.Index.WaitBackgroundMerge();
                Check(Hits(engine, "omega").SequenceEqual(new long[] { 2 }), "the changed line is found");
                var beta = Beta(2).Concat(Book1()).ToList();
                Check(Hits(engine, "beta").SequenceEqual(beta), "beta in catalog order after the update");
                Resumes(engine, beta);
            }
            Check(!File.Exists(Path.Combine(index, "background_merge.json")), "merge record removed");

            // book 2 (first in the catalog) grows by two lines: repack, book 1 renumbered
            Sql($"INSERT INTO line (id, bookId, lineIndex, content, heRef) VALUES (9001, 2, {LinesPerBook}, 'psi beta', ''), (9002, 2, {LinesPerBook + 1}, 'psi', '');" +
                "UPDATE book SET contentHash = 2001 WHERE id = 2;");
            using (var engine = new SeforimDbFtsEngine(index, db))
            {
                var r = engine.Index.UpdateChangedBooks();
                Check(r.Changed == 1 && r.Repack && r.MergeStarted, "grown book: " + r);
                engine.Index.WaitBackgroundMerge();
                Check(Hits(engine, "psi").SequenceEqual(new long[] { 9001, 9002 }), "new lines found");
                var beta = Beta(2).Concat(new long[] { 9001 }).Concat(Book1()).ToList();
                Check(Hits(engine, "beta").SequenceEqual(beta), "beta in catalog order after the repack");
                Resumes(engine, beta);
                Check(engine.PositionOfLine(1) == LinesPerBook + 3, "book 1 starts after the grown book 2");

                // crash mid-update: both books change, one commit per book, cancelled after the first
                Sql("UPDATE line SET content = 'rho' WHERE id = 3; UPDATE book SET contentHash = 1002 WHERE id = 1;" +
                    $"UPDATE line SET content = 'rho' WHERE id = {LinesPerBook + 2}; UPDATE book SET contentHash = 2002 WHERE id = 2;");
                engine.Index.UpdateLinesPerCommit = 1;
                using var cts = new CancellationTokenSource();
                bool cancelled = false;
                try { engine.Index.UpdateChangedBooks(cts.Token, line => { if (line.Contains(" 1 / 2 books")) cts.Cancel(); }); }
                catch (OperationCanceledException) { cancelled = true; }
                Check(cancelled, "the update was cancelled after its first book");
                var mid = engine.Index.ReadBuildState()!;
                Check(!string.IsNullOrEmpty(mid.Pending) && mid.Pending!.Split(',').Length == 1, "one book still pending: " + mid.Pending);
                Check(Hits(engine, "rho").Count == 1, "the first book's commit is visible, the other book is whole and old");
                r = engine.Index.UpdateChangedBooks();
                Check(r.Changed == 1 && string.IsNullOrEmpty(engine.Index.ReadBuildState()!.Pending), "resumed: " + r);
                engine.Index.WaitBackgroundMerge();
                Check(Hits(engine, "rho").SequenceEqual(new long[] { LinesPerBook + 2, 3 }), "both books updated, catalog order");

                // a removed book: its lines go, the next book moves up
                Sql("DELETE FROM line WHERE bookId = 2; DELETE FROM book WHERE id = 2;");
                r = engine.Index.UpdateChangedBooks();
                Check(r.Removed == 1 && r.Repack, "removed book: " + r);
                engine.Index.WaitBackgroundMerge();
                Check(Hits(engine, "psi").Count == 0, "removed book's lines gone");
                engine.InvalidateBookPositions();
                Check(engine.PositionOfLine(1) == 1, "book 1 is first now");
                Check(Hits(engine, "beta").SequenceEqual(Book1()), "book 1 alone");
                Resumes(engine, Book1());
            }
        }

        private static void AsyncOpen(string db, string dir)
        {
            string d = Path.Combine(dir, "async-open");
            using (var created = SeforimIndex.CreateAsync(d).GetAwaiter().GetResult())
            {
                var w = created.OpenWriter(30_000_000, 1);
                w.Add(new LineDocument { LineId = 1, Position = 0, BookId = 1, Text = "alpha beta" });
                w.Add(new LineDocument { LineId = 2, Position = 1, BookId = 1, Text = "gamma beta" });
                w.MergeAndClose();
            }
            using (var index = SeforimIndex.OpenAsync(d).GetAwaiter().GetResult())
            using (var s = index.Search(new SearchOptions("beta")))
                Check(s.HitCount == 2, "OpenAsync then search: " + s.HitCount);
            using (var cts = new CancellationTokenSource())
            {
                cts.Cancel();
                var task = SeforimIndex.OpenAsync(d, cts.Token);
                Check(task.IsCanceled, "a token cancelled before open gives a cancelled task");
                bool threw = false;
                try { task.GetAwaiter().GetResult(); } catch (OperationCanceledException) { threw = true; }
                Check(threw, "awaiting it throws OperationCanceledException");
            }
        }

        private static void FmAutoRepair(string db, string dir)
        {
            string d = Path.Combine(dir, "fm-repair");
            using (var index = SeforimIndex.Create(d))
            {
                var w = index.OpenWriter(30_000_000, 1);
                w.Add(new LineDocument { LineId = 1, Position = 0, BookId = 1, Text = "alpha beta" });
                w.Add(new LineDocument { LineId = 2, Position = 1, BookId = 1, Text = "gamma alphabet" });
                w.MergeAndClose();
                index.SetFmAutoRepair(false);
                using (index.Search(new SearchOptions("*lph*"))) { }
                Check(!index.FmSidecarRebuilding && !index.HasFmSidecar, "off: no rebuild");
                index.SetFmAutoRepair(true);
                using (var s = index.Search(new SearchOptions("*lph*")))
                    Check(s.HitCount == 2, "walk hits while rebuilding: " + s.HitCount);
                var until = DateTime.UtcNow.AddSeconds(30);
                while (index.FmSidecarRebuilding && DateTime.UtcNow < until)
                    System.Threading.Thread.Sleep(10);
                Check(!index.FmSidecarRebuilding, "the rebuild finished");
                Check(index.HasFmSidecar, "the rebuilt sidecar is used");
                Check(index.FmSidecarProblem == null, "no problem left");
            }
        }

        /// <summary>Issue #31: one writer per index directory. A second OpenWriter, on the same
        /// SeforimIndex or on another one opened on the same directory, throws
        /// SeforimEngineException with tantivy's lockfile message (LockBusy).</summary>
        private static void SecondWriter(string db, string dir)
        {
            string d = Path.Combine(dir, "second-writer");
            using (var index = SeforimIndex.Create(d))
            using (var other = SeforimIndex.Open(d))
            {
                using (var first = index.OpenWriter(30_000_000, 1))
                {
                    foreach (var handle in new[] { index, other })
                    {
                        string? message = null;
                        try { handle.OpenWriter(30_000_000, 1).Dispose(); }
                        catch (SeforimEngineException e) { message = e.Message; }
                        Check(message != null && message.Contains("LockBusy"), "second writer: " + (message ?? "no error"));
                    }
                    first.Add(new LineDocument { LineId = 1, Position = 0, BookId = 1, Text = "alpha" });
                    first.Commit();
                }
                using (var again = other.OpenWriter(30_000_000, 1)) { }
                using (var s = index.Search(new SearchOptions("alpha")))
                    Check(s.HitCount == 1, "the first writer's commit: " + s.HitCount);
            }
        }

        /// <summary>Issue #31: the demo's load command on the fixture: 3 searchers x 4 rounds
        /// while the writer re-indexes 5 lines every 5 ms; at least one commit lands, no error, the index stays built
        /// and the hits are unchanged afterwards.</summary>
        private static void LoadMode(string db, string dir)
        {
            string index = Path.Combine(dir, "load-index");
            string queries = Path.Combine(dir, "load-queries.txt");
            File.WriteAllText(queries, "beta\nalpha beta\t3\n");
            using var engine = new SeforimDbFtsEngine(index, db);
            engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
            int before = engine.Index.Search("beta").Count();
            int code = LoadCommand.Run(engine, new[] { "load", "--queries", queries, "--searchers", "3", "--rounds", "4",
                                                       "--commit-every", "5", "--update-lines", "5", "--update-from", "2" });
            Check(code == 0, "load exit " + code);
            Check(engine.Index.IsBuilt, "still built after the updates");
            Check(engine.Index.Search("beta").Count() == before, "hits unchanged by identical updates");
        }

        private static void AbiCheck(string db, string dir)
        {
            NativeAbi.EnsureLoaded();
            Check(NativeAbi.Loaded == NativeAbi.Expected, "loaded ABI " + NativeAbi.Loaded);
            string path = NativeAbi.LoadedPath;
            Check(path.IndexOf("seforim_search_native", StringComparison.OrdinalIgnoreCase) >= 0, "loaded path: " + path);
            string? message = null;
            try { NativeAbi.Verify(NativeAbi.Loaded, path, NativeAbi.Expected + 1); }
            catch (SeforimEngineException e) { message = e.Message; }
            Check(message != null, "a mismatch must throw");
            Check(message!.Contains("needs ABI " + (NativeAbi.Expected + 1)) && message.Contains("has ABI " + NativeAbi.Expected)
                  && message.Contains(path), "message names both versions and the path: " + message);
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
                Check(ok.FetchPlacedDocuments(Enumerable.Range(1, 10).Select(i => (long)i)).Count() == 10, "fetch 10");
            Check(pool.IdleCount == 1, "pooled after a complete enumeration");

            var a = pool.Rent();
            var conn = a.Connection;
            var e = a.FetchPlacedDocuments(Enumerable.Range(1, 2000).Select(i => (long)i)).GetEnumerator();
            Check(e.MoveNext(), "first row");
            a.Dispose(); // the enumerator is abandoned, never disposed
            Check(pool.IdleCount == 0, "a connection with an open sequence must not be pooled");
            bool threw = false;
            try { e.MoveNext(); } catch (ObjectDisposedException) { threw = true; }
            Check(threw, "the abandoned enumerator must fail, not read a closed/pooled connection");
            using var b = pool.Rent();
            Check(!ReferenceEquals(b.Connection, conn), "the abandoned connection came back");
            Check(b.FetchPlacedDocuments(Enumerable.Range(1, 2000).Select(i => (long)i)).Count() == 2000, "fresh connection works");
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

        /// <summary>Issue #8: a commit through the WAL leaves the main file's stamp alone; the
        /// pool must still see it (PRAGMA data_version) and reopen the idle connection.</summary>
        private static void DataVersionReopens(string db, string dir)
        {
            string copy = Path.Combine(dir, "wal.db");
            File.Copy(db, copy, true);
            using var w = new SqliteConnection($"Data Source={copy};Pooling=False");
            w.Open();
            using (var cmd = w.CreateCommand())
            {
                cmd.CommandText = "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;";
                cmd.ExecuteNonQuery();
            }
            using var pool = new SeforimDbFtsCorpusPool(copy);
            SeforimDbCorpusConnection before;
            using (var a = pool.Rent())
            {
                before = a.Connection;
                Check(a.CountDocuments() == 2 * LinesPerBook, "count before");
            }
            var length = new FileInfo(copy).Length;
            var time = File.GetLastWriteTimeUtc(copy);
            using (var cmd = w.CreateCommand())
            {
                cmd.CommandText = "INSERT INTO line (id, bookId, lineIndex, content) VALUES (99999, 1, 99999, 'alpha new')";
                cmd.ExecuteNonQuery();
            }
            File.SetLastWriteTimeUtc(copy, time);
            Check(new FileInfo(copy).Length == length, "the WAL commit must not grow the main file");
            using var b = pool.Rent();
            Check(!ReferenceEquals(b.Connection, before), "not reopened after a WAL commit");
            Check(b.CountDocuments() == 2 * LinesPerBook + 1, "the reopened connection sees the commit");
        }

        private static byte[] TrainDictionary(int seed)
        {
            var random = new Random(seed);
            var samples = new List<byte[]>();
            for (int i = 0; i < 2000; i++)
                samples.Add(System.Text.Encoding.UTF8.GetBytes(
                    $"line {seed} {i} alpha beta gamma delta {random.Next(1000)} epsilon zeta {random.Next(100)} eta theta"));
            return ZstdSharp.DictBuilder.TrainFromBuffer(samples, 4096).ToArray();
        }

        private static byte[] Compress(byte[] dictionary, string text)
        {
            using var compressor = new ZstdSharp.Compressor(3);
            compressor.LoadDictionary(dictionary);
            return compressor.Wrap(System.Text.Encoding.UTF8.GetBytes(text)).ToArray();
        }

        private static uint DictionaryIdOf(byte[] dictionary) => BitConverter.ToUInt32(dictionary, 4);

        /// <summary>Issue #8 D1 (A): a frame naming a dictionary the codec lacks re-reads
        /// zstd_dict once (a writer appended it through the WAL) and decodes.</summary>
        private static void CodecReloadsNewDictionary(string db, string dir)
        {
            string path = Path.Combine(dir, "dict-wal.db");
            byte[] first = TrainDictionary(1), second = TrainDictionary(2);
            Check(DictionaryIdOf(first) != DictionaryIdOf(second), "two dictionaries with one id");
            using var w = new SqliteConnection($"Data Source={path};Pooling=False");
            w.Open();
            using (var cmd = w.CreateCommand())
            {
                cmd.CommandText = "PRAGMA journal_mode=WAL; CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB); " +
                                  "INSERT INTO zstd_dict (dict) VALUES ($d);";
                cmd.Parameters.AddWithValue("$d", first);
                cmd.ExecuteNonQuery();
            }
            using var r = new SqliteConnection($"Data Source={path};Mode=ReadOnly;Pooling=False");
            r.Open();
            using var codec = SeforimSearch.Demo.SeforimDb.SeforimDbLineContentCodec.Create(r);
            Check(codec.Decode(Compress(first, "alpha one"), 1) == "alpha one", "first dictionary");
            using (var cmd = w.CreateCommand())
            {
                cmd.CommandText = "INSERT INTO zstd_dict (dict) VALUES ($d)";
                cmd.Parameters.AddWithValue("$d", second);
                cmd.ExecuteNonQuery();
            }
            Check(codec.Decode(Compress(second, "beta two"), 2) == "beta two", "the appended dictionary was not picked up");
        }

        /// <summary>Issue #8 D2: base64 is decoded only when it carries a zstd frame header; a
        /// frame that cannot be decoded raises an error naming the line.</summary>
        private static void CodecBase64AndErrors(string db, string dir)
        {
            string path = Path.Combine(dir, "dict-b64.db");
            byte[] known = TrainDictionary(3), unknown = TrainDictionary(4);
            using (var w = new SqliteConnection($"Data Source={path};Pooling=False"))
            {
                w.Open();
                using var cmd = w.CreateCommand();
                cmd.CommandText = "CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB); INSERT INTO zstd_dict (dict) VALUES ($d);";
                cmd.Parameters.AddWithValue("$d", known);
                cmd.ExecuteNonQuery();
            }
            using var r = new SqliteConnection($"Data Source={path};Mode=ReadOnly;Pooling=False");
            r.Open();
            using var codec = SeforimSearch.Demo.SeforimDb.SeforimDbLineContentCodec.Create(r);
            // KLUv-looking text: not base64 at all, and base64 without a frame header
            Check(codec.Decode("KLUv is how it starts", 5) == "KLUv is how it starts", "not base64: stays text");
            Check(codec.Decode("KLUvAAAAAAAA", 5) == "KLUvAAAAAAAA", "base64 without a frame header: stays text");
            Check(codec.Decode(Convert.ToBase64String(Compress(known, "gamma three")), 6) == "gamma three", "a base64 frame decodes");
            foreach (object value in new object[] { Convert.ToBase64String(Compress(unknown, "x")), Compress(unknown, "x") })
            {
                string? message = null;
                try { codec.Decode(value, 4242); }
                catch (SeforimSearch.Demo.SeforimDb.SeforimDbContentCodecException e) { message = e.Message; }
                Check(message != null && message.Contains("4242"), "a frame with a missing dictionary must raise naming the line: " + message);
            }
        }

        private static void EndToEnd(string db, string dir)
        {
            string index = Path.Combine(dir, "index");
            using var engine = new SeforimDbFtsEngine(index, db);
            engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
            Check(engine.Index.IsBuilt, "built");
            string meta = File.ReadAllText(Path.Combine(index, "meta.json"));
            Check(meta.Contains("sort_by_field") && meta.Contains("\"position\""), "index not sorted by position");
            Check(FtsLib.Fts5.TantivySeforimIndex.IndexFormatVersion == "tantivy16", "format version");

            // ABI edges: a helper's text output is pointer + length (an embedded NUL
            // survives); ids live in 0..=long.MaxValue, a negative id is rejected
            Check(SeforimIndex.ReducePairedReadings("a b") == "a b", "embedded NUL in a helper output");
            Check(SeforimIndex.ReducePairedReadings("") == "", "empty helper output");
            string idsDir = Path.Combine(dir, "ids");
            using (var ids = SeforimIndex.Create(idsDir))
            using (var w = ids.OpenWriter(30_000_000, 1))
            {
                bool rejected = false;
                try { w.Delete(-1); } catch (Exception) { rejected = true; }
                Check(rejected, "a negative line id must be rejected");
            }

            // catalog order: book 2 (orderIndex 1) first, then book 1, by lineIndex
            var expected = new List<int>();
            foreach (int book in new[] { 2, 1 })
                for (int k = 0; k < LinesPerBook; k += 3)
                    expected.Add((book - 1) * LinesPerBook + k + 1);

            for (int round = 0; round < 2; round++) // the second round reuses the pooled DB
            {
                var got = new List<long>();
                int snippets = 0;
                foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                             engine, "beta", int.MaxValue, false, 8, null, new FtsTermTable()))
                    foreach (var hit in batch.Hits)
                    {
                        got.Add(hit.LineId);
                        if (hit.SnippetMarks.Length > 0) snippets++;
                    }
                Check(got.SequenceEqual(expected.Select(i => (long)i)), $"round {round}: hits {got.Count}, expected {expected.Count} in catalog order");
                Check(snippets == got.Count, "every hit has a marked snippet");
            }

            // Resume (Core 277de7a9): a run resumed after its N-th hit's line yields exactly the
            // rest of the full run; a line that cannot be placed has no position (restart).
            Check(engine.PositionOfLine(LinesPerBook + 1) == 1, "first catalog line is position 1 (FtsLib's rowid)");
            Check(engine.PositionOfLine(1) == LinesPerBook + 1, "book 1 starts after book 2");
            Check(engine.PositionOfLine(987654) == null, "an unknown line cannot be placed");
            foreach (int n in new[] { 1, 7, expected.Count - 1, expected.Count })
            {
                long? after = engine.PositionOfLine(expected[n - 1]);
                Check(after.HasValue, $"resume line {expected[n - 1]} placed");
                var rest = new List<long>();
                int restSeeded = 0;
                foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                             engine, "beta", int.MaxValue, false, 8, null, new FtsTermTable(), afterPosition: after!.Value))
                {
                    restSeeded += batch.SnippetTexts.Count;
                    foreach (var hit in batch.Hits) rest.Add(hit.LineId);
                }
                Check(expected.Take(n).Select(i => (long)i).Concat(rest).SequenceEqual(expected.Select(i => (long)i)),
                      $"resume after hit {n}: first {n} + {rest.Count} resumed != full {expected.Count}");
                Check(restSeeded == 0, "a resumed run seeds no snippet texts");
            }

            // Snippet texts (Core 5e1091b8): every fixture line has 3 words < 8 context, so
            // every hit is SHORT and its seeded text comes with same-book +-2 neighbours.
            var seeded = new List<FtsSnippetText>();
            int shortHits = 0;
            foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                         engine, "beta", int.MaxValue, false, 8, null, new FtsTermTable()))
            {
                seeded.AddRange(batch.SnippetTexts);
                foreach (var hit in batch.Hits)
                    if ((hit.SnippetFlags & FtsHit.SnippetFlagShort) != 0) shortHits++;
            }
            Check(shortHits == expected.Count, $"short hits {shortHits}, expected {expected.Count}");
            Check(seeded.Count == SeforimDbFtsSearcher.SeedSnippetTextHitCount, $"seeded {seeded.Count}");
            int firstOfBook2 = LinesPerBook + 1;   // book 2, lineIndex 0: no line before it
            var first = seeded.Find(t => t.LineId == firstOfBook2)
                        ?? throw new InvalidOperationException("first hit not seeded");
            Check(first.Before.Length == 0, "no context before a book's first line (book bound)");
            Check(first.After.Split(' ').Length == 6, "after = two neighbour lines joined by a space");
            using (var corpus = engine.OpenCorpus())
            {
                // plain id 4 (book 1, lineIndex 3) and short id 2 + LinesPerBook (book 2, index 1)
                var texts = corpus.FetchSnippetTexts(new long[] { 4 }, new long[] { LinesPerBook + 2 });
                Check(texts.Count == 2, $"snippet texts {texts.Count}");
                var plain = texts.Find(t => t.LineId == 4)!;
                var shortText = texts.Find(t => t.LineId == LinesPerBook + 2)!;
                Check(plain.Before.Length == 0 && plain.After.Length == 0 && plain.Text.Length > 0, "plain line has no context");
                Check(shortText.Before.Split(' ').Length == 3 && shortText.After.Split(' ').Length == 6,
                      "short line: one line before (book start), two after");
            }
        }

        private static List<FtsStreamChunk> StreamAll(SeforimDbFtsIndexCoordinator c, string query,
            long resumeAfterLineId = 0, CancellationToken ct = default)
        {
            var frames = new List<FtsStreamChunk>();
            c.StreamSearch(query, int.MaxValue, false, 8, f => { frames.Add(f); return Task.CompletedTask; },
                ct, resumeAfterLineId).GetAwaiter().GetResult();
            return frames;
        }

        private static void Stream(string db, string dir)
        {
            string index = Path.Combine(dir, "index-stream");
            using var engine = new SeforimDbFtsEngine(index, db);
            var notBuilt = StreamAll(new SeforimDbFtsIndexCoordinator(engine), "beta");
            Check(notBuilt.Count == 1 && !notBuilt[0].Ready && notBuilt[0].Done, "unbuilt index: one Ready=false Done frame");
            engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
            var c = new SeforimDbFtsIndexCoordinator(engine);

            var full = StreamAll(c, "beta");
            Check(full[0].Stage == "validate" && !full[0].Restarted && full[0].Results.Count == 0, "first frame = validate");
            Check(full[full.Count - 1].Done && full[full.Count - 1].Error == null, "last frame = Done");
            Check(full.Count(f => f.Done) == 1, "one Done frame");

            // KitveiHakodesh's snippet model: the first SeedSnippetTextHitCount hits' texts ride on
            // the stream; later rows ask the coordinator (ftsSnippetTexts) through the page cache,
            // at most FetchBatch ids per request, with the list's lookahead.
            var hitsInOrder = full.SelectMany(f => f.Results).ToList();
            var seeded = full.SelectMany(f => f.SnippetTexts).ToList();
            Check(seeded.Select(t => t.LineId).OrderBy(x => x).SequenceEqual(
                      hitsInOrder.Take(SeforimDbFtsSearcher.SeedSnippetTextHitCount).Select(h => h.LineId).OrderBy(x => x)),
                  $"seeded texts = the first {SeforimDbFtsSearcher.SeedSnippetTextHitCount} hits ({seeded.Count})");
            var onDemand = c.SnippetTexts(seeded.Select(t => t.LineId).ToList(), Array.Empty<long>());
            Check(onDemand.Count == seeded.Count && onDemand.All(t => seeded.Any(x => x.LineId == t.LineId && x.Text == t.Text)),
                  "an on-demand read returns the seeded texts");
            var cache = new FtsSnippetTextCache((ids, shorts) => Task.FromResult(c.SnippetTexts(ids, shorts)));
            cache.Seed(seeded);
            cache.EnsureAround(hitsInOrder, 0, 30).GetAwaiter().GetResult();
            Check(cache.Requests == 0, "the first page renders from the seeded texts: " + cache.Requests);
            cache.EnsureAround(hitsInOrder, 600, 30).GetAwaiter().GetResult();
            Check(cache.Requests == 1, "a later page: one request with its lookahead: " + cache.Requests);
            Check(Enumerable.Range(580, 110).All(i => cache.TextOf(hitsInOrder[i].LineId, false) is { Text.Length: > 0 }),
                  "shown rows + 60 ahead + 20 behind cached");
            Check(cache.TextOf(hitsInOrder[579].LineId, false) == null, "nothing before the lookbehind");
            cache.EnsureAround(hitsInOrder, 0, hitsInOrder.Count).GetAwaiter().GetResult();
            Check(cache.Requests == 1 + (hitsInOrder.Count - SeforimDbFtsSearcher.SeedSnippetTextHitCount - 110 + FtsSnippetTextCache.FetchBatch - 1) / FtsSnippetTextCache.FetchBatch,
                  "requests split at FetchBatch ids: " + cache.Requests);
            var fullIds = full.SelectMany(f => f.Results).Select(h => h.LineId).ToList();
            Check(fullIds.Count == 2 * LinesPerBook / 3, $"full stream {fullIds.Count} hits");
            var resultFrames = full.Where(f => f.Results.Count > 0).ToList();
            Check(resultFrames.Count >= 2, "several result frames");
            Check(resultFrames[0].NewTerms.Count > 0, "the first results frame carries the terms");
            Check(full.Skip(1).TakeWhile(f => f.Results.Count == 0).All(f => f.Stage != null && !f.Done),
                  "stage-only frames until the first results frame");

            // Resume after the first chunk(s): first k frames' hits + resumed stream == full stream.
            for (int k = 1; k < resultFrames.Count; k++)
            {
                var held = resultFrames.Take(k).SelectMany(f => f.Results).Select(h => h.LineId).ToList();
                var resumed = StreamAll(c, "beta", held[held.Count - 1]);
                Check(!resumed[0].Restarted, "a placeable line does not restart");
                var rest = resumed.SelectMany(f => f.Results).ToList();
                Check(held.Concat(rest.Select(h => h.LineId)).SequenceEqual(fullIds), $"resume after {k} frame(s) != full");
                Check(resumed.All(f => f.SnippetTexts.Count == 0), "a resumed stream seeds no snippet texts");
                // Term ids restart in the resumed stream: its own NewTerms resolve its own marks.
                var table = resumed.SelectMany(f => f.NewTerms).ToList();
                Check(table.Count > 0 && rest.All(h => Marks(h).All(t => t < table.Count)), "resumed term ids index its own table");
            }

            // An unplaceable resume line: Restarted on the first frame only, and the full results.
            var restarted = StreamAll(c, "beta", 987654);
            Check(restarted[0].Restarted && restarted.Skip(1).All(f => !f.Restarted), "Restarted on the first frame only");
            Check(restarted.SelectMany(f => f.Results).Select(h => h.LineId).SequenceEqual(fullIds), "restart = full results");

            // Cancellation: a cancelled client gets no Done frame and no error.
            using (var cts = new CancellationTokenSource())
            {
                cts.Cancel();
                var cancelled = StreamAll(c, "beta", 0, cts.Token);
                Check(!cancelled.Any(f => f.Done || f.Error != null || f.Results.Count > 0), "cancelled: no results, no Done");
            }
            var empty = StreamAll(c, "  ");
            Check(empty.Count == 1 && empty[0].Done && empty[0].Ready, "blank query: one Done frame");
        }

        private static IEnumerable<int> Marks(FtsHit h)
        {
            for (int i = 2; i < h.SnippetMarks.Length; i += 3) yield return h.SnippetMarks[i];
        }

        // ── Notes books (issue #13, Core 02236a02 NotesBookCheck) ─────────────────────

        /// <summary>Footnotes as virtual notes books on a synthetic schema-7 database (a
        /// <c>line_note</c> table): the id encoding's boundaries, catalog order, a search finding
        /// a note word through the engine, the note's snippet text (no neighbour context), the
        /// hit mapping back to its source line, BOM stripping, unreadable rows counted on the
        /// search path while indexing stays strict, and the id guard.</summary>
        private static void NotesBooks(string unusedDb, string dir)
        {
            const long R = SeforimDb.SeforimDbNoteIds.ReservedStart;
            const long MaxLine = (1L << 36) - 1;
            Check(R == 4503599627370496L, "R = 2^52");
            Check(!SeforimDb.SeforimDbNoteIds.IsVirtual(R - 1) && SeforimDb.SeforimDbNoteIds.IsVirtual(R), "R-1 real, R virtual");
            Check(SeforimDb.SeforimDbNoteIds.EncodeLine(MaxLine, 65535) == (1L << 53) - 1, "lineId 2^36-1 / note 65535 -> 2^53-1 (JS-safe max)");
            Check(SeforimDb.SeforimDbNoteIds.EncodeLine(0, 0) == R, "lineId 0 / note 0 -> R");
            long v = SeforimDb.SeforimDbNoteIds.EncodeLine(10350062, 54);
            Check(SeforimDb.SeforimDbNoteIds.TryDecodeLine(v, out long dl, out int dn) && dl == 10350062 && dn == 54, "line roundtrip");
            long vMax = SeforimDb.SeforimDbNoteIds.EncodeLine(MaxLine, 65535);
            Check(SeforimDb.SeforimDbNoteIds.TryDecodeLine(vMax, out long dlm, out int dnm) && dlm == MaxLine && dnm == 65535, "max line roundtrip");
            Check(SeforimDb.SeforimDbNoteIds.TryDecodeBook(SeforimDb.SeforimDbNoteIds.EncodeBook(8440), out long db8) && db8 == 8440, "book roundtrip");
            Check(SeforimDb.SeforimDbNoteIds.TryDecodeBook(SeforimDb.SeforimDbNoteIds.EncodeBook(R - 1), out long dbm) && dbm == R - 1, "book R-1 roundtrip");
            Check(Throws<ArgumentOutOfRangeException>(() => SeforimDb.SeforimDbNoteIds.EncodeLine(1L << 36, 0)), "lineId 2^36 rejected");
            Check(Throws<ArgumentOutOfRangeException>(() => SeforimDb.SeforimDbNoteIds.EncodeLine(1, 65536)), "noteIndex 65536 rejected");
            Check(Throws<ArgumentOutOfRangeException>(() => SeforimDb.SeforimDbNoteIds.EncodeBook(R)), "book R rejected");
            Check(!Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(R - 1, R - 1, MaxLine, 0, 65535)), "guard: R-1 ok");
            Check(Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(R, 1, 1, 0, 0)), "guard: line.id R rejected");
            Check(Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(1, R, 1, 0, 0)), "guard: book.id R rejected");
            Check(Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(1, 1, 1L << 36, 0, 0)), "guard: note lineId 2^36 rejected");
            Check(Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(1, 1, 1, -1, 0)), "guard: noteIndex -1 rejected");
            Check(Throws<SeforimDb.SeforimDbNoteIdRangeException>(() => SeforimDb.SeforimDbNoteIds.EnsureRepresentable(1, 1, 1, 0, 65536)), "guard: noteIndex 65536 rejected");

            // ---- order, reads, search, hit -> source, unreadable
            string db = CreateNotesDb(dir, "notes",
                "INSERT INTO line (id, bookId, lineIndex, content) VALUES (10, 1, 0, 'alpha beta'), (11, 1, 1, 'gamma delta'), (12, 1, 2, 'epsilon'), (20, 2, 0, 'omega');" +
                "INSERT INTO line_note VALUES (11, 1, 3, 1, char(65279) || 'zetanote kappa'), (11, 0, 1, 1, 'etanote'), (10, 0, 2, 0, 'thetanote');");
            long n10 = SeforimDb.SeforimDbNoteIds.EncodeLine(10, 0), n110 = SeforimDb.SeforimDbNoteIds.EncodeLine(11, 0), n111 = SeforimDb.SeforimDbNoteIds.EncodeLine(11, 1);
            long notesBook = SeforimDb.SeforimDbNoteIds.EncodeBook(1);
            using (var corpus = new SeforimDbFtsCorpus(db))
            {
                var order = corpus.ReadDocumentsWithSectionInResultOrder(0).ToList();
                Check(string.Join(",", order.Select(d => d.Id)) == string.Join(",", new long[] { 10, 11, 12, n10, n110, n111, 20 }),
                      "order: book lines, its notes book in source order, next book: " + string.Join(",", order.Select(d => d.Id)));
                Check(order.Where(d => SeforimDb.SeforimDbNoteIds.IsVirtual(d.Id)).All(d => d.BookId == notesBook && d.SectionId == null),
                      "notes carry the virtual book id and no section");
                Check(order.First(d => d.Id == n111).Text == "zetanote kappa", "BOM stripped from a note body");
                Check(corpus.CountDocuments() == 7, "count includes notes");
                var resumed = corpus.ReadDocumentsInResultOrder(4).Select(d => d.Id).ToList();
                Check(resumed.Count == 3 && resumed[0] == n110, "resume skip lands inside the notes book");
                var ranges = corpus.BookPositionRanges();
                Check(ranges.TryGetValue(notesBook, out var nr) && nr.First == 4 && nr.Count == 3, "notes book position range");
                Check(corpus.PositionOfLine(n111, ranges) == 6, "position of a note");
                Check(corpus.PositionOfLine(20, ranges) == 7, "position of a later real line unchanged");
                var placement = corpus.FetchPlacements(new long[] { n110, 12 });
                Check(placement[n110].BookId == notesBook && placement[12].BookId == 1, "placements: virtual book for a note");
                Check(corpus.GetDocumentText(n10) == "thetanote", "document text of a note");
                var texts = corpus.FetchSnippetTexts(new long[] { n111 }, new long[] { n10 });
                Check(texts.Count == 2 && texts.All(t => t.Before == "" && t.After == ""), "snippet text of a note has no neighbour context");
                Check(texts.First(t => t.LineId == n10).Text == "thetanote", "snippet text = note text");
            }

            string index = Path.Combine(dir, "index-notes");
            using (var engine = new SeforimDbFtsEngine(index, db))
            {
                engine.Index.BuildLines(linesPerCommit: 500, heapBytes: 60_000_000);
                Check(engine.Index.IsBuilt, "index build with notes");

                var hits = NotesSearch(engine, "zetanote", out var clean);
                Check(hits.Count == 1 && hits[0].LineId == n111, "search finds a note word: " + hits.Count);
                var h = hits[0];
                Check(h.IsNote && h.SourceLineId == 11 && h.NoteIndex == 1 && h.SourceBookId == 1 && h.BookId == notesBook,
                      "hit maps back to its source line / note / book");
                var plain = NotesSearch(engine, "gamma", out _);
                Check(plain.Count == 1 && !plain[0].IsNote && plain[0].SourceLineId == 11 && plain[0].NoteIndex == -1 && plain[0].SourceBookId == 1,
                      "plain hit is not a note");
                Check(clean.Count == 0, "no unreadable rows on a clean db");
                Check(engine.PositionOfLine(n111) == 6, "resume position of a note through the engine");

                // A row that went bad after the build: the search counts it instead of failing.
                using (var c = new SqliteConnection("Data Source=" + db + ";Pooling=False"))
                {
                    c.Open();
                    using var cmd = c.CreateCommand();
                    cmd.CommandText = "UPDATE line SET content = X'28B52FFD00' WHERE id = 11; UPDATE line_note SET body = X'28B52FFD00' WHERE lineId = 10;";
                    cmd.ExecuteNonQuery();
                }
                SqliteConnection.ClearAllPools();
                var afterHits = NotesSearch(engine, "gamma", out var bad);
                Check(afterHits.Count == 0 && bad.Count == 1 && bad.LineIds()[0] == 11, "unreadable line counted, search not aborted: " + bad.Count);
                Check(bad.FirstError?.Contains("Line 11") == true, "error names the line: " + bad.FirstError);
                NotesSearch(engine, "thetanote", out var badNote);
                Check(badNote.Count == 1 && badNote.LineIds()[0] == n10, "unreadable note counted by its virtual id");

                // The streamed search reports the total on its Done frame.
                var coordinator = new SeforimDbFtsIndexCoordinator(engine);
                int doneCount = -1, frameIds = 0;
                coordinator.StreamSearch("gamma", int.MaxValue, false, 5, chunk =>
                {
                    frameIds += chunk.UnreadableLineIds.Count;
                    if (chunk.Done) doneCount = chunk.UnreadableRowCount;
                    return Task.CompletedTask;
                }, CancellationToken.None).GetAwaiter().GetResult();
                Check(doneCount == 1 && frameIds == 1, $"stream: unreadable total {doneCount}, frame ids {frameIds}");

                // The facade's exact count and line regex follow the same rule: lenient, counted.
                var countRows = new SeforimDbFtsUnreadableRows();
                bool countThrew = false;
                HitCounts? counts = null;
                try
                {
                    counts = engine.Index.CountByBook("gamma delta", new FtsLib.Fts5.TantivySearchOptions { MaxWordDistance = 5 },
                                                      CancellationToken.None, 4096, out _, countRows);
                }
                catch (SeforimDb.SeforimDbContentCodecException) { countThrew = true; }
                // (the fixture index has positions: the distance is decided from the
                // positions during the search, issue #21, so the count reads no text
                // and the unreadable line 11 counts as the hit it is)
                Check(!countThrew && counts!.Exact && counts.Total == 1 && countRows.Count == 0,
                      $"CountByBook: unreadable row counted, not thrown (threw {countThrew}, rows {countRows.Count})");
                var regexRows = new SeforimDbFtsUnreadableRows();
                bool regexThrew = false;
                try
                {
                    engine.Index.CountByBook("/gamma .* delta/", new FtsLib.Fts5.TantivySearchOptions { UserRegex = true },
                                             CancellationToken.None, 4096, out _, regexRows);
                }
                catch (SeforimDb.SeforimDbContentCodecException) { regexThrew = true; }
                Check(!regexThrew && regexRows.LineIds().Contains(11),
                      $"RunLineRegex: unreadable row counted, not thrown (threw {regexThrew}, rows {regexRows.Count})");
                Check(!Throws<SeforimDb.SeforimDbContentCodecException>(() =>
                          engine.Index.CountByBook("gamma delta", new FtsLib.Fts5.TantivySearchOptions { MaxWordDistance = 5 })),
                      "CountByBook without a rows object is lenient too");
            }
            using (var corpus = new SeforimDbFtsCorpus(db))
            {
                var t = corpus.FetchSnippetTexts(new long[] { 11 }, Array.Empty<long>());
                Check(t.Count == 1 && t[0].Unreadable && t[0].Text == "", "snippet text of an unreadable row is flagged");
                var tn = corpus.FetchSnippetTexts(new long[] { n10 }, Array.Empty<long>());
                Check(tn.Count == 1 && tn[0].Unreadable && tn[0].Text == "", "snippet text of an unreadable note is flagged");
            }

            // ---- indexing stays strict
            string strict = CreateNotesDb(dir, "strict",
                "INSERT INTO line (id, bookId, lineIndex, content) VALUES (10, 1, 0, 'alpha');" +
                "INSERT INTO line_note VALUES (10, 0, 0, 0, X'28B52FFD00');");
            using (var corpus = new SeforimDbFtsCorpus(strict))
            {
                string? message = null;
                try { corpus.ReadDocumentsWithSectionInResultOrder(0).ToList(); }
                catch (SeforimDb.SeforimDbContentCodecException e) { message = e.Message; }
                Check(message?.Contains("line 10") == true, "indexing an unreadable note throws naming its line: " + message);
            }

            // ---- the guard
            string guard = CreateNotesDb(dir, "guard",
                "INSERT INTO line (id, bookId, lineIndex, content) VALUES (4503599627370496, 1, 0, 'alpha');" +
                "INSERT INTO line_note VALUES (5, 0, 0, 0, 'x');");
            using (var corpus = new SeforimDbFtsCorpus(guard))
            {
                string? message = null;
                try { corpus.ReadDocumentsWithSectionInResultOrder(0).ToList(); }
                catch (SeforimDb.SeforimDbNoteIdRangeException e) { message = e.Message; }
                Check(message?.Contains("4503599627370496") == true, "guard fails the build naming the id");
            }

            // ---- BOM on a plain line too (the codec strips it for every row)
            string bom = CreateNotesDb(dir, "bom", "INSERT INTO line (id, bookId, lineIndex, content) VALUES (10, 1, 0, char(65279) || 'alpha');");
            using (var corpus = new SeforimDbFtsCorpus(bom))
                Check(corpus.GetDocumentText(10) == "alpha", "BOM stripped from a line");
        }

        /// <summary>A schema-7-shaped database (Zayit text content plus <c>line_note</c>), two
        /// books in one category, filled by <paramref name="fill"/>.</summary>
        private static string CreateNotesDb(string dir, string name, string fill)
        {
            string path = Path.Combine(dir, name + ".db");
            using var c = new SqliteConnection($"Data Source={path};Pooling=False");
            c.Open();
            using var cmd = c.CreateCommand();
            cmd.CommandText =
                "CREATE TABLE category (id INTEGER PRIMARY KEY NOT NULL, parentId INTEGER, title TEXT NOT NULL," +
                "  level INTEGER NOT NULL DEFAULT 0, orderIndex INTEGER NOT NULL DEFAULT 999);" +
                "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, categoryId INTEGER NOT NULL, sourceId INTEGER NOT NULL DEFAULT 0," +
                "  title TEXT NOT NULL, heRef TEXT, orderIndex INTEGER NOT NULL DEFAULT 999, totalLines INTEGER NOT NULL DEFAULT 0," +
                "  isBaseBook INTEGER NOT NULL DEFAULT 0);" +
                "CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL," +
                "  content TEXT NOT NULL, heRef TEXT, tocEntryId INTEGER);" +
                "CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);" +
                "CREATE TABLE author (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL);" +
                "CREATE TABLE book_author (bookId INTEGER NOT NULL, authorId INTEGER NOT NULL);" +
                "CREATE TABLE line_note (lineId INTEGER NOT NULL, noteIndex INTEGER NOT NULL, charPos INTEGER, hasMarker INTEGER, body BLOB," +
                "  PRIMARY KEY (lineId, noteIndex));" +
                "INSERT INTO category (id, parentId, title, level, orderIndex) VALUES (1, NULL, 'cat', 0, 1);" +
                "INSERT INTO book (id, categoryId, title, orderIndex) VALUES (1, 1, 'first', 1), (2, 1, 'second', 2);" + fill;
            cmd.ExecuteNonQuery();
            return path;
        }

        private static List<FtsHit> NotesSearch(SeforimDbFtsEngine engine, string query, out SeforimDbFtsUnreadableRows unreadable)
        {
            unreadable = new SeforimDbFtsUnreadableRows();
            var hits = new List<FtsHit>();
            foreach (var batch in new SeforimDbFtsSearcher().SearchInBatches(
                         engine, query, 50, false, 5, null, new FtsTermTable(), unreadable: unreadable))
                hits.AddRange(batch.Hits);
            return hits;
        }

        private static bool Throws<T>(Action a) where T : Exception
        {
            try { a(); return false; } catch (T) { return true; }
        }

        /// <summary>Issue #16: the engine package's async API over its own small index.</summary>
        private static void AsyncEngine(string db, string dir)
        {
            string idx = Path.Combine(dir, "async-index");
            using (var index = SeforimIndex.Create(idx))
            {
                using (var w = index.OpenWriter(30000000, 1))
                {
                    for (int i = 0; i < 50; i++)
                        w.Add(new LineDocument { LineId = 100 + i, Position = i, BookId = 1 + i % 3, Text = "gamma x" + i });
                    w.Commit();
                }
                var opts = new SearchOptions("gamma");
                List<long> want;
                using (var s = index.Search(opts)) want = s.AllHits().Select(h => h.LineId).ToList();
                Check(want.Count == 50, "sync hits " + want.Count);

                var got = new List<long>();
                int batches = 0;
                Task.Run(async () =>
                {
                    await foreach (var page in index.SearchBatchesAsync(opts, 7))
                    {
                        batches++;
                        got.AddRange(page.Select(h => h.LineId));
                    }
                }).GetAwaiter().GetResult();
                Check(batches == 8 && got.SequenceEqual(want), "async batches differ from sync hits");

                // a streamed search (issue #21): pulled until complete = the sync hits, exact counts
                using (var s = index.SearchStreaming(opts))
                {
                    while (!s.Pull(s.HitCount + 1)) { }
                    Check(s.IsComplete && s.AllHits().Select(h => h.LineId).SequenceEqual(want), "streamed hits differ");
                    var c = s.GetCounts();
                    Check(c.Exact && c.Total == 50, "streamed counts");
                }
                using (var s = index.SearchStreamingAsync(opts).GetAwaiter().GetResult())
                {
                    while (!s.PullAsync(s.HitCount + 1).GetAwaiter().GetResult()) { }
                    Check(s.HitCount == 50, "streamed async hits " + s.HitCount);
                }

                // session calls on the thread pool
                using (var s = index.SearchAsync(opts).GetAwaiter().GetResult())
                {
                    Check(s.GetHitsAsync(10, 3).GetAwaiter().GetResult().Length == 3, "GetHitsAsync");
                    Check(s.GetCountsAsync().GetAwaiter().GetResult().Total == 50, "GetCountsAsync");
                    Check(s.LocateBatchAsync(new[] { "gamma x0" }).GetAwaiter().GetResult()[0] != null, "LocateBatchAsync");
                }

                // cancel mid-stream: OperationCanceledException, session disposed by the iterator
                using (var cts = new CancellationTokenSource())
                {
                    bool canceled = false;
                    int seen = 0;
                    try
                    {
                        Task.Run(async () =>
                        {
                            await foreach (var page in index.SearchBatchesAsync(opts, 5, cts.Token))
                            {
                                if (++seen == 1) cts.Cancel();
                            }
                        }).GetAwaiter().GetResult();
                    }
                    catch (OperationCanceledException) { canceled = true; }
                    Check(canceled && seen == 1, "cancel mid-stream: canceled " + canceled + ", pages " + seen);
                }

                // abandoned enumeration (break) and a pre-cancelled token
                Task.Run(async () =>
                {
                    await foreach (var page in index.SearchBatchesAsync(opts, 5)) break;
                }).GetAwaiter().GetResult();
                using (var cts = new CancellationTokenSource())
                {
                    cts.Cancel();
                    Check(Throws<OperationCanceledException>(() => index.SearchAsync(opts, cts.Token).GetAwaiter().GetResult()),
                          "a cancelled token must cancel SearchAsync");
                }

                // errors propagate typed
                bool stale = false;
                try
                {
                    index.SearchAsync(new SearchOptions("gamma") { AfterPosition = 3, ResumeStamp = "not-this-index" })
                         .GetAwaiter().GetResult().Dispose();
                }
                catch (SeforimEngineException e) { stale = e.IsStaleResume; }
                Check(stale, "stale resume must surface as SeforimEngineException.IsStaleResume");
            }
        }
    }
}
