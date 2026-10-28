using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using FtsLib;
using Microsoft.Data.Sqlite;
using SeforimSearch.Demo.Common;
using SeforimSearch.Demo.SeforimDb;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The seforim database, served to the FTS engine through its corpus seam
    /// (<see cref="IFtsCorpus"/>) — Core reads the lines, the engine indexes and searches them,
    /// and the engine's own built-in reader is never used.
    ///
    /// The engine is constructed as
    /// <c>Fts5SeforimIndex(indexPath, () => new SeforimDbFtsCorpus(dbPath))</c> and opens no
    /// content database of its own.
    ///
    /// ONE CONNECTION PER LEASE, ONE USER AT A TIME (concurrent searches must never share a
    /// connection). The database is kept OPEN across searches and batches: a
    /// <see cref="SeforimDbFtsCorpusPool"/> keeps <see cref="SeforimDbCorpusConnection"/>s
    /// (connection, schema probe, zstd dictionary, prepared statements) and hands each one out
    /// in a fresh lease, this class. The lease returns its connection EXACTLY ONCE (idempotent
    /// Dispose), refuses every call after that, and its lazy sequences are counted: a
    /// connection whose lease ends with a sequence still open (an abandoned enumerator) is
    /// closed instead of pooled, so nothing can keep a pooled connection busy. A corpus made
    /// with the public constructor owns a private connection, closed on Dispose.
    ///
    /// NOT part of <see cref="SeforimDbQueries"/>, although it reads the same file: that class
    /// opens a connection per method and returns lists, which is right for request/response
    /// queries and wrong for an indexing scan that streams millions of rows over one handle.
    /// Different lifetime, different class.
    /// </summary>
    public sealed class SeforimDbFtsCorpus : IDisposable
    {
        private readonly SeforimDbCorpusConnection _c;
        private readonly SeforimDbFtsCorpusPool? _pool;
        private int _disposed;

        /// <summary>A corpus over its own connection, closed on Dispose (no pool).</summary>
        public SeforimDbFtsCorpus(string databasePath) : this(new SeforimDbCorpusConnection(databasePath), null) { }

        internal SeforimDbFtsCorpus(SeforimDbCorpusConnection connection, SeforimDbFtsCorpusPool? pool)
        {
            _c = connection;
            _pool = pool;
        }

        /// <summary>The leased connection (tests: identity checks).</summary>
        internal SeforimDbCorpusConnection Connection => _c;

        private SeforimDbCorpusConnection C =>
            Volatile.Read(ref _disposed) == 0 ? _c : throw new ObjectDisposedException(nameof(SeforimDbFtsCorpus));

        /// <summary>Wraps a lazy sequence: counted open while enumerated (released in its
        /// finally), and every step checks the lease is still live.</summary>
        private IEnumerable<T> Guard<T>(IEnumerable<T> source)
        {
            var c = _c;
            Interlocked.Increment(ref c.OpenSequences); // counted BEFORE the live check below
            try
            {
                _ = C;
                using var items = source.GetEnumerator();
                while (true)
                {
                    _ = C; // checked BEFORE the step: a dead lease never touches the connection
                    if (!items.MoveNext()) break;
                    yield return items.Current;
                }
            }
            finally
            {
                Interlocked.Decrement(ref c.OpenSequences);
            }
        }

        public long CountDocuments() => C.CountDocuments();
        public long CountDocumentsUpTo(int upToId) => C.CountDocumentsUpTo(upToId);
        public string? GetDocumentText(int id) => C.GetDocumentText(id);
        public IEnumerable<(int Id, string Text)> ReadDocuments(int limit, CancellationToken ct = default) =>
            Guard(C.ReadDocuments(limit, ct));
        public IEnumerable<(int Id, string Text)> ReadDocumentsInResultOrder(long skip, CancellationToken ct = default) =>
            Guard(C.ReadDocumentsInResultOrder(skip, ct));
        public IEnumerable<(int Id, string Text, int BookId)> ReadDocumentsWithBookInResultOrder(long skip,
                                                                                               CancellationToken ct = default) =>
            Guard(C.ReadDocumentsWithBookInResultOrder(skip, ct));
        public IEnumerable<(int Id, string Text, int BookId, long? SectionId)> ReadDocumentsWithSectionInResultOrder(long skip,
                                                                                               CancellationToken ct = default) =>
            Guard(C.ReadDocumentsWithSectionInResultOrder(skip, ct));
        public IEnumerable<(int Id, string Text)> ReadDocumentsAfter(int afterId, int limit = 0, CancellationToken ct = default) =>
            Guard(C.ReadDocumentsAfter(afterId, limit, ct));
        public IEnumerable<(int Id, string Text, string Title)> FetchDocuments(IEnumerable<int> ids) =>
            Guard(C.FetchDocuments(ids));
        public IEnumerable<(int Id, string Text, FtsDocumentPlacement Placement)> FetchPlacedDocuments(IEnumerable<int> ids) =>
            Guard(C.FetchPlacedDocuments(ids));
        public Dictionary<int, FtsDocumentPlacement> FetchPlacements(IReadOnlyList<int> ids) => C.FetchPlacements(ids);
        public List<FtsSnippetText> FetchSnippetTexts(IReadOnlyList<int> lineIds, IReadOnlyCollection<int> shortLineIds) =>
            C.FetchSnippetTexts(lineIds, shortLineIds);

        /// <summary>How many lines per side a SHORT snippet's context reaches (same book only;
        /// Core's SeforimDbFtsCorpus.SnippetNeighbourLineRadius).</summary>
        public const int SnippetNeighbourLineRadius = 2;
        public IDictionary<int, (string Previous, string Next)> FetchNeighbourText(IReadOnlyList<int> ids, int radius) =>
            C.FetchNeighbourText(ids, radius);

        /// <summary>Ends the lease, once: the connection goes back to the pool (kept open) or
        /// is closed (no pool, pool full / disposed, file changed, or a sequence of this lease
        /// was left open). A second Dispose does nothing.</summary>
        public void Dispose()
        {
            if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
            if (_pool != null && _pool.Return(_c)) return;
            _c.Close();
        }
    }

    /// <summary>One open seforim.db: connection, schema, codec and prepared statements. Used
    /// by one lease at a time; the pool keeps it between leases.</summary>
    internal sealed class SeforimDbCorpusConnection
    {
        /// <summary>SQLite's default variable limit is 999; staying under it caps the IN-lists.
        /// How many ids a fetch is asked for at a time is the CALLER's decision (the searcher's
        /// batch plan); this only splits a request that would not fit one statement.</summary>
        private const int SqliteInListChunkSize = 999;

        private readonly SqliteConnection _connection;

        /// <summary>
        /// The database's shape, and the decoder for its line text. Held for the life of this
        /// corpus because it owns ONE connection used for the whole indexing scan: probing the
        /// schema once and loading the zstd dictionary once is the point, over millions of rows.
        /// Both are read-only after construction, matching this class's single-threaded use.
        /// </summary>
        private readonly SeforimDbSchema _schema;
        private readonly SeforimDbLineContentCodec _codec;

        /// <summary>The database file's stamp when it was opened (the pool drops a stale
        /// connection), and how many lazy sequences are open on it (see the lease's Guard).</summary>
        internal readonly SeforimDbFileStamp Stamp;
        internal int OpenSequences;

        /// <summary>Prepared statements kept with the connection, by SQL text (the IN-list
        /// statements differ only by their chunk size). A statement in use (a lazy sequence
        /// still reading it) is not handed out twice: the caller gets a one-off instead.</summary>
        private readonly Dictionary<string, (SqliteCommand Command, string[] Names)> _statements =
            new Dictionary<string, (SqliteCommand, string[])>();
        private readonly HashSet<SqliteCommand> _busy = new HashSet<SqliteCommand>();

        public SeforimDbCorpusConnection(string databasePath)
        {
            Stamp = SeforimDbFileStamp.Of(databasePath);
            _connection = SqliteConnectionFactory.OpenCorpusRead(databasePath);
            _schema = SeforimDbSchema.ForDatabase(_connection, databasePath);
            _codec = SeforimDbLineContentCodec.Create(_connection);

            // Scan tuning on top of the standard corpus-read policy, per connection: an index
            // build reads every line of a multi-gigabyte file in one pass, which earns a bigger
            // page cache than the request/response reads get, and temp_store=MEMORY keeps the
            // neighbour self-join's transient structures off disk. Matches the built-in reader.
            using var command = _connection.CreateCommand();
            command.CommandText =
                "PRAGMA cache_size=-65536;" +   // up to 64 MB page cache
                "PRAGMA temp_store=MEMORY;";
            command.ExecuteNonQuery();
        }

        public long CountDocuments()
        {
            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsCountLines;
            return (long)command.ExecuteScalar()!;
        }

        public long CountDocumentsUpTo(int upToId)
        {
            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsCountLinesUpTo;
            command.Parameters.AddWithValue("@id", upToId);
            return (long)command.ExecuteScalar()!;
        }

        public string? GetDocumentText(int id)
        {
            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsGetLineContent(_schema);
            command.Parameters.AddWithValue("@id", id);
            return _codec.Decode(command.ExecuteScalar());
        }

        public IEnumerable<(int Id, string Text)> ReadDocuments(int limit, CancellationToken ct = default)
        {
            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsReadLines(_schema, limit > 0);
            if (limit > 0) command.Parameters.AddWithValue("@lim", limit);

            using var reader = command.ExecuteReader();
            while (reader.Read())
            {
                ct.ThrowIfCancellationRequested();
                yield return (reader.GetInt32(0), _codec.DecodeColumnOrEmpty(reader, 1));
            }
        }

        /// <summary>
        /// Every line in the order the app shows the library: books in catalog tree order
        /// (<see cref="SeforimDbBookTreeOrder"/>, the frontend's treeOrder), each book's lines
        /// in reading order — so the full-text index stores, and returns, lines in that order.
        /// Lines of a book the catalog does not list (none in the shipped databases) follow at
        /// the end, by book id, so every line is indexed exactly once. <paramref name="skip"/>
        /// counts lines in this order: whole books are skipped by their line counts.
        /// </summary>
        public IEnumerable<(int Id, string Text)> ReadDocumentsInResultOrder(long skip, CancellationToken ct = default)
        {
            foreach (var d in ReadDocumentsWithBookInResultOrder(skip, ct))
                yield return (d.Id, d.Text);
        }

        /// <summary><see cref="ReadDocumentsInResultOrder"/> plus each line's book id, which the
        /// engine index keeps for filtered search by book (the only placement it stores; the
        /// loop already reads book by book, so this costs no extra query). Not in Core: FtsLib
        /// stored no book id.</summary>
        public IEnumerable<(int Id, string Text, int BookId)> ReadDocumentsWithBookInResultOrder(long skip,
                                                                                               CancellationToken ct = default)
        {
            foreach (var d in ReadDocumentsWithSectionInResultOrder(skip, ct))
                yield return (d.Id, d.Text, d.BookId);
        }

        /// <summary><see cref="ReadDocumentsWithBookInResultOrder"/> plus each line's section
        /// (its TOC entry; null when it has none), which the engine keeps for the same-section
        /// scope. Not in Core.</summary>
        public IEnumerable<(int Id, string Text, int BookId, long? SectionId)> ReadDocumentsWithSectionInResultOrder(long skip,
                                                                                               CancellationToken ct = default)
        {
            var treeOrder = SeforimDbBookTreeOrder.Load(_connection);

            var lineCounts = new Dictionary<int, long>();
            using (var count = _connection.CreateCommand())
            {
                count.CommandText = SeforimDbSqlStrings.FtsLineCountsByBook;
                using var r = count.ExecuteReader();
                while (r.Read()) lineCounts[r.GetInt32(0)] = r.GetInt64(1);
            }

            var bookOrder = new List<int>(lineCounts.Keys);
            bookOrder.Sort((a, b) =>
            {
                bool hasA = treeOrder.TryGetValue(a, out int rankA), hasB = treeOrder.TryGetValue(b, out int rankB);
                if (hasA != hasB) return hasA ? -1 : 1;
                return hasA ? rankA.CompareTo(rankB) : a.CompareTo(b);
            });

            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsReadBookLinesWithSection(_schema);
            var pBook = command.Parameters.Add("@b", SqliteType.Integer);
            var pSkip = command.Parameters.Add("@skip", SqliteType.Integer);
            foreach (int bookId in bookOrder)
            {
                long lines = lineCounts[bookId];
                if (skip >= lines) { skip -= lines; continue; }
                pBook.Value = bookId;
                pSkip.Value = skip;
                skip = 0;
                using var reader = command.ExecuteReader();
                while (reader.Read())
                {
                    ct.ThrowIfCancellationRequested();
                    yield return (reader.GetInt32(0), _codec.DecodeColumnOrEmpty(reader, 1), bookId,
                                  reader.IsDBNull(2) ? null : (long?)reader.GetInt64(2));
                }
            }
        }

        public IEnumerable<(int Id, string Text)> ReadDocumentsAfter(int afterId, int limit = 0, CancellationToken ct = default)
        {
            using var command = _connection.CreateCommand();
            command.CommandText = SeforimDbSqlStrings.FtsReadLinesAfter(_schema, limit > 0);
            command.Parameters.AddWithValue("@after", afterId);
            if (limit > 0) command.Parameters.AddWithValue("@lim", limit);

            using var reader = command.ExecuteReader();
            while (reader.Read())
            {
                ct.ThrowIfCancellationRequested();
                yield return (reader.GetInt32(0), _codec.DecodeColumnOrEmpty(reader, 1));
            }
        }

        /// <summary>Search-result content without placement - the plain corpus seam, and the
        /// searcher's verification fetch: id and text only, by primary key from the table that
        /// holds the text (<see cref="SeforimDbSqlStrings.FtsFetchLines"/>). The Title slot is
        /// left empty on purpose: Core enriches hits with their book ID instead and never reads
        /// the engine's title.</summary>
        public IEnumerable<(int Id, string Text, string Title)> FetchDocuments(IEnumerable<int> ids)
        {
            var chunk = new List<int>(SqliteInListChunkSize);

            foreach (int id in ids)
            {
                chunk.Add(id);
                if (chunk.Count == SqliteInListChunkSize)
                {
                    foreach (var row in FetchTextChunk(chunk)) yield return row;
                    chunk.Clear();
                }
            }

            if (chunk.Count > 0)
                foreach (var row in FetchTextChunk(chunk)) yield return row;
        }

        private IEnumerable<(int Id, string Text, string Title)> FetchTextChunk(List<int> ids)
        {
            var (command, parameterNames, cached) =
                Statement(SeforimDbSqlStrings.FtsFetchLines(_schema, ids.Count), ids.Count);
            try
            {
                for (int i = 0; i < ids.Count; i++)
                    command.Parameters[parameterNames[i]].Value = ids[i];

                using var reader = command.ExecuteReader();
                while (reader.Read())
                    yield return (reader.GetInt32(0), _codec.DecodeColumnOrEmpty(reader, 1), "");
            }
            finally
            {
                Release(command, cached);
            }
        }

        /// <summary>
        /// The placement (book id, lineIndex) of each line, no text - what the
        /// searcher reads AFTER verification, for the hits that passed
        /// (<see cref="SeforimDbSqlStrings.FtsFetchPlacements"/>). An id with no line is absent.
        /// </summary>
        public Dictionary<int, FtsDocumentPlacement> FetchPlacements(IReadOnlyList<int> ids)
        {
            var placements = new Dictionary<int, FtsDocumentPlacement>(ids?.Count ?? 0);
            if (ids == null || ids.Count == 0) return placements;

            for (int start = 0; start < ids.Count; start += SqliteInListChunkSize)
            {
                int count = Math.Min(SqliteInListChunkSize, ids.Count - start);
                var (command, parameterNames, cached) =
                    Statement(SeforimDbSqlStrings.FtsFetchPlacements(_schema, count), count);
                try
                {
                    for (int i = 0; i < count; i++)
                        command.Parameters[parameterNames[i]].Value = ids[start + i];

                    using var reader = command.ExecuteReader();
                    while (reader.Read())
                        placements[reader.GetInt32(0)] = new FtsDocumentPlacement(
                            bookId: reader.IsDBNull(1) ? 0 : reader.GetInt32(1),
                            lineIndex: reader.IsDBNull(2) ? 0 : reader.GetInt32(2));
                }
                finally
                {
                    Release(command, cached);
                }
            }
            return placements;
        }

        /// <summary>Search-result content plus each line's placement (book id, lineIndex),
        /// from the one `line` row the content fetch reads - what lets the
        /// searcher finish a hit without a second query. See
        /// <see cref="SeforimDbSqlStrings.FtsFetchPlacedLines"/>.</summary>
        public IEnumerable<(int Id, string Text, FtsDocumentPlacement Placement)> FetchPlacedDocuments(IEnumerable<int> ids)
        {
            var chunk = new List<int>(SqliteInListChunkSize);

            foreach (int id in ids)
            {
                chunk.Add(id);
                if (chunk.Count == SqliteInListChunkSize)
                {
                    foreach (var row in FetchPlacedChunk(chunk)) yield return row;
                    chunk.Clear();
                }
            }

            if (chunk.Count > 0)
                foreach (var row in FetchPlacedChunk(chunk)) yield return row;
        }

        private IEnumerable<(int Id, string Text, FtsDocumentPlacement Placement)> FetchPlacedChunk(List<int> ids)
        {
            var (command, parameterNames, cached) =
                Statement(SeforimDbSqlStrings.FtsFetchPlacedLines(_schema, ids.Count), ids.Count);
            try
            {
                for (int i = 0; i < ids.Count; i++)
                    command.Parameters[parameterNames[i]].Value = ids[i];

                using var reader = command.ExecuteReader();
                while (reader.Read())
                {
                    var placement = new FtsDocumentPlacement(
                        bookId: reader.IsDBNull(2) ? 0 : reader.GetInt32(2),
                        lineIndex: reader.IsDBNull(3) ? 0 : reader.GetInt32(3));
                    yield return (reader.GetInt32(0), _codec.DecodeColumnOrEmpty(reader, 1), placement);
                }
            }
            finally
            {
                Release(command, cached);
            }
        }

        /// <summary>The prepared statement for <paramref name="sql"/> with @p0..@p(n-1), kept
        /// with the connection; a one-off (cached = false) while the kept one is in use.</summary>
        private (SqliteCommand Command, string[] Names, bool Cached) Statement(string sql, int parameterCount)
        {
            if (!_statements.TryGetValue(sql, out var entry))
            {
                var command = _connection.CreateCommand();
                command.CommandText = sql;
                entry = (command, AddChunkParameters(command, parameterCount));
                _statements[sql] = entry;
            }
            if (_busy.Add(entry.Command)) return (entry.Command, entry.Names, true);
            var oneOff = _connection.CreateCommand();
            oneOff.CommandText = sql;
            return (oneOff, AddChunkParameters(oneOff, parameterCount), false);
        }

        private void Release(SqliteCommand command, bool cached)
        {
            if (cached) _busy.Remove(command);
            else command.Dispose();
        }

        /// <summary>
        /// Core's FetchSnippetTexts: the snippet texts of the given lines, ONE statement per
        /// chunk (<see cref="SeforimDbSqlStrings.FtsFetchSnippetLines"/>): every line's own
        /// text, and for the lines in <paramref name="shortLineIds"/> also their non-empty
        /// same-book neighbours within <see cref="SeforimDbFtsCorpus.SnippetNeighbourLineRadius"/>,
        /// each side joined by single spaces in document order. A line with no text row is
        /// absent. A short id need not also appear in <paramref name="lineIds"/>.
        /// </summary>
        public List<FtsSnippetText> FetchSnippetTexts(IReadOnlyList<int> lineIds, IReadOnlyCollection<int> shortLineIds)
        {
            var shortSet = new HashSet<int>(shortLineIds);
            var plain = new List<int>(lineIds.Count);
            foreach (int id in lineIds) if (!shortSet.Contains(id)) plain.Add(id);
            var shortIds = new List<int>(shortSet);

            var texts = new Dictionary<int, string>(plain.Count + shortIds.Count);
            var before = new Dictionary<int, SortedList<int, string>>();
            var after = new Dictionary<int, SortedList<int, string>>();

            // One chunk stays under SQLite's 999 variables (@radius is one more).
            const int perStatement = SqliteInListChunkSize - 1;
            int p = 0, q = 0;
            while (p < plain.Count || q < shortIds.Count)
            {
                int plainCount = Math.Min(perStatement, plain.Count - p);
                int shortCount = Math.Min(perStatement - plainCount, shortIds.Count - q);
                using var command = _connection.CreateCommand();
                command.CommandText = SeforimDbSqlStrings.FtsFetchSnippetLines(_schema, plainCount, shortCount);
                for (int i = 0; i < plainCount; i++) command.Parameters.AddWithValue("@p" + i, plain[p + i]);
                for (int i = 0; i < shortCount; i++) command.Parameters.AddWithValue("@s" + i, shortIds[q + i]);
                if (shortCount > 0) command.Parameters.AddWithValue("@radius", SeforimDbFtsCorpus.SnippetNeighbourLineRadius);
                p += plainCount;
                q += shortCount;

                using var reader = command.ExecuteReader();
                while (reader.Read())
                {
                    int id = reader.GetInt32(0);
                    int delta = reader.GetInt32(1);
                    string text = _codec.DecodeColumnOrEmpty(reader, 2);
                    if (delta == 0) { texts[id] = text; continue; }
                    if (text.Length == 0) continue;   // empty neighbours are skipped, not joined
                    var side = delta < 0 ? before : after;
                    if (!side.TryGetValue(id, out var parts)) side[id] = parts = new SortedList<int, string>();
                    parts[delta] = text;
                }
            }

            var result = new List<FtsSnippetText>(texts.Count);
            foreach (var entry in texts)
            {
                result.Add(new FtsSnippetText
                {
                    LineId = entry.Key,
                    Text = entry.Value,
                    Before = before.TryGetValue(entry.Key, out var b) ? string.Join(" ", b.Values) : "",
                    After = after.TryGetValue(entry.Key, out var a) ? string.Join(" ", a.Values) : "",
                });
            }
            return result;
        }

        public IDictionary<int, (string Previous, string Next)> FetchNeighbourText(
            IReadOnlyList<int> ids, int radius)
        {
            var neighbours = new Dictionary<int, (string Previous, string Next)>(ids?.Count ?? 0);
            foreach (var entry in FetchNeighbourLines(ids, radius))
                neighbours[entry.Key] = (entry.Value.Previous, entry.Value.Next);
            return neighbours;
        }

        /// <summary>
        /// <see cref="FetchNeighbourText"/> plus WHICH lines each side was joined from, in the
        /// same document order. Empty neighbour lines are skipped on both counts, so joining
        /// the ids' texts with single spaces reproduces <c>Previous</c> / <c>Next</c>. One
        /// self-join per chunk of ids; the searcher no longer uses this — it serves its short
        /// snippet context (that is <see cref="FetchSnippetTexts"/>) — so it only backs the
        /// plain corpus seam.
        /// </summary>
        private IDictionary<int, SeforimDbFtsNeighbourLines> FetchNeighbourLines(
            IReadOnlyList<int>? ids, int radius)
        {
            var neighbours = new Dictionary<int, SeforimDbFtsNeighbourLines>(ids?.Count ?? 0);
            if (ids == null || ids.Count == 0 || radius <= 0) return neighbours;

            // Neighbour lines per matched id, keyed by their lineIndex delta, so each side joins
            // back in document order: previous = negative deltas ascending, next = positive.
            var previousParts = new Dictionary<int, SortedList<int, (int Id, string Text)>>();
            var nextParts = new Dictionary<int, SortedList<int, (int Id, string Text)>>();

            using (var command = _connection.CreateCommand())
            {
                string[] parameterNames = AddChunkParameters(command, SqliteInListChunkSize);
                command.Parameters.AddWithValue("@radius", radius);

                for (int start = 0; start < ids.Count; start += SqliteInListChunkSize)
                {
                    int count = Math.Min(SqliteInListChunkSize, ids.Count - start);
                    command.CommandText = SeforimDbSqlStrings.FtsFetchNeighborLines(_schema, count);
                    for (int i = 0; i < count; i++)
                        command.Parameters[parameterNames[i]].Value = ids[start + i];

                    using var reader = command.ExecuteReader();
                    while (reader.Read())
                    {
                        int matchedId = reader.GetInt32(0);
                        int delta = reader.GetInt32(1);
                        string text = _codec.DecodeColumnOrEmpty(reader, 2);
                        if (text.Length == 0) continue;
                        int neighbourId = reader.GetInt32(3);

                        var side = delta < 0 ? previousParts : nextParts;
                        if (!side.TryGetValue(matchedId, out var parts))
                            side[matchedId] = parts = new SortedList<int, (int, string)>();
                        parts[delta] = (neighbourId, text);
                    }
                }
            }

            foreach (int id in ids)
            {
                var entry = new SeforimDbFtsNeighbourLines();
                if (previousParts.TryGetValue(id, out var p)) Collect(p, entry.PreviousIds, out entry.Previous);
                if (nextParts.TryGetValue(id, out var n)) Collect(n, entry.NextIds, out entry.Next);
                if (entry.Previous.Length > 0 || entry.Next.Length > 0) neighbours[id] = entry;
            }

            return neighbours;

            static void Collect(SortedList<int, (int Id, string Text)> parts, List<int> idsOut, out string joined)
            {
                var texts = new List<string>(parts.Count);
                foreach (var part in parts.Values)
                {
                    idsOut.Add(part.Id);
                    texts.Add(part.Text);
                }
                joined = string.Join(" ", texts);
            }
        }

        /// <summary>Pre-registers @p0..@pN once per command, so chunk loops only assign values
        /// instead of rebuilding the parameter collection per chunk.</summary>
        private static string[] AddChunkParameters(SqliteCommand command, int count)
        {
            var names = new string[count];
            for (int i = 0; i < count; i++)
            {
                names[i] = "@p" + i;
                command.Parameters.Add(names[i], SqliteType.Integer);
            }
            return names;
        }

        /// <summary>True when nothing of the last lease is still running on the connection
        /// (no prepared statement marked busy), so it can serve the next one.</summary>
        internal bool IsIdle() => _busy.Count == 0;

        internal void Close()
        {
            foreach (var entry in _statements.Values) entry.Command.Dispose();
            _statements.Clear();
            // The codec holds the database's zstd dictionary, which is megabytes - release it
            // with the connection it belongs to rather than waiting for a collection.
            _codec.Dispose();
            _connection.Dispose();
        }
    }

    /// <summary>The database file's identity for "did it change": size and last write time
    /// of the main file. The -wal file is not stamped: a reader alone can create or touch it,
    /// which would make every corpus look stale. Rent once per caller and Dispose once.</summary>
    internal readonly struct SeforimDbFileStamp : IEquatable<SeforimDbFileStamp>
    {
        private readonly long _length, _ticks;

        private SeforimDbFileStamp(long length, long ticks)
        {
            _length = length; _ticks = ticks;
        }

        public static SeforimDbFileStamp Of(string path)
        {
            var file = new System.IO.FileInfo(path);
            return new SeforimDbFileStamp(
                file.Exists ? file.Length : -1, file.Exists ? file.LastWriteTimeUtc.Ticks : 0);
        }

        public bool Equals(SeforimDbFileStamp o) =>
            _length == o._length && _ticks == o._ticks;
        public override bool Equals(object? obj) => obj is SeforimDbFileStamp o && Equals(o);
        public override int GetHashCode() => (_length, _ticks).GetHashCode();
    }

    /// <summary>
    /// Keeps the seforim.db open across searches: corpora (each one connection, its schema
    /// probe, zstd dictionary and prepared statements) are rented, used by one caller at a
    /// time, and returned on Dispose. Concurrent searches rent different corpora (a new one
    /// is opened when none is idle; up to <see cref="MaxIdle"/> stay open). When the database
    /// file changes (size or write time of the main file), idle corpora are closed and
    /// the next rent opens a fresh one; one rented before the change closes when returned.
    /// </summary>
    public sealed class SeforimDbFtsCorpusPool : IDisposable
    {
        private const int MaxIdle = 4;

        private readonly string _databasePath;
        private readonly Stack<SeforimDbCorpusConnection> _idle = new Stack<SeforimDbCorpusConnection>();
        private readonly object _lock = new object();
        private bool _disposed;

        public SeforimDbFtsCorpusPool(string databasePath)
        {
            _databasePath = databasePath ?? throw new ArgumentNullException(nameof(databasePath));
        }

        /// <summary>How many connections are kept idle (tests).</summary>
        internal int IdleCount { get { lock (_lock) return _idle.Count; } }

        /// <summary>A fresh lease on an open connection; its Dispose returns it here, once.</summary>
        public SeforimDbFtsCorpus Rent()
        {
            var stamp = SeforimDbFileStamp.Of(_databasePath);
            List<SeforimDbCorpusConnection>? stale = null;
            SeforimDbCorpusConnection? connection = null;
            lock (_lock)
            {
                if (_disposed) throw new ObjectDisposedException(nameof(SeforimDbFtsCorpusPool));
                while (_idle.Count > 0)
                {
                    var c = _idle.Pop();
                    if (c.Stamp.Equals(stamp)) { connection = c; break; }
                    (stale ??= new List<SeforimDbCorpusConnection>()).Add(c);
                }
            }
            if (stale != null) foreach (var c in stale) c.Close();
            return new SeforimDbFtsCorpus(connection ?? new SeforimDbCorpusConnection(_databasePath), this);
        }

        /// <summary>Takes a connection back from its (ending) lease; false: the caller closes
        /// it. Refused when a sequence is still open on it (abandoned enumerator), when the
        /// file changed, the pool is full or disposed.</summary>
        internal bool Return(SeforimDbCorpusConnection connection)
        {
            if (Volatile.Read(ref connection.OpenSequences) != 0 || !connection.IsIdle()) return false;
            bool current = connection.Stamp.Equals(SeforimDbFileStamp.Of(_databasePath));
            lock (_lock)
            {
                if (_disposed || !current || _idle.Count >= MaxIdle) return false;
                _idle.Push(connection);
                return true;
            }
        }

        public void Dispose()
        {
            SeforimDbCorpusConnection[] idle;
            lock (_lock)
            {
                _disposed = true;
                idle = _idle.ToArray();
                _idle.Clear();
            }
            foreach (var c in idle) c.Close();
        }
    }

    /// <summary>One matched line's same-book neighbours: each side's joined text and the ids
    /// it was joined from, in document order. See <see cref="SeforimDbFtsCorpus.FetchNeighbourText"/>.</summary>
    internal sealed class SeforimDbFtsNeighbourLines
    {
        public string Previous = "";
        public string Next = "";
        public readonly List<int> PreviousIds = new List<int>();
        public readonly List<int> NextIds = new List<int>();
    }
}
