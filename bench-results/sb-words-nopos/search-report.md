# Search timing report (2026-11-16 09:35)

Engine: tantivy engine (SeforimSearch.Demo). Runs per query: 3 (median, after one warm-up). Index: `C:/bench/indexes/ours-nopos` (807.6 MB). Runner: `D:/a/SeforimLibrarySearchEngine/SeforimLibrarySearchEngine/csharp/SeforimSearch.Demo/bin/Release/net10.0/SeforimSearch.Demo.exe`.

| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | ה' אלהים | - | 0 | 45,667 | 45,667 | 0.0 | 17.7 | 3,470 | 207 | 3,488 | 3,629 |
| 1 | ר' עקיבא | 0 | 0 | 15,752 | 10,753 | 0.0 | 11.2 | 1,720 | 207 | 1,731 | 1,867 |
| 2 | אמר ר | 0 | 0 | 139,168 | 40,438 | 0.0 | 28.4 | 8,291 | 214 | 8,319 | 8,473 |
| 3 | ב ג | - | 0 | 135,669 | 135,669 | 0.0 | 32.1 | 8,328 | 215 | 8,360 | 8,531 |
| 4 | משה אהרן | 1 | 0 | 15,574 | 3,739 | 0.0 | 10.4 | 1,625 | 183 | 1,636 | 1,765 |
| 5 | משה ו אהרן | 0 | 0 | 2,057 | 1 | 0.0 | 8.4 | 533 | 427 | 541 | 657 |
| 6 | פרק 1 | - | 0 | 6,390 | 6,390 | 0.0 | 7.0 | 477 | 199 | 484 | 601 |
| 7 | סימן 12 | - | 0 | 124 | 124 | 0.0 | 5.7 | 206 | 201 | 212 | 319 |
| 8 | דף 2 עמוד | 1 | 0 | 23 | 0 | 0.0 | 6.9 | 168 | - | 175 | 281 |
| 9 | ב5 | - | 0 | 8 | 8 | 0.0 | 3.9 | 186 | 174 | 190 | 303 |
| 10 | 12* | - | 0 | 13,307 | 13,307 | 0.0 | 8.4 | 937 | 191 | 946 | 1,072 |
| 11 | א~1 | - | 0 | 5,475,472 | 5,475,472 | 0.1 | 366.3 | 126,250 | 910 | 126,614 | 126,907 |
| 12 | אמר ה | 2 | 0 | 156,718 | 24,428 | 0.0 | 33.3 | 8,577 | 211 | 8,610 | 8,762 |
| 13 | ס"ק 3 | 0 | 0 | 4,367 | 1 | 0.0 | 6.3 | 293 | 290 | 299 | 408 |

14 queries, 0 failed.

Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.
Full logs: search-<n>.log next to this report.
