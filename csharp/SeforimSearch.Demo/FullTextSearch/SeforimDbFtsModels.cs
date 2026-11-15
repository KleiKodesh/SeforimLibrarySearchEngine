using System.Collections.Generic;
using FtsLib.Fts5;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// One full-text hit (KitveiHakodesh Core's FtsHit, same fields): the line,
    /// its book, and the snippet DESCRIPTOR located once, by the search's own check,
    /// over the MATCHED LINE's text alone: the page renders the window
    /// [<see cref="SnippetStart"/>, <see cref="SnippetEnd"/>) of that text with
    /// <see cref="SnippetMarks"/> highlighted. A <see cref="SnippetFlagShort"/> window
    /// also gets plain context from the neighbour lines (<see cref="FtsSnippetText"/>),
    /// presentation only, never part of the descriptor. No TOC entry: the page reads
    /// row headers by line id for the rows it renders.
    /// </summary>
    public sealed class FtsHit
    {
        public long LineId { get; set; }
        public long BookId { get; set; }
        public int Score { get; set; }
        public int WordDistance { get; set; }
        public int SnippetStart { get; set; }
        public int SnippetEnd { get; set; }
        /// <summary>(start, length, termId) triples; termId into the search's <see cref="FtsTermTable"/>.</summary>
        public int[] SnippetMarks { get; set; } = System.Array.Empty<int>();
        public int SnippetFlags { get; set; }

        /// <summary>Index with positions: the window decided during the search, as word
        /// positions (word-level descriptor). Null once the descriptor above is final (always
        /// on an index without positions). Demo host side: not part of Core's DTO.</summary>
        public SeforimSearch.Engine.WordWindow? Window { get; set; }

        /// <summary>Turns <see cref="Window"/> into the final descriptor with the line text
        /// (<see cref="FtsWindowFinisher.Finish"/>). Null when nothing is pending.</summary>
        public FtsWindowFinisher? Finisher { get; set; }

        /// <summary>The snippet descriptor still waits for the line text (<see cref="Window"/>).</summary>
        public bool IsPending => Window != null;

        public const int SnippetFlagLeadingEllipsis = 1;
        public const int SnippetFlagTrailingEllipsis = 2;
        /// <summary>The window holds fewer words than the requested context: the line is too
        /// short to fill it, so the page fetches its neighbour lines with it.</summary>
        public const int SnippetFlagShort = 4;

        // Hit -> source (issue #13, Core 02236a02): a hit in a virtual notes book (LineId >= 2^30,
        // see SeforimDb.SeforimDbNoteIds) opens its SOURCE line with the note shown. Derived
        // from the ids.

        /// <summary>The hit is a footnote (a line of a virtual notes book).</summary>
        public bool IsNote => SeforimDb.SeforimDbNoteIds.IsVirtual(LineId);
        /// <summary>The line to open: the note's source line, else <see cref="LineId"/>.</summary>
        public long SourceLineId => SeforimDb.SeforimDbNoteIds.SourceLineOf(LineId);
        /// <summary>The note's index in its source line; -1 for a plain line.</summary>
        public int NoteIndex => SeforimDb.SeforimDbNoteIds.TryDecodeLine(LineId, out _, out int n) ? n : -1;
        /// <summary>The book to open: the notes book's source book, else <see cref="BookId"/>.</summary>
        public long SourceBookId => SeforimDb.SeforimDbNoteIds.SourceBookOf(BookId);
    }

    /// <summary>
    /// A snippet's TEXT (Core's FtsSnippetText): the matched line's own text and, for a short
    /// window only, the plain context around it - the same book's non-empty lines up to
    /// <see cref="SeforimDbFtsCorpus.SnippetNeighbourLineRadius"/> before and after, each side
    /// joined by single spaces in document order. Empty sides for a line that is not short.
    /// </summary>
    public sealed class FtsSnippetText
    {
        public long LineId { get; set; }
        public string Text { get; set; } = "";
        public string Before { get; set; } = "";
        public string After { get; set; } = "";
        /// <summary>The line's row could not be decoded: <see cref="Text"/> is empty (issue #13).</summary>
        public bool Unreadable { get; set; }
    }

    /// <summary>One pushed frame of a streaming search (Core's FtsStreamChunk, same fields).
    /// The final frame has <c>Done</c> true. <c>Ready</c> false = index not built: sent as
    /// the first and only frame.</summary>
    public sealed class FtsStreamChunk
    {
        public bool Ready { get; set; } = true;
        public List<FtsHit> Results { get; set; } = new List<FtsHit>();
        /// <summary>Matched-term strings first seen in this frame, in id order: the receiver
        /// appends them to its table, which the hits' mark term ids index into.</summary>
        public List<string> NewTerms { get; set; } = new List<string>();
        /// <summary>Snippet texts for the first hits of the search only
        /// (<see cref="SeforimDbFtsSearcher.SeedSnippetTextHitCount"/>); a resumed search seeds none.</summary>
        public List<FtsSnippetText> SnippetTexts { get; set; } = new List<FtsSnippetText>();
        public bool Done { get; set; }
        public string? Error { get; set; }
        /// <summary>A RESUMED search whose resume line could not be placed any more, so it runs
        /// from the start instead: the receiver drops the hits it had. First frame only.</summary>
        public bool Restarted { get; set; }
        /// <summary>What the engine is doing (validate, parse, expand, lookup, fetch, snippets,
        /// enrich). Until the first results frame a stage change is a frame of its own; after
        /// it, the latest stage rides on the next results frame.</summary>
        public string? Stage { get; set; }
        /// <summary>Matched rows first found undecodable in this frame (skipped as empty, issue
        /// #13); the receiver sums them. The Done frame carries the total in
        /// <see cref="UnreadableRowCount"/> with the first error.</summary>
        public List<long> UnreadableLineIds { get; set; } = new List<long>();
        public int UnreadableRowCount { get; set; }
        public string? UnreadableError { get; set; }
    }

    /// <summary>One search's matched-term strings, numbered in first-seen order
    /// (Core's FtsTermTable). Thread-safe.</summary>
    public sealed class FtsTermTable
    {
        private readonly System.Collections.Concurrent.ConcurrentDictionary<string, int> _ids =
            new System.Collections.Concurrent.ConcurrentDictionary<string, int>();
        private readonly List<string> _terms = new List<string>();
        private int _sent;

        public int IdOf(string term)
        {
            if (_ids.TryGetValue(term, out int id)) return id;
            lock (_terms)
            {
                if (_ids.TryGetValue(term, out id)) return id;
                id = _terms.Count;
                _terms.Add(term);
                _ids[term] = id;
                return id;
            }
        }

        public List<string> TakeNewTerms()
        {
            lock (_terms)
            {
                var fresh = _terms.GetRange(_sent, _terms.Count - _sent);
                _sent = _terms.Count;
                return fresh;
            }
        }

        public List<string> All()
        {
            lock (_terms) return new List<string>(_terms);
        }
    }

    /// <summary>
    /// Core's engine handle (SeforimDbFtsEngine): the index plus a way to open
    /// the corpus. One instance serves searches and the build.
    /// </summary>
    public sealed class SeforimDbFtsEngine : System.IDisposable
    {
        /// <summary>The seforim.db kept open across searches (see SeforimDbFtsCorpusPool).</summary>
        private readonly SeforimDbFtsCorpusPool _corpora;

        public SeforimDbFtsEngine(string indexPath, string seforimDbPath)
        {
            _corpora = new SeforimDbFtsCorpusPool(seforimDbPath);
            Index = new TantivySeforimIndex(indexPath, seforimDbPath, OpenCorpus);
        }

        public TantivySeforimIndex Index { get; }

        /// <summary>An open corpus from the pool; Dispose returns it (the database stays open).</summary>
        public SeforimDbFtsCorpus OpenCorpus() => _corpora.Rent();

        private readonly object _lock = new object();
        private System.Collections.Generic.Dictionary<long, (long First, long Count)>? _bookPositions;

        /// <summary>Forgets the cached book position runs (call after a per-book index update,
        /// issue #29: the catalog numbering may have moved).</summary>
        public void InvalidateBookPositions()
        {
            lock (_lock) _bookPositions = null;
        }

        /// <summary>
        /// The index position of a line (<see cref="SeforimDbFtsCorpus.PositionOfLine"/>), to
        /// resume a search after it. Null when the line cannot be placed (gone from the library,
        /// or a book this index does not hold): the caller then starts over. (Core 277de7a9.)
        /// </summary>
        public long? PositionOfLine(long lineId)
        {
            var books = BookPositions();
            using var corpus = OpenCorpus();
            return corpus.PositionOfLine(lineId, books);
        }

        /// <summary>Each book's block of index positions (FtsLib's 1-based rowids), read once
        /// and kept until <see cref="InvalidateBookPositions"/>.</summary>
        internal System.Collections.Generic.Dictionary<long, (long First, long Count)> BookPositions()
        {
            lock (_lock)
            {
                if (_bookPositions == null)
                {
                    using var positions = OpenCorpus();
                    _bookPositions = positions.BookPositionRanges();
                }
                return _bookPositions;
            }
        }

        /// <summary>Core's WarmBookPositions: the book positions into memory ahead of the first
        /// in-book / commentary search or resumed stream. Safe from any thread.</summary>
        public void WarmBookPositions() => BookPositions();

        /// <summary>Core's constant: a fragment contained in more dictionary terms than this is
        /// too common for the index to be the cheap answer; the caller reads the book instead.</summary>
        public const int MaxTermsPerSubstringFragment = 4000;

        /// <summary>
        /// Core's <c>SubstringCandidates</c>, ported step for step (issue G-K1): the lines inside
        /// the given (book, first lineIndex, last lineIndex) ranges whose text may hold
        /// <paramref name="query"/> as a substring (a superset: the caller confirms on the text),
        /// as (book, lineIndex, line id), in range order then reading order. Null when the index
        /// cannot answer exactly (a fragment too common, no FM sidecar, a word with no term):
        /// the caller reads the lines itself. A book the index does not hold contributes nothing.
        /// One index range per BOOK over the span of its ranges, rows filtered back to the exact
        /// ranges afterwards (Core's plan).
        /// </summary>
        public System.Collections.Generic.List<(int BookId, int LineIndex, long LineId)>? SubstringCandidates(
            string query, System.Collections.Generic.IReadOnlyList<(int BookId, int FirstLineIndex, int LastLineIndex)> lineRanges,
            System.Threading.CancellationToken ct = default)
        {
            var books = BookPositions();
            var spans = new System.Collections.Generic.Dictionary<int, (long First, long Last)>();
            var wanted = new System.Collections.Generic.Dictionary<int, System.Collections.Generic.List<(long First, long Last)>>();
            foreach (var (bookId, firstLine, lastLine) in lineRanges)
            {
                if (!books.TryGetValue(bookId, out var book) || book.Count == 0) continue;
                long from = System.Math.Max(0, firstLine), to = System.Math.Min(book.Count - 1, lastLine);
                if (to < from) continue;
                long first = book.First + from, last = book.First + to;
                if (spans.TryGetValue(bookId, out var span))
                    spans[bookId] = (System.Math.Min(span.First, first), System.Math.Max(span.Last, last));
                else
                {
                    spans[bookId] = (first, last);
                    wanted[bookId] = new System.Collections.Generic.List<(long, long)>();
                }
                wanted[bookId].Add((first, last));
            }
            if (spans.Count == 0) return new System.Collections.Generic.List<(int, int, long)>();
            var ranges = new System.Collections.Generic.List<(long First, long Last)>(spans.Count);
            var owners = new System.Collections.Generic.List<(int BookId, long First, System.Collections.Generic.List<(long First, long Last)> Wanted)>(spans.Count);
            foreach (var kv in spans)
            {
                ranges.Add(kv.Value);
                var list = wanted[kv.Key];
                list.Sort((a, b) => a.First.CompareTo(b.First));
                owners.Add((kv.Key, books[kv.Key].First, list));
            }
            var found = Index.FindSubstringCandidates(query, ranges, MaxTermsPerSubstringFragment, ct);
            if (found == null) return null;
            var result = new System.Collections.Generic.List<(int, int, long)>(found.Count);
            foreach (var (range, position, lineId) in found)
            {
                var owner = owners[range];
                if (!InsideAny(owner.Wanted, position)) continue;
                result.Add((owner.BookId, (int)(position - owner.First), lineId));
            }
            return result;
        }

        /// <summary>Core's InsideAny: whether <paramref name="position"/> falls inside one of the
        /// ranges sorted by start (binary search on the starts, then a short walk back).</summary>
        private static bool InsideAny(System.Collections.Generic.List<(long First, long Last)> sorted, long position)
        {
            int lo = 0, hi = sorted.Count - 1, at = -1;
            while (lo <= hi)
            {
                int mid = (lo + hi) >> 1;
                if (sorted[mid].First <= position) { at = mid; lo = mid + 1; } else hi = mid - 1;
            }
            for (int i = at; i >= 0; i--)
            {
                if (sorted[i].Last >= position) return true;
                if (at - i >= 4) break;
            }
            return false;
        }

        public void Dispose()
        {
            Index.Dispose();
            _corpora.Dispose();
        }
    }

    /// <summary>Core's FtsIndexStatus (ftsIndexingStatus / ftsIndexProgressStream), same fields.</summary>
    public sealed class FtsIndexStatus
    {
        public bool IsReady { get; set; }
        public bool IsIndexing { get; set; }
        public double Percentage { get; set; }
        public int ProcessedChunks { get; set; }
        public int TotalChunks { get; set; }
        /// <summary>No seforim database at the given path: nothing to index.</summary>
        public bool DbMissing { get; set; }
        /// <summary>The build ended without a searchable index and nothing will retry it
        /// (TERMINAL, like <see cref="IsReady"/> and <see cref="DbMissing"/>).</summary>
        public bool Failed { get; set; }
        /// <summary>The build's final phase (merge into one segment + FM sidecar): no line
        /// progress, a distinct phase rather than a stalled bar.</summary>
        public bool IsMerging { get; set; }
        public int MergePassesDone { get; set; }
        public int MergePassesExpected { get; set; }
        /// <summary>Why the index last started building, and when; null if it never has in
        /// this process.</summary>
        public SeforimSearch.Demo.Common.IndexBuildReason? BuildReason { get; set; }
    }

    /// <summary>Core's FtsLinesSearchResult: substring candidates inside line ranges (the book
    /// view's in-book find, the commentary panel's find). <c>Answered</c> false: the index
    /// cannot answer exactly right now and the caller reads the lines itself. Otherwise the
    /// three parallel arrays name every line that MAY contain the query (the caller confirms
    /// the exact substring on the text).</summary>
    public sealed class FtsLinesSearchResult
    {
        public bool Answered { get; set; }
        public int[] BookIds { get; set; } = System.Array.Empty<int>();
        public int[] LineIndexes { get; set; } = System.Array.Empty<int>();
        /// <summary>The same lines' ids, to fetch their text by.</summary>
        public long[] LineIds { get; set; } = System.Array.Empty<long>();
    }

    /// <summary>Core's FtsLineRange: one (book, first lineIndex, last lineIndex) range, inclusive.</summary>
    public sealed class FtsLineRange
    {
        public int BookId { get; set; }
        public int FirstLineIndex { get; set; }
        public int LastLineIndex { get; set; }
    }

    /// <summary>Core's FtsSearchResult (the one-shot ftsSearch). <c>Ready</c> is false while
    /// the index is still building.</summary>
    public sealed class FtsSearchResult
    {
        public bool Ready { get; set; }
        public string? Error { get; set; }
        public List<FtsHit> Results { get; set; } = new List<FtsHit>();
        /// <summary>The matched-term strings the hits' mark term ids index into.</summary>
        public List<string> Terms { get; set; } = new List<string>();
        /// <summary>Snippet texts for the FIRST hits (see <see cref="FtsStreamChunk.SnippetTexts"/>).</summary>
        public List<FtsSnippetText> SnippetTexts { get; set; } = new List<FtsSnippetText>();
        /// <summary>Matched rows the search could not decode (skipped as empty, issue #13).</summary>
        public int UnreadableRowCount { get; set; }
        public List<long> UnreadableLineIds { get; set; } = new List<long>();
        public string? UnreadableError { get; set; }
    }
}
