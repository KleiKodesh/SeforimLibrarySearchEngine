# Search timing report (2026-11-16 09:47)

Engine: FtsLib (KitveiHakodesh Core searcher). Runs per query: 3 (median, after one warm-up). Index: `C:/bench/indexes/ftslib` (817.0 MB). Runner: `D:/a/SeforimLibrarySearchEngine/SeforimLibrarySearchEngine/tools/parity/FtsLibDump/bin/Release/net10.0-windows/FtsLibDump.exe`.

| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | ה' אלהים | - | 0 | - | 45,667 | - | - | 4,504 | 221 | 4,504 | 4,612 |
| 1 | ר' עקיבא | 0 | 0 | - | 10,753 | - | - | 2,798 | 229 | 2,798 | 2,897 |
| 2 | אמר ר | 0 | 0 | - | 40,438 | - | - | 9,997 | 222 | 9,997 | 10,104 |
| 3 | ב ג | - | 0 | - | 135,669 | - | - | 9,672 | 215 | 9,672 | 9,787 |
| 4 | משה אהרן | 1 | 0 | - | 3,739 | - | - | 2,463 | 197 | 2,463 | 2,561 |
| 5 | משה ו אהרן | 0 | 0 | - | 1 | - | - | 962 | 733 | 962 | 1,055 |
| 6 | פרק 1 | - | 0 | - | 6,390 | - | - | 616 | 224 | 616 | 712 |
| 7 | סימן 12 | - | 0 | - | 124 | - | - | 241 | 241 | 241 | 336 |
| 8 | דף 2 עמוד | 1 | 0 | - | 0 | - | - | 198 | - | 198 | 290 |
| 9 | ב5 | - | 0 | - | 8 | - | - | 192 | 191 | 192 | 286 |
| 10 | 12* | - | 0 | - | 13,307 | - | - | 1,549 | 220 | 1,549 | 1,650 |
| 11 | א~1 | - | 0 | - | 5,475,472 | - | - | 140,685 | 215 | 140,685 | 140,823 |
| 12 | אמר ה | 2 | 0 | - | 24,428 | - | - | 10,536 | 213 | 10,536 | 10,644 |
| 13 | ס"ק 3 | 0 | 0 | - | 1 | - | - | 383 | 383 | 383 | 475 |

14 queries, 0 failed.

Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.
Full logs: search-<n>.log next to this report.
