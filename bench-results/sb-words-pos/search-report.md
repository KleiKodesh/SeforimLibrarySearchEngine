# Search timing report (2026-11-16 09:24)

Engine: tantivy engine (SeforimSearch.Demo). Runs per query: 3 (median, after one warm-up). Index: `C:/bench/indexes/ours-pos` (1,757.9 MB). Runner: `D:/a/SeforimLibrarySearchEngine/SeforimLibrarySearchEngine/csharp/SeforimSearch.Demo/bin/Release/net10.0/SeforimSearch.Demo.exe`.

| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | ה' אלהים | - | 0 | 0 | 45,667 | 0.0 | 6.5 | 848 | 221 | 854 | 982 |
| 1 | ר' עקיבא | 0 | 0 | 0 | 10,753 | 0.0 | 5.2 | 411 | 202 | 416 | 535 |
| 2 | אמר ר | 0 | 0 | 0 | 40,438 | 0.0 | 7.5 | 811 | 277 | 818 | 951 |
| 3 | ב ג | - | 0 | 0 | 135,669 | 0.0 | 7.8 | 1,632 | 267 | 1,640 | 1,783 |
| 4 | משה אהרן | 1 | 0 | 0 | 3,739 | 0.0 | 4.7 | 247 | 197 | 252 | 366 |
| 5 | משה ו אהרן | 0 | 0 | 0 | 1 | 0.0 | 5.5 | 174 | 172 | 180 | 289 |
| 6 | פרק 1 | - | 0 | 0 | 6,390 | 0.0 | 5.2 | 285 | 197 | 290 | 400 |
| 7 | סימן 12 | - | 0 | 0 | 124 | 0.0 | 5.4 | 203 | 197 | 208 | 318 |
| 8 | דף 2 עמוד | 1 | 0 | 0 | 0 | 0.0 | 6.2 | 102 | - | 108 | 215 |
| 9 | ב5 | - | 0 | 0 | 8 | 0.0 | 4.0 | 171 | 158 | 175 | 282 |
| 10 | 12* | - | 0 | 0 | 13,307 | 0.0 | 5.2 | 428 | 189 | 433 | 549 |
| 11 | א~1 | - | 0 | 0 | 5,475,472 | 0.1 | 58.2 | 37,782 | 4,565 | 37,840 | 38,056 |
| 12 | אמר ה | 2 | 0 | 0 | 24,428 | 0.0 | 8.7 | 754 | 284 | 763 | 890 |
| 13 | ס"ק 3 | 0 | 0 | 0 | 1 | 0.0 | 4.5 | 174 | 170 | 179 | 286 |

14 queries, 0 failed.

Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.
Full logs: search-<n>.log next to this report.
