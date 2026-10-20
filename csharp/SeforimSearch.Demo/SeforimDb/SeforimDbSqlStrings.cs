namespace SeforimSearch.Demo.SeforimDb
{
    /// <summary>
    /// SQL for the seforim database: the subset of KitveiHakodesh Core's
    /// SeforimDbSqlStrings that the full-text index build and search use, copied
    /// verbatim (catalog tree order, corpus feed, placed-line and neighbour fetches).
    /// </summary>
    internal static class SeforimDbSqlStrings
    {
        // ── Categories ──────────────────────────────────────────────────────────────

        /// <summary>Sibling order within a level: orderIndex, except that a NEGATIVE orderIndex
        /// sorts AFTER every non-negative one. The library marks its "mefarshim" subfolders with
        /// -5 — a commentaries folder belongs below the text it comments on, not above it (the
        /// Otzaria app reads the sign the same way). Shared by the browse query and the
        /// tree-order query so the catalog view and every ranking agree.</summary>
        private const string CategorySiblingOrder = "CASE WHEN orderIndex < 0 THEN 1 ELSE 0 END, orderIndex";

        // ── Full-text search corpus feed (SeforimDbFtsCorpus) ─────────────────────────
        // The reads the FTS engine's corpus seam is served from. Ports of FtsLib's internal
        // ZayitDb queries, kept semantically identical so an index built through Core is
        // byte-for-byte the index the built-in reader would have built.

        internal const string FtsCountLines = "SELECT COUNT(*) FROM line";

        internal const string FtsCountLinesUpTo = "SELECT COUNT(*) FROM line WHERE id <= @id";
        internal static string FtsGetLineContent(SeforimDbSchema schema) =>
            "SELECT " + C_ContentExpression(schema, "l") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            " WHERE l.id = @id";

        /// <summary>Every line ascending, optionally capped - the from-scratch index feed.
        /// Streamed by the caller; no ORDER BY alternative exists, ascending id IS the contract.</summary>
        internal static string FtsReadLines(SeforimDbSchema schema, bool hasLimit) =>
            "SELECT l.id, " + C_ContentColumn(schema, "l") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            " ORDER BY l.id" + (hasLimit ? " LIMIT @lim" : "");

        /// <summary>Lines strictly after @after, ascending - the resume feed. Exclusive bound,
        /// so the resume point itself is never indexed twice.</summary>
        internal static string FtsReadLinesAfter(SeforimDbSchema schema, bool hasLimit) =>
            "SELECT l.id, " + C_ContentColumn(schema, "l") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            " WHERE l.id > @after ORDER BY l.id" + (hasLimit ? " LIMIT @lim" : "");

        /// <summary>Lines per book — lets the catalog-order feed skip whole books on resume.</summary>
        internal const string FtsLineCountsByBook = "SELECT bookId, COUNT(*) FROM line GROUP BY bookId";

        /// <summary>One book's lines in reading order (lineIndex; id breaks a tie), from the
        /// @skip-th on — the catalog-order index feed, one book at a time. Served by
        /// idx_line_book_index.</summary>
        internal static string FtsReadBookLines(SeforimDbSchema schema) =>
            "SELECT l.id, " + C_ContentColumn(schema, "l") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            " WHERE l.bookId = @b ORDER BY l.lineIndex, l.id LIMIT -1 OFFSET @skip";

        /// <summary>One chunk of search-result rows: id, content, and the line's PLACEMENT -
        /// bookId, lineIndex and its own TOC entry (0 when it has none) - read off the same
        /// `line` row the content fetch visits anyway, so hits leave the searcher complete
        /// without a second query per batch. No book title: the page resolves titles from the
        /// book id. The TOC entry comes from `line_toc` when the database has it (the
        /// authoritative source; `line.tocEntryId` is NULL for most lines), else from that
        /// column, else 0. IN-list sized to the chunk.</summary>
        internal static string FtsFetchPlacedLines(SeforimDbSchema schema, int count) =>
            "SELECT l.id, " + C_ContentColumn(schema, "l") + ", l.bookId, l.lineIndex, " +
            (schema.HasLineTocTable ? "COALESCE(lt.tocEntryId, 0)"
             : schema.HasLineTocEntryIdColumn ? "COALESCE(l.tocEntryId, 0)"
             : "0") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            (schema.HasLineTocTable ? " LEFT JOIN line_toc lt ON lt.lineId = l.id" : "") +
            " WHERE l.id IN (" + InPlaceholders("p", count) + ")";

        /// <summary>
        /// The lines at a set of (bookId, lineIndex) positions — one row-value IN over the
        /// (bookId, lineIndex) index, so a whole batch's neighbour lines cost one round-trip.
        /// Parameters go in pairs: @p0,@p1 = first position, @p2,@p3 = second, and so on.
        /// A position with no line is simply absent.
        /// </summary>
        internal static string FtsFetchLinesAtBookPositions(SeforimDbSchema schema, int positionCount)
        {
            var values = new System.Text.StringBuilder(positionCount * 12);
            for (int i = 0; i < positionCount; i++)
            {
                if (i > 0) values.Append(',');
                values.Append("(@p").Append(2 * i).Append(",@p").Append(2 * i + 1).Append(')');
            }
            return "SELECT l.id, l.bookId, l.lineIndex, " + C_ContentColumn(schema, "l") +
                   " FROM line l" + C_ContentJoin(schema, "l") +
                   " WHERE (l.bookId, l.lineIndex) IN (VALUES " + values + ")";
        }

        /// <summary>
        /// Neighbour lines for one chunk of matched ids: m is the matched line, n every line in
        /// the SAME BOOK within ±@radius rows by lineIndex, excluding m itself. Returns the
        /// matched id, the signed delta, the neighbour's content and (last, so readers of the
        /// first three columns are unaffected) the neighbour's own id — the caller buckets by the
        /// delta's sign and joins each side in document order. One self-join per chunk, so a
        /// whole batch of short matches costs one round-trip, and the bookId bound keeps a
        /// snippet from ever bleeding across a book boundary.
        /// </summary>
        internal static string FtsFetchNeighborLines(SeforimDbSchema schema, int count) =>
            "SELECT m.id, n.lineIndex - m.lineIndex AS delta, " + C_ContentColumn(schema, "n") + ", n.id" +
            // The sidecar join must come AFTER this JOIN's own ON clause: putting it between
            // `JOIN line n` and its ON is a syntax error, because the ON would then bind to
            // the sidecar rather than to `n`.
            " FROM line m JOIN line n" +
            " ON n.bookId = m.bookId" +
            " AND n.lineIndex BETWEEN m.lineIndex - @radius AND m.lineIndex + @radius" +
            " AND n.lineIndex <> m.lineIndex" +
            C_ContentJoin(schema, "n") +
            " WHERE m.id IN (" + InPlaceholders("p", count) + ")";

        // ── Catalog index build (SeforimDbCatalogIndex) ───────────────────────────────
        // The reads the Lucene catalog index is built from. Here rather than beside the
        // indexer because SQL belongs to the DATABASE, not the feature (rule 9) — these read
        // the same seforim.db as everything above.

        /// <summary>Every line of one book, in reading order — the verse-marker scan.</summary>
        internal static string CatalogIndexBookLines(SeforimDbSchema schema) =>
            "SELECT l.lineIndex, " + C_ContentColumn(schema, "l") +
            " FROM line l" + C_ContentJoin(schema, "l") +
            " WHERE l.bookId = @b ORDER BY l.lineIndex";

        internal const string CatalogIndexTocRowsForBook = @"
            SELECT te.id, te.parentId, tt.text, l.lineIndex
            FROM tocEntry te
            JOIN tocText tt ON tt.id = te.textId
            LEFT JOIN line l ON l.id = te.lineId
            WHERE te.bookId = @b
            ORDER BY te.id";

        /// <summary>Same order the frontend loads categories in (level, then the sibling order
        /// above when the column exists) — the catalog tree-order computation depends on it.
        /// Distinct from GetAllCategories above: different SELECT list, different reader.</summary>
        internal static string CatalogIndexCategories(bool hasOrderIndex) => hasOrderIndex
            ? "SELECT id, parentId, title FROM category ORDER BY level, " + CategorySiblingOrder
            : "SELECT id, parentId, title FROM category ORDER BY level";

        /// <summary>Distinct from GetAllBooks above: no hasTeamim, and the index consumes the
        /// columns positionally in this order. The last two columns feed the tree order
        /// (<see cref="SeforimDb.SeforimDbBookTreeOrder"/>): a library whose schema lacks them
        /// gets 0 / NULL, i.e. no base-text promotion.</summary>
        internal static string CatalogIndexBooks(bool hasIsBaseBook, bool hasDependenceType) => $@"
            SELECT b.id, b.categoryId, b.title, group_concat(a.name, ', ') AS authors,
                   {(hasIsBaseBook ? "b.isBaseBook" : "0")} AS isBaseBook,
                   {(hasDependenceType ? "b.dependenceType" : "NULL")} AS dependenceType
            FROM book b
            LEFT JOIN book_author ba ON ba.bookId = b.id
            LEFT JOIN author a ON a.id = ba.authorId
            GROUP BY b.id
            ORDER BY b.orderIndex";

        private static string C_ContentColumn(SeforimDbSchema schema, string lineAlias)
            => SeforimDbLineContentSql.ContentColumn(schema, lineAlias);

        private static string C_ContentExpression(SeforimDbSchema schema, string lineAlias)
            => SeforimDbLineContentSql.ContentExpression(schema, lineAlias);

        private static string C_ContentJoin(SeforimDbSchema schema, string lineAlias)
            => SeforimDbLineContentSql.ContentJoin(schema, lineAlias);

        private static string C_RawContentExpression(SeforimDbSchema schema, string lineAlias)
            => SeforimDbLineContentSql.RawContentExpression(schema, lineAlias);

        internal static string InPlaceholders(string prefix, int count)
        {
            var sb = new System.Text.StringBuilder();
            for (int i = 0; i < count; i++)
            {
                if (i > 0) sb.Append(", ");
                sb.Append('@').Append(prefix).Append(i);
            }
            return sb.ToString();
        }
    }
}
