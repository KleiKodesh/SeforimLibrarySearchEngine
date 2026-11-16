using System;
using System.Collections.Generic;
using System.Threading;
using FtsLib;

namespace SeforimSearch.Demo.SeforimDbFullTextSearch
{
    /// <summary>
    /// The facade's ONLY way to seforim.db. The facade holds no SQLite and no zstd (one SQLite
    /// per host app): the host implements this on its own database layer (KitveiHakodesh Core
    /// on its existing seforim.db code). <c>SeforimSearch.SeforimDb</c>'s
    /// <c>SeforimDbSqliteDataSource</c> is the default implementation (demo CLI, selftest).
    /// Every method may be called from any thread; each <see cref="OpenCorpus"/> lease is used
    /// by one caller at a time.
    /// </summary>
    public interface ISeforimDbFtsDataSource
    {
        /// <summary>The database is there (a missing one is a state: the coordinator reports
        /// <c>DbMissing</c>).</summary>
        bool IsAvailable { get; }

        /// <summary>A lease on the database for one caller; Dispose ends it. A host may keep
        /// the database open across leases (the default source pools connections).</summary>
        ISeforimDbFtsCorpus OpenCorpus();

        /// <summary>The database's provenance stamp with <paramref name="prefix"/> (the index
        /// format) prepended: equal stamps = the index still matches the database. Throws when
        /// the database cannot be read right now (the caller keeps what is there).</summary>
        string ContentFingerprint(string prefix);

        /// <summary>Why <paramref name="recorded"/> differs from <paramref name="current"/>, in
        /// words for a log line; null when equal. The stamp's format is the source's own.</summary>
        string? DescribeFingerprintChange(string? recorded, string current);

        /// <summary>Idle memory release: close what is kept open between leases (the next lease
        /// reopens it). Leases in use are not affected.</summary>
        void ReleaseIdle();
    }

    /// <summary>
    /// One lease on seforim.db (Core's SeforimDbFtsCorpus shape): line texts in catalog order,
    /// per-book reads for updates, snippet texts and neighbours, placements (book + line index),
    /// catalog order, content hashes. Footnotes are virtual lines / books
    /// (<see cref="SeforimSearch.Demo.SeforimDb.SeforimDbNoteIds"/>), each notes book right after
    /// its book. A read inside a <see cref="SeforimDbFtsUnreadableRows"/> scope reports a line it
    /// cannot decode there (<see cref="SeforimDbFtsUnreadableRows.Report"/>) and skips it.
    /// </summary>
    public interface ISeforimDbFtsCorpus : IDisposable
    {
        /// <summary>Lines in the index (footnote lines included).</summary>
        long CountDocuments();
        long CountDocumentsUpTo(long upToId);
        string? GetDocumentText(long id);
        IEnumerable<(long Id, string Text)> ReadDocuments(int limit, CancellationToken ct = default);
        /// <summary>Every line in catalog order (= index position order), from position <paramref name="skip"/>.</summary>
        IEnumerable<(long Id, string Text)> ReadDocumentsInResultOrder(long skip, CancellationToken ct = default);
        IEnumerable<(long Id, string Text, long BookId)> ReadDocumentsWithBookInResultOrder(long skip, CancellationToken ct = default);
        IEnumerable<(long Id, string Text, long BookId, long? SectionId)> ReadDocumentsWithSectionInResultOrder(long skip, CancellationToken ct = default);
        IEnumerable<(long Id, string Text)> ReadDocumentsAfter(long afterId, int limit = 0, CancellationToken ct = default);
        IEnumerable<(long Id, string Text, string Title)> FetchDocuments(IEnumerable<long> ids);
        IEnumerable<(long Id, string Text, FtsDocumentPlacement Placement)> FetchPlacedDocuments(IEnumerable<long> ids);
        Dictionary<long, FtsDocumentPlacement> FetchPlacements(IReadOnlyList<long> ids);
        /// <summary>Each book's block of index positions (first, count), notes books included.</summary>
        Dictionary<long, (long First, long Count)> BookPositionRanges();
        /// <summary>The book ids in the index order (catalog order, each notes book after its book).</summary>
        List<long> CatalogBookOrder();
        /// <summary>Per-book content hash (the per-book update), or null when the database has none.</summary>
        Dictionary<long, long>? BookContentHashes();
        IEnumerable<(long Id, string Text, long? SectionId)> ReadBookDocumentsWithSection(long bookId, CancellationToken ct = default);
        long? PositionOfLine(long lineId, Dictionary<long, (long First, long Count)> books);
        /// <summary>Hit texts in one read; a line in <paramref name="shortLineIds"/> also gets
        /// its same-book neighbours (<see cref="SeforimDbFtsSnippets.NeighbourLineRadius"/>).</summary>
        List<FtsSnippetText> FetchSnippetTexts(IReadOnlyList<long> lineIds, IReadOnlyCollection<long> shortLineIds);
        IDictionary<long, (string Previous, string Next)> FetchNeighbourText(IReadOnlyList<long> ids, int radius);
    }

    /// <summary>Snippet constants shared by the facade and every data source.</summary>
    public static class SeforimDbFtsSnippets
    {
        /// <summary>How many lines per side a SHORT snippet's context reaches (same book only;
        /// Core's SeforimDbFtsCorpus.SnippetNeighbourLineRadius).</summary>
        public const int NeighbourLineRadius = 2;
    }

    /// <summary>
    /// The related-forms artifact (Core's expansion-routed.db) as the expander reads it, so the
    /// facade holds no SQLite. <c>SeforimSearch.SeforimDb</c>'s
    /// <c>SeforimDbSqliteRelatedFormLexicon</c> is the default implementation.
    /// </summary>
    public interface ISeforimDbRelatedFormLexicon
    {
        /// <summary>The artifact is installed (else the expander is inert).</summary>
        bool IsAvailable { get; }

        /// <summary>One query's lookups (one open artifact); Dispose closes it.</summary>
        ISeforimDbRelatedFormLookup Open();
    }

    /// <summary>One query's reads of the artifact.</summary>
    public interface ISeforimDbRelatedFormLookup : IDisposable
    {
        /// <summary>The fold rows of <paramref name="surface"/>, heaviest first (weight DESC,
        /// lemma): lemma null = the surface itself, weight null = 1 (an artifact without weights).</summary>
        List<(string? Lemma, double? Weight)> Lemmas(string surface);

        /// <summary>The exp rows of <paramref name="lemma"/> in rank order, read lazily (the
        /// expander stops when its budget is full).</summary>
        IEnumerable<(string Form, string Channel, string Source)> Forms(string lemma);
    }
}
