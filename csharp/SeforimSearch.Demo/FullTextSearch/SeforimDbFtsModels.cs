using System.Collections.Generic;
using FtsLib.Fts5;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// One full-text hit (KitveiHakodesh Core's FtsHit, same fields): the line,
    /// its book and TOC entry, and the snippet DESCRIPTOR — the page joins the
    /// texts of <see cref="SnippetLineIds"/> with single spaces and renders the
    /// window [<see cref="SnippetStart"/>, <see cref="SnippetEnd"/>) with
    /// <see cref="SnippetMarks"/> highlighted.
    /// </summary>
    public sealed class FtsHit
    {
        public int LineId { get; set; }
        public int BookId { get; set; }
        public int TocEntryId { get; set; }
        public int Score { get; set; }
        public int WordDistance { get; set; }
        public int[] SnippetLineIds { get; set; } = System.Array.Empty<int>();
        public int SnippetStart { get; set; }
        public int SnippetEnd { get; set; }
        /// <summary>(start, length, termId) triples; termId into the search's <see cref="FtsTermTable"/>.</summary>
        public int[] SnippetMarks { get; set; } = System.Array.Empty<int>();
        public int SnippetFlags { get; set; }

        public const int SnippetFlagLeadingEllipsis = 1;
        public const int SnippetFlagTrailingEllipsis = 2;
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

        public void Dispose()
        {
            Index.Dispose();
            _corpora.Dispose();
        }
    }
}
