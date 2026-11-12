using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;
using System.Threading;
using SeforimSearch.Demo.SeforimDb;
using SeforimSearch.Engine;

namespace FtsLib.Fts5
{
    /// <summary>What <see cref="TantivySeforimIndex.UpdateChangedBooks"/> did.</summary>
    public sealed class BookUpdateReport
    {
        /// <summary>False: a per-book update cannot serve this change (no complete index, the
        /// index format or DB path changed, the DB has no <c>book.contentHash</c>, or an index
        /// built before issue #29 whose DB already changed): rebuild with <c>BuildLines</c>.</summary>
        public bool Applicable;
        public string Reason = "";
        public int Changed, Added, Removed;
        public long LinesWritten;
        /// <summary>A line count changed or a book was added / removed: the background merge
        /// is a repack in the catalog order (positions renumbered).</summary>
        public bool Repack;
        public bool MergeStarted;
        public TimeSpan Elapsed;

        public override string ToString() => !Applicable ? "per-book update not applicable: " + Reason
            : $"{Changed} changed, {Added} added, {Removed} removed books; {LinesWritten:N0} lines written; " +
              $"background {(Repack ? "repack" : "merge")} {(MergeStarted ? "started" : "not needed")}; {Elapsed.TotalMilliseconds:F0} ms";
    }

    public sealed partial class TantivySeforimIndex
    {
        /// <summary>Lines per commit of a per-book update: a commit always ends on a book boundary
        /// (issue #32: never commit mid-book); one book larger than this is one commit.</summary>
        public int UpdateLinesPerCommit { get; set; } = 20000;

