using System;
using System.Collections.Generic;
using SeforimSearch.Engine;

// FtsLib's public surface, recreated with the SAME names and members over the
// tantivy engine, so code written against FtsLib (KitveiHakodesh Core) moves
// over by swapping the namespace import.
namespace FtsLib
{
    /// <summary>Where a line sits: book, reading position, own TOC entry (0 when none).</summary>
    public readonly struct FtsDocumentPlacement
    {
        public FtsDocumentPlacement(int bookId, int lineIndex, int tocEntryId)
        {
            BookId = bookId;
            LineIndex = lineIndex;
            TocEntryId = tocEntryId;
        }

        public int BookId { get; }
        public int LineIndex { get; }
        public int TocEntryId { get; }
    }
}

namespace FtsLib.SeforimDb
{
    /// <summary>
    /// One row returned by <see cref="FtsLib.Fts5.TantivySeforimIndex.Search"/>
    /// (FtsLib's SearchResult): the matched line's id, content and placement.
    /// The snippet state lives in the search's session, which the result carries.
    /// </summary>
    public sealed class SearchResult
    {
        internal SearchResult(int lineId, string content, FtsDocumentPlacement placement, Fts5.SearchSessionRef session)
        {
            LineId = lineId;
            Content = content ?? string.Empty;
            Placement = placement;
            Session = session;
        }

        /// <summary>The line ID in the seforim database.</summary>
        public int LineId { get; }

        /// <summary>Kept for FtsLib compatibility; Core never reads it.</summary>
        public string BookTitle => string.Empty;

        /// <summary>Raw HTML content of the line as stored in the database.</summary>
        public string Content { get; }

        /// <summary>Where the line sits (book, position, TOC entry).</summary>
        public FtsDocumentPlacement Placement { get; }

        internal Fts5.SearchSessionRef Session { get; }

        /// <summary>The snippet located for this line in its batch (set by the
        /// batch locate; null when the line did not pass or was not located).</summary>
        internal Snippet? LocatedSnippet { get; set; }

        public override string ToString() => $"[{LineId}]";
    }
}

namespace FtsLib.Snippets
{
    /// <summary>
    /// WHERE a snippet is in its source text (FtsLib's SnippetDescriptor). Offsets
    /// are char offsets into the string located — the matched line, or that line
    /// joined with its neighbours by single spaces.
    /// </summary>
    public readonly struct SnippetDescriptor
    {
        public const int MarkStride = 3;

        public readonly bool IsMatch;
        public readonly int Score;
        public readonly int WordDistance;
        public readonly int WindowWordCount;
        public readonly int SnapStart;
        public readonly int SnapEnd;
        public readonly bool LeadingEllipsis;
        public readonly bool TrailingEllipsis;

        /// <summary>(rawStart, rawLength, termId) triples. Unlike FtsLib (query term
        /// ORDINAL, resolved with MatchedTermAt), the third value is already the
        /// search-wide term id; <see cref="MatchedTermAt"/> resolves it.</summary>
        public readonly IReadOnlyList<int> MarkRanges;

        private readonly Func<int, string>? _termAt;

        internal SnippetDescriptor(Snippet s, Func<int, string> termAt)
        {
            IsMatch = true;
            Score = s.Score;
            WordDistance = s.WordDistance;
            WindowWordCount = s.WindowWordCount;
            SnapStart = s.Start;
            SnapEnd = s.End;
            LeadingEllipsis = s.LeadingEllipsis;
            TrailingEllipsis = s.TrailingEllipsis;
            MarkRanges = s.Marks;
            _termAt = termAt;
        }

        /// <summary>The term a mark's third value stands for.</summary>
        public string MatchedTermAt(int termId) => _termAt?.Invoke(termId) ?? string.Empty;

        public static readonly SnippetDescriptor NoMatch = default;
    }
}
