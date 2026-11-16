# Search timing report (2026-11-16 09:54)

Engine: tantivy engine (SeforimSearch.Demo). Runs per query: 3 (median, after one warm-up). Index: `C:/bench/indexes/ours-pos` (1,757.9 MB). Runner: `D:/a/SeforimLibrarySearchEngine/SeforimLibrarySearchEngine/csharp/SeforimSearch.Demo/bin/Release/net10.0/SeforimSearch.Demo.exe`.

| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | ע.ש | - | 0 | 0 | 310,526 | 0.0 | 4.9 | 2,852 | 299 | 2,857 | 3,005 |
| 1 | עש | - | 0 | 0 | 310,526 | 0.0 | 4.7 | 2,691 | 298 | 2,696 | 2,846 |
| 2 | א.ב | - | 0 | 0 | 59,125 | 0.0 | 3.6 | 956 | 209 | 960 | 1,096 |
| 3 | וי[א]מר | - | 0 | 0 | 72,719 | 0.0 | 4.0 | 1,066 | 210 | 1,070 | 1,207 |
| 4 | ויאמר‏ משה | 0 | 0 | 0 | 3,731 | 0.0 | 4.8 | 240 | 195 | 245 | 359 |
| 5 | the | - | 0 | 0 | 2,815 | 0.0 | 3.7 | 208 | 180 | 212 | 322 |
| 6 | torah | - | 0 | 0 | 53 | 0.0 | 3.7 | 187 | 181 | 190 | 298 |
| 7 | de la | - | 0 | 0 | 4 | 0.0 | 4.1 | 176 | 170 | 180 | 286 |
| 8 | בabc | - | 0 | 0 | 0 | 0.0 | 3.8 | 101 | - | 106 | 210 |
| 9 | και | - | 0 | 0 | 0 | 0.0 | 3.5 | 100 | - | 103 | 207 |
| 10 | תוס' | - | 0 | 0 | 184,320 | 0.0 | 4.0 | 1,861 | 244 | 1,865 | 2,000 |
| 11 | תוס' ד״ה | 2 | 0 | 0 | 51,331 | 0.0 | 5.5 | 881 | 236 | 886 | 1,013 |
| 12 | ר' יוחנן | 0 | 0 | 0 | 44,017 | 0.0 | 5.3 | 838 | 232 | 843 | 971 |
| 13 | רש”י | - | 0 | 0 | 367,347 | 0.0 | 5.0 | 3,348 | 311 | 3,353 | 3,505 |
| 14 | ז’ל | - | 0 | 0 | 509,528 | 0.0 | 5.5 | 4,242 | 387 | 4,248 | 4,395 |
| 15 | רמב”ם הלכות | 3 | 0 | 0 | 1,454 | 0.0 | 4.6 | 224 | 196 | 229 | 340 |
| 16 | שׂלום | - | 0 | 0 | 58,628 | 0.0 | 4.0 | 974 | 214 | 978 | 1,113 |
| 17 | (הוצא) [היצא] | - | 0 | 0 | 237 | 0.0 | 3.9 | 189 | 183 | 193 | 302 |
| 18 | היצא אותם | 1 | 0 | 0 | 1 | 0.0 | 4.3 | 170 | 165 | 174 | 283 |
| 19 | משה אאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאאא רבנו | 1 | 0 | 0 | 2,422 | 0.0 | 4.8 | 227 | 194 | 232 | 345 |
| 20 | משה רבנו | 0 | 0 | 0 | 2,383 | 0.0 | 4.6 | 226 | 194 | 231 | 343 |

21 queries, 0 failed.

Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.
Full logs: search-<n>.log next to this report.