        /// <summary>
        /// The per-book update (issue #29, Otzaria's rule): compares each book's
        /// <c>book.contentHash</c> in seforim.db with the hashes the index recorded in its payload,
        /// then for every changed, new or removed book deletes all its lines (and its virtual notes
        /// book's, issue #13) and adds its current lines; then starts the engine's background full
        /// merge. Call it on app start and whenever the DB file changes.
        ///
        /// Positions: a written book's lines get their catalog positions in the CURRENT DB
        /// numbering (<see cref="ReadLineDocuments"/>). When no line count changed and no book was
        /// added or removed, that numbering equals the index's, so a plain merge keeps every
        /// position. Otherwise the numbering of the books after the change moved: the merge is a
        /// REPACK in the current catalog order (the engine's re-sort renumbers every line densely,
        /// = what a fresh build gives), recorded with the engine BEFORE the first write so a crash
        /// mid-update still repacks. Until the merge publishes, the index has several segments and
        /// the hits are sorted by position (the written books' lines may interleave with their
        /// neighbours' for that while).
        ///
        /// Crash safety: the books to write are recorded in the payload (<c>Pending</c>) before the
        /// first one; each commit holds whole books only, with their new hashes, so a process that
        /// ends mid-update redoes the remaining books at the next call (delete + add is idempotent).
        /// Counts stay "exact" while books are pending (Otzaria's behaviour, no stale flag).
        /// </summary>
        public BookUpdateReport UpdateChangedBooks(CancellationToken ct = default, Action<string>? log = null)
        {
            var sw = System.Diagnostics.Stopwatch.StartNew();
            var report = new BookUpdateReport();
            BookUpdateReport NotApplicable(string why) { report.Applicable = false; report.Reason = why; return report; }

            var state = ReadBuildState();
            if (state == null || !state.Complete) return NotApplicable("no complete index");
            string fingerprint = SeforimDbContentFingerprint.Compute(_seforimDbPath, IndexFormatVersion);
            string? change = SeforimDbContentFingerprint.DescribeChange(state.Fingerprint, fingerprint);
            if (change != null && !change.StartsWith("seforim DB content changed", StringComparison.Ordinal))
                return NotApplicable(change);

            using var corpus = _openCorpus();
            var hashes = corpus.BookContentHashes();
            if (hashes == null) return NotApplicable("the database has no book.contentHash");
            var ranges = corpus.BookPositionRanges();
            var current = new Dictionary<long, BookStamp>(hashes.Count);
            foreach (var kv in hashes)
            {
                long lines = ranges.TryGetValue(kv.Key, out var r) ? r.Count : 0;
                long notes = ranges.TryGetValue(SeforimDbNoteIds.EncodeBook(kv.Key), out var n) ? n.Count : 0;
                current[kv.Key] = new BookStamp(kv.Value, lines, notes);
            }

            var order = corpus.CatalogBookOrder();
            string orderStamp = OrderStamp(order);
            report.Applicable = true;
            if (state.Books == null)
            {
                // An index built before the per-book record: adopt the DB's hashes when it still
                // is the database the index was built from; otherwise nothing tells which books changed.
                if (change != null) return NotApplicable("index has no per-book record and the DB changed: " + change);
                state.Books = FormatBooks(current, orderStamp);
                using (var w = Engine().OpenWriter(30_000_000, 1)) w.Commit(state.Serialize());
                log?.Invoke($"recorded {current.Count:N0} book hashes");
                report.Elapsed = sw.Elapsed;
                return report;
            }

            var recorded = ParseBooks(state.Books, out string? recordedOrder);
            var todo = new HashSet<long>(ParsePending(state.Pending));
            foreach (var kv in current)
                if (!recorded.TryGetValue(kv.Key, out var was) || was.Hash != kv.Value.Hash) todo.Add(kv.Key);
            foreach (long b in recorded.Keys)
                if (!current.ContainsKey(b)) todo.Add(b);
            if (todo.Count == 0)
            {
                // the DB file changed but no book did (e.g. catalog edits): only the stamp moves.
                // A recorded repack the engine has not finished resumes.
                if (change != null)
                    using (var w = Engine().OpenWriter(30_000_000, 1)) { state.Fingerprint = fingerprint; w.Commit(state.Serialize()); }
                report.MergeStarted = Engine().StartBackgroundMerge();
                report.Elapsed = sw.Elapsed;
                return report;
            }

            foreach (long b in todo)
            {
                bool inDb = current.TryGetValue(b, out var now), wasIndexed = recorded.TryGetValue(b, out var was);
                if (inDb && wasIndexed) report.Changed++;
                else if (inDb) report.Added++;
                else report.Removed++;
                if (!inDb || !wasIndexed || now.Lines != was.Lines || now.Notes != was.Notes) report.Repack = true;
            }
            // the books' tree order changed since the index's numbering: the written books' new
            // positions would overlap the others' old ones, so the update repacks (alone, an order
            // change is not acted on: re-sorting stays the host's explicit choice)
            if (recordedOrder != orderStamp) report.Repack = true;
            // books in catalog order (removed ones first: they only delete)
            var rank = new Dictionary<long, int>(order.Count);
            for (int i = 0; i < order.Count; i++) rank[order[i]] = i;
            var books = todo.OrderBy(b => rank.TryGetValue(b, out int r) ? r : -1).ThenBy(b => b).ToList();
            log?.Invoke($"per-book update: {report.Changed} changed, {report.Added} added, {report.Removed} removed" +
                        (report.Repack ? " (line counts changed: repack after)" : ""));

            long[]? repackOrder = report.Repack ? order.ToArray() : null;
            using (var writer = Engine().OpenWriter(UpdateLinesPerCommit > 0 ? 64_000_000 : 30_000_000, 2))
            {
                if (repackOrder != null) Engine().RecordBackgroundMerge(repackOrder);
                state.Pending = string.Join(",", books.Select(b => b.ToString(CultureInfo.InvariantCulture)));
                writer.Commit(state.Serialize());

                var remaining = new List<long>(books);
                long sinceCommit = 0;
                int done = 0;
                foreach (long book in books)
                {
                    ct.ThrowIfCancellationRequested();
                    writer.DeleteBook(book);
                    writer.DeleteBook(SeforimDbNoteIds.EncodeBook(book));
                    if (current.TryGetValue(book, out var stamp))
                    {
                        foreach (long id in new[] { book, SeforimDbNoteIds.EncodeBook(book) })
                        {
                            if (!ranges.TryGetValue(id, out var range) || range.Count == 0) continue;
                            // the book's run in the current numbering (FtsLib's from 1, the engine's from 0)
                            long position = range.First - 1;
                            foreach (var (lineId, text, sectionId) in corpus.ReadBookDocumentsWithSection(id, ct))
                            {
                                writer.Add(ToLineDocument(lineId, position++, id, text, sectionId));
                                sinceCommit++;
                                report.LinesWritten++;
                            }
                        }
                        recorded[book] = stamp;
                    }
                    else recorded.Remove(book);
                    remaining.Remove(book);
                    done++;
                    // whole books only per commit, with their new hashes
                    if (sinceCommit >= UpdateLinesPerCommit || remaining.Count == 0)
                    {
                        state.Books = FormatBooks(recorded, report.Repack ? orderStamp : recordedOrder);
                        state.Pending = string.Join(",", remaining.Select(b => b.ToString(CultureInfo.InvariantCulture)));
                        if (remaining.Count == 0)
                        {
                            state.Pending = null;
                            state.Fingerprint = fingerprint;
                            long total = current.Values.Sum(s => s.Lines + s.Notes);
                            state.TotalLines = total;
                            state.LinesDone = total;
                        }
                        writer.Commit(state.Serialize());
                        sinceCommit = 0;
                        log?.Invoke($"  {done:N0} / {books.Count:N0} books, {report.LinesWritten:N0} lines");
                    }
                }
            }
            // the engine picks up a recorded repack's order
            report.MergeStarted = Engine().StartBackgroundMerge(repackOrder);
            report.Elapsed = sw.Elapsed;
            return report;
        }

