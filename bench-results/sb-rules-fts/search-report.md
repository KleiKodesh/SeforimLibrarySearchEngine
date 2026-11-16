# Search timing report (2026-11-16 10:08)

Engine: FtsLib (KitveiHakodesh Core searcher). Runs per query: 3 (median, after one warm-up). Index: `C:/bench/indexes/ftslib` (817.0 MB). Runner: `D:/a/SeforimLibrarySearchEngine/SeforimLibrarySearchEngine/tools/parity/FtsLibDump/bin/Release/net10.0-windows/FtsLibDump.exe`.

| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | ע.ש | - | 0 | - | 310,526 | - | - | 14,643 | 236 | 14,643 | 14,760 |
| 1 | עש | - | 0 | - | 310,526 | - | - | 14,376 | 234 | 14,376 | 14,488 |
| 2 | א.ב | - | 0 | - | 59,125 | - | - | 4,497 | 220 | 4,497 | 4,609 |
| 3 | וי[א]מר | - | 0 | - | 72,719 | - | - | 5,129 | 197 | 5,129 | 5,250 |
| 4 | ויאמר‏ משה | 0 | 0 | - | 3,731 | - | - | 2,889 | 207 | 2,889 | 2,998 |
| 5 | the | - | 0 | - | 2,815 | - | - | 406 | 199 | 406 | 498 |
| 6 | torah | - | 0 | - | 53 | - | - | 203 | 203 | 203 | 292 |
| 7 | de la | - | 0 | - | 4 | - | - | 191 | 190 | 191 | 280 |
| 8 | בabc | - | 0 | - | 0 | - | - | 79 | - | 79 | 169 |
| 9 | και | - | 0 | - | 0 | - | - | 80 | - | 80 | 172 |
| 10 | תוס' | - | 0 | - | 184,320 | - | - | 8,461 | 217 | 8,461 | 8,570 |
| 11 | תוס' ד״ה | 2 | 0 | - | 51,331 | - | - | 4,925 | 247 | 4,925 | 5,026 |
| 12 | ר' יוחנן | 0 | 0 | - | 44,017 | - | - | 5,208 | 239 | 5,208 | 5,313 |
| 13 | רש”י | - | 0 | - | 367,347 | - | - | 16,745 | 221 | 16,745 | 16,862 |
| 14 | ז’ל | - | 0 | - | 509,528 | - | - | 23,394 | 220 | 23,394 | 23,510 |
| 15 | רמב”ם הלכות | 3 | 0 | - | 1,454 | - | - | 593 | 249 | 593 | 688 |
| 16 | שׂלום | - | 0 | - | 58,628 | - | - | 4,268 | 218 | 4,268 | 4,380 |
| 17 | (הוצא) [היצא] | - | 0 | - | 237 | - | - | 230 | 229 | 230 | 323 |
| 18 | היצא אותם | 1 | 0 | - | 1 | - | - | 200 | 200 | 200 | 293 |
| 19 | משה אאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאא רבנו | 1 | 0 | - | 2,422 | - | - | 993 | 246 | 993 | 1,091 |
| 20 | משה רבנו | 0 | 0 | - | 2,383 | - | - | 1,035 | 246 | 1,035 | 1,132 |

21 queries, 0 failed.

Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.
Full logs: search-<n>.log next to this report.
