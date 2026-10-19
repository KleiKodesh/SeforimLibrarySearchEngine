# SeforimdbSerachEngine

Search engine over the Otzaria `seforim.db` (SQLite).

Layout: `csharp/` (empty), `dart/` (empty), `tantivy/` (clone of Tantivy, Rust full-text engine).

Reference implementation for reading the DB: KitveiHakodesh app at
`C:\Users\Public\Documents\KleiKodeshProject\KitveiHakodesh` (C# backend under `CSharpBackend\KitveiHakodesh.Core\SeforimDb\`).
Authoritative schema doc: `KitveiHakodesh\.kiro\steering\database-schema.md`.

## DB location

Resolution order (`Settings\SeforimDbPathResolver.cs`):
1. Registry `HKCU\Software\VB and VBA Program Settings\KitveiHakodesh\Database` → `Path` (if file exists)
2. `%APPDATA%\otzaria\books\seforim.db` (Otzaria)
3. `%APPDATA%\io.github.kdroidfilter.seforimapp\databases\seforim.db` (Zayit)

Open read-only: `Mode=ReadOnly`, pooling, then `PRAGMA cache_size` / `PRAGMA mmap_size`.

## Relevant schema

- `book(id, categoryId, title, orderIndex, totalLines, ...)`
- `category(id, parentId, title, level, orderIndex)` + `category_closure(ancestorId, descendantId)`
- `line(id, bookId, lineIndex, heRef, tocEntryId, charCount[, content])` — `content` only on schema < 6
- `line_content(id = line.id, content)` — schema 6+. Declared TEXT, actually zstd BLOB (some rows base64 text of a frame)
- `zstd_dict(id, dict BLOB)` — schema 6+
- `book_version(id, bookId, versionTitle, hasContent)`, `version_line(versionId, lineId, content, charCount)` — NULL content = same as base line
- TOC: `tocEntry(id, bookId, parentId, textId, level, lineId, ...)`, `tocText(id, text)`, `line_toc(lineId, tocEntryId)` (authoritative mapping)
- Links: `link(sourceBookId, targetBookId, sourceLineId, targetLineId, connectionTypeId, ...)`

## Schema detection (don't trust `schema_meta.db_schema_version`)

Probe once per file (`SeforimDbSchema.cs`):
- split content: `line_content` table exists AND `line` has no `content` column
- compressed: `zstd_dict` table exists

## Reading lines

```sql
-- split DB
SELECT l.id, l.lineIndex, COALESCE(lc.content,'') AS content
FROM line l LEFT JOIN line_content lc ON lc.id = l.id
WHERE l.bookId = @bookId
ORDER BY l.lineIndex
LIMIT @limit OFFSET @offset;
-- old DB: SELECT l.id, l.lineIndex, l.content FROM line l WHERE ...
```

For indexing, stream whole table ordered by `bookId, lineIndex` instead of paging.

## Decoding content (`SeforimDbLineContentCodec.cs`)

Decide by runtime value type — never via SQL CASE/LIKE (SQLite coerces blobs):
- NULL → null
- string → as-is, unless it's a base64 zstd frame → base64-decode then decompress
- byte[] → zstd frame: read dictionary id from frame header, pick matching dict from `SELECT dict FROM zstd_dict`
  (match on the dict id embedded in dict bytes, e.g. 32768 — not the table `id`), decompress (cap 16 MB), UTF-8 decode
- Cache dicts per file; decompressor is not thread-safe → one per thread/connection.

Content is Otzaria HTML markup — strip tags (and optionally nikud/teamim) before indexing.
`LIKE` on content is useless on compressed DBs — that's why this engine exists.