        /// <summary>The background merge of the engine index (issue #29): see
        /// <see cref="SeforimIndex.StartBackgroundMerge"/>.</summary>
        public bool BackgroundMergeRunning => Engine().BackgroundMergeRunning;

        /// <summary>Waits for the background merge; throws its error.</summary>
        public void WaitBackgroundMerge() => Engine().WaitBackgroundMerge();

        private readonly struct BookStamp
        {
            public readonly long Hash, Lines, Notes;
            public BookStamp(long hash, long lines, long notes) { Hash = hash; Lines = lines; Notes = notes; }
        }

        /// <summary>A stamp of the catalog book order the index's positions follow (FNV-1a).</summary>
        private static string OrderStamp(List<long> order)
        {
            ulong h = 14695981039346656037UL;
            foreach (long id in order) { h ^= (ulong)id; h *= 1099511628211UL; }
            return h.ToString("x16", CultureInfo.InvariantCulture);
        }

        private static string FormatBooks(Dictionary<long, BookStamp> books, string? orderStamp)
        {
            var sb = new StringBuilder(books.Count * 32);
            if (orderStamp != null) sb.Append('@').Append(orderStamp);
            foreach (var kv in books.OrderBy(k => k.Key))
            {
                if (sb.Length > 0) sb.Append(';');
                sb.Append(kv.Key.ToString(CultureInfo.InvariantCulture)).Append(':')
                  .Append(kv.Value.Hash.ToString(CultureInfo.InvariantCulture)).Append(':')
                  .Append(kv.Value.Lines.ToString(CultureInfo.InvariantCulture)).Append(':')
                  .Append(kv.Value.Notes.ToString(CultureInfo.InvariantCulture));
            }
            return sb.ToString();
        }

        private static Dictionary<long, BookStamp> ParseBooks(string text, out string? orderStamp)
        {
            var books = new Dictionary<long, BookStamp>();
            orderStamp = null;
            foreach (string item in text.Split(new[] { ';' }, StringSplitOptions.RemoveEmptyEntries))
            {
                if (item[0] == '@') { orderStamp = item.Substring(1); continue; }
                var p = item.Split(':');
                if (p.Length != 4) continue;
                books[long.Parse(p[0], CultureInfo.InvariantCulture)] = new BookStamp(
                    long.Parse(p[1], CultureInfo.InvariantCulture), long.Parse(p[2], CultureInfo.InvariantCulture),
                    long.Parse(p[3], CultureInfo.InvariantCulture));
            }
            return books;
        }

        private static IEnumerable<long> ParsePending(string? text) =>
            string.IsNullOrEmpty(text) ? Enumerable.Empty<long>()
                : text!.Split(new[] { ',' }, StringSplitOptions.RemoveEmptyEntries).Select(s => long.Parse(s, CultureInfo.InvariantCulture));
    }
}
