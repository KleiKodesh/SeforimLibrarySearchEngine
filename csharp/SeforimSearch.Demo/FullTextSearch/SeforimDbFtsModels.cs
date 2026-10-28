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
        public int LineId { get; set; }
        public int BookId { get; set; }
        public int Score { get; set; }
        public int WordDistance { get; set; }
        public int SnippetStart { get; set; }
        public int SnippetEnd { get; set; }
        /// <summary>(start, length, termId) triples; termId into the search's <see cref="FtsTermTable"/>.</summary>
        public int[] SnippetMarks { get; set; } = System.Array.Empty<int>();
        public int SnippetFlags { get; set; }

        public const int SnippetFlagLeadingEllipsis = 1;
        public const int SnippetFlagTrailingEllipsis = 2;
        /// <summary>The window holds fewer words than the requested context: the line is too
        /// short to fill it, so the page fetches its neighbour lines with it.</summary>
        public const int SnippetFlagShort = 4;
    }

    /// <summary>
    /// A snippet's TEXT (Core's FtsSnippetText): the matched line's own text and, for a short
    /// window only, the plain context around it - the same book's non-empty lines up to
    /// <see cref="SeforimDbFtsCorpus.SnippetNeighbourLineRadius"/> before and after, each side
    /// joined by single spaces in document order. Empty sides for a line that is not short.
    /// </summary>
    public sealed class FtsSnippetText
    {
        public int LineId { get; set; }
        public string Text { get; set; } = "";
        public string Before { get; set; } = "";
        public string After { get; set; } = "";
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
        private System.Collections.Generic.Dictionary<int, (long First, long Count)>? _bookPositions;

        /// <summary>
        /// The index position of a line (<see cref="SeforimDbFtsCorpus.PositionOfLine"/>), to
        /// resume a search after it. Null when the line cannot be placed (gone from the library,
        /// or a book this index does not hold): the caller then starts over. (Core 277de7a9.)
        /// </summary>
        public long? PositionOfLine(int lineId)
        {
            System.Collections.Generic.Dictionary<int, (long First, long Count)> books;
            lock (_lock)
            {
                if (_bookPositions == null)
                {
                    using var positions = OpenCorpus();
                    _bookPositions = positions.BookPositionRanges();
                }
                books = _bookPositions;
            }
            using var corpus = OpenCorpus();
            return corpus.PositionOfLine(lineId, books);
        }

        public void Dispose()
        {
            Index.Dispose();
            _corpora.Dispose();
        }
    }
}
