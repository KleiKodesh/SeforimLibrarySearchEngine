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
    /// ONE CONNECTION PER INSTANCE, ONE USER AT A TIME (concurrent searches must never share
    /// a connection). The database is kept OPEN across searches and batches: instances come
    /// from a <see cref="SeforimDbFtsCorpusPool"/>, and Dispose hands the instance back to its
    /// pool (connection, schema probe, zstd dictionary and prepared statements kept) instead
    /// of closing it. A corpus made without a pool closes on Dispose.
    ///
    /// NOT part of <see cref="SeforimDbQueries"/>, although it reads the same file: that class
    /// opens a connection per method and returns lists, which is right for request/response
    /// queries and wrong for an indexing scan that streams millions of rows over one handle.
    /// Different lifetime, different class.
    /// </summary>
    public sealed class SeforimDbFtsCorpus : IDisposable
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

        /// <summary>The pool this corpus returns to on Dispose (null: Dispose closes it), and
        /// the database file's stamp when it was opened (the pool drops a stale corpus).</summary>
        internal SeforimDbFtsCorpusPool? Pool;
        internal SeforimDbFileStamp Stamp;

        /// <summary>Prepared statements kept with the connection, by SQL text (the IN-list
        /// statements differ only by their chunk size). A statement in use (a lazy sequence
        /// still reading it) is not handed out twice: the caller gets a one-off instead.</summary>
        private readonly Dictionary<string, (SqliteCommand Command, string[] Names)> _statements =
            new Dictionary<string, (SqliteCommand, string[])>();
        private readonly HashSet<SqliteCommand> _busy = new HashSet<SqliteCommand>();

        public SeforimDbFtsCorpus(string databasePath)
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

        /// <summary>Search-result content without placement - the plain corpus seam. The
        /// Title slot is left empty on purpose: Core enriches hits with their book ID instead
        /// and never reads the engine's title. The engine's own diagnostics run over the test
        /// corpus, which does fill it.</summary>
        public IEnumerable<(int Id, string Text, string Title)> FetchDocuments(IEnumerable<int> ids)
        {
            foreach (var (id, text, _) in FetchPlacedDocuments(ids)) yield return (id, text, "");
        }

        /// <summary>Search-result content plus each line's placement (book id, lineIndex, own
        /// TOC entry), from the one `line` row the content fetch reads - what lets the
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
                        lineIndex: reader.IsDBNull(3) ? 0 : reader.GetInt32(3),
                        tocEntryId: reader.GetInt32(4));
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
        /// The lines at the given (bookId, lineIndex) positions: id, position and text. One
        /// statement per 499 positions (two parameters each, under SQLite's 999 limit). What
        /// the searcher asks for a batch's short hits' neighbours — the positions it could
        /// not already serve from the batch's own fetched content. A position with no line
        /// is absent; empty lines are returned (the caller skips them, as the self-join did).
        /// </summary>
        public List<SeforimDbFtsLineAtPosition> FetchLinesAtBookPositions(
            IReadOnlyList<(int BookId, int LineIndex)> positions)
        {
            var lines = new List<SeforimDbFtsLineAtPosition>(positions?.Count ?? 0);
            if (positions == null || positions.Count == 0) return lines;

            const int positionsPerStatement = SqliteInListChunkSize / 2;

            for (int start = 0; start < positions.Count; start += positionsPerStatement)
            {
                int count = Math.Min(positionsPerStatement, positions.Count - start);
                var (command, parameterNames, cached) =
                    Statement(SeforimDbSqlStrings.FtsFetchLinesAtBookPositions(_schema, count), count * 2);
                try
                {
                for (int i = 0; i < count; i++)
                {
                    command.Parameters[parameterNames[2 * i]].Value = positions[start + i].BookId;
                    command.Parameters[parameterNames[2 * i + 1]].Value = positions[start + i].LineIndex;
                }

                using var reader = command.ExecuteReader();
                while (reader.Read())
                {
                    lines.Add(new SeforimDbFtsLineAtPosition(
                        id: reader.GetInt32(0),
                        bookId: reader.GetInt32(1),
                        lineIndex: reader.GetInt32(2),
                        text: _codec.DecodeColumnOrEmpty(reader, 3)));
                }
                }
                finally
                {
                    Release(command, cached);
                }
            }
            return lines;
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
        /// hits' neighbours from the batch's own content plus
        /// <see cref="FetchLinesAtBookPositions"/> — so it only backs the plain corpus seam.
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

        /// <summary>Back to the pool (kept open), or closed when there is no pool or the pool
        /// does not take it back (disposed, full, or the database file changed).</summary>
        public void Dispose()
        {
            if (Pool != null && Pool.Return(this)) return;
            Close();
        }

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
        private readonly Stack<SeforimDbFtsCorpus> _idle = new Stack<SeforimDbFtsCorpus>();
        private readonly object _lock = new object();
        private bool _disposed;

        public SeforimDbFtsCorpusPool(string databasePath)
        {
            _databasePath = databasePath ?? throw new ArgumentNullException(nameof(databasePath));
        }

        /// <summary>An open corpus for one caller; Dispose returns it here.</summary>
        public SeforimDbFtsCorpus Rent()
        {
            var stamp = SeforimDbFileStamp.Of(_databasePath);
            List<SeforimDbFtsCorpus>? stale = null;
            SeforimDbFtsCorpus? corpus = null;
            lock (_lock)
            {
                if (_disposed) throw new ObjectDisposedException(nameof(SeforimDbFtsCorpusPool));
                while (_idle.Count > 0)
                {
                    var c = _idle.Pop();
                    if (c.Stamp.Equals(stamp)) { corpus = c; break; }
                    (stale ??= new List<SeforimDbFtsCorpus>()).Add(c);
                }
            }
            if (stale != null) foreach (var c in stale) c.Close();
            corpus ??= new SeforimDbFtsCorpus(_databasePath);
            corpus.Pool = this;
            return corpus;
        }

        internal bool Return(SeforimDbFtsCorpus corpus)
        {
            bool current = corpus.Stamp.Equals(SeforimDbFileStamp.Of(_databasePath));
            lock (_lock)
            {
                if (_disposed || !current || _idle.Count >= MaxIdle || _idle.Contains(corpus)) return false;
                _idle.Push(corpus);
                return true;
            }
        }

        public void Dispose()
        {
            SeforimDbFtsCorpus[] idle;
            lock (_lock)
            {
                _disposed = true;
                idle = _idle.ToArray();
                _idle.Clear();
            }
            foreach (var c in idle) c.Close();
        }
    }

    /// <summary>One line of the library with its position: what
    /// <see cref="SeforimDbFtsCorpus.FetchLinesAtBookPositions"/> returns.</summary>
    public readonly struct SeforimDbFtsLineAtPosition
    {
        public SeforimDbFtsLineAtPosition(int id, int bookId, int lineIndex, string text)
        {
            Id = id;
            BookId = bookId;
            LineIndex = lineIndex;
            Text = text;
        }

        public int Id { get; }
        public int BookId { get; }
        public int LineIndex { get; }
        public string Text { get; }
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
