## Where each engine is slower, and why

Measured on the run above: Apple M4 (10 cores, 16 GB), macOS, `seforim.db` of Zayit (7,019,636 lines in the index),
`tantivy11` / `fts9`, timings are medians of 3 runs after a warm-up, every run in a fresh process. Units: ms below one
second, minutes otherwise. "Equal hits" means the engines returned the same hit count (within 1%), so they did the same
work. Where a cause is an inference and not a measurement, the text says so.

### Summary

| Stage | Slower | Numbers | Why | Evidence |
|---|---|---|---|---|
| Index build | ours (positions) vs FtsLib and Otzaria | 3.48 min vs 1.95 min (FtsLib) and 1.42 min (Otzaria); no positions 2.82 min | ours merges to one segment (0.90 min, 26% of the build) and Otzaria's index is not merged; positions add 0.66 min | build logs (indexing 2.56 min, merge 0.90 min) |
| Index size | ours (positions) vs FtsLib | 1,585.5 MB vs 738.1 MB (FtsLib); Otzaria 1,886.0 MB | the positions file `.pos` is 704.6 MB (44%); without positions ours is 711.7 MB, on par with FtsLib | build logs, per-file sizes |
| First batch (first 256 hits) | ours vs Otzaria; ours is faster than FtsLib | median over each set (ours / Otzaria / FtsLib): large 41 / 17 / 56 ms, rules 39 / 17 / 55 ms; the words set is the exception: 42 / 72 / 55 ms | (a) the first search of a fresh process pays the SQLite corpus open, JIT and first FFI call, and our clock starts before them while the Otzaria driver's clock starts after it opened its index; (b) the first page waits until every hit is collected, so it grows with the hit count | see "First batch" below |
| Total, small result sets (up to 1,000 hits, equal hits) | ours vs Otzaria; ours is faster than FtsLib | median 32-44 ms vs 9-12 ms (Otzaria) vs 43-64 ms (FtsLib) | the same fixed first-search cost; there is almost no work left | rules q9 (0 hits): 23 ms vs 8 ms; words q10 (2 hits): 34 ms vs 9 ms |
| Total, mid result sets (15-40k hits, equal hits) | no consistent direction | `רמב"ן` 477 ms vs Otzaria 401 ms (+19%), `רמב"ם` 637 vs 597 (+7%), `ירושלים` 556 vs 604 (ours faster), `אלקים` 679 vs 706 (ours faster) | the host stage (text fetch from SQLite, locate, widening) is 99.7% of our total; Otzaria keeps the text in its index (inference: no stage split exists for Otzaria) | search reports |
| Total, large result sets (200k+ hits, equal hits) | Otzaria | over the 28 equal-hit queries of the large set: ours 1.97 min (19.1 µs/hit), FtsLib 2.13 min (20.6 µs/hit), Otzaria 3.27 min (31.6 µs/hit) | ours is the fastest here | search reports |
| Several words with a distance | ours (and FtsLib) vs Otzaria | `משה אהרן` 0: 409 ms vs 59 ms; `משה אהרן` 3: 403 vs 314 ms; `שמע ישראל` 0: 547 vs 341 ms; `אמר ה` 2: 0.05 min vs 0.02 min | (a) not the same work: Otzaria's advanced mode is ordered, ours here is unordered (662 vs 587, 5,593 vs 4,424, 5,656 vs 5,463, 23,248 vs 16,226 hits); (b) the host path fetches and checks the text of every candidate line instead of using the positions | see "Distance queries" below |
| Fuzzy `~1` / `~2` on very broad words | ours (no positions) vs FtsLib, first batch | `שבת~1`: 138 ms vs 80 ms; `רש"י~1`: 125 vs 76 ms; `א~1` (4.79M hits): 226 ms (positions) / 269 ms (no positions) vs 71 ms | the walk plus collecting every hit of a very broad fuzzy expansion; totals are equal or better than FtsLib | cause of the positions / no-positions gap not isolated (re-measured in process: collect 63-240 ms, noisy) |

Where our totals are lower than the others: against FtsLib the median of the per-query total ratio (ours, positions / FtsLib) is
large 0.84, words 0.72, rules 0.78, and no query is slower on the positions index (0 of 96); on the no-positions index three are
slightly slower (large q1 and q55 by 0.1% and 1%, rules q15 by 6%). Against Otzaria: the 28 large-set queries with equal hits
(0.60 of its time).

Not slower, only different: `ז’ל` (curly quote) returns 450,405 hits in the benchmark's FtsLib-rules path (the same count as `זל`,
the curly quote is ignorable there) and 32 in Otzaria, which the engine called directly with a plain request also gives (32); `רש"י~1` returns 1,123,320 hits in ours and 550,524 in Otzaria (Otzaria's fuzzy
semantics differ), so Otzaria's lower total there (0.33 min vs 0.40 min) is half the work.

### First batch

The two tools time the first batch differently (see the README of `tools/bench`): ours starts the clock before the search
call of a fresh process, Otzaria's driver opens its index and configures its line source first. Findings:

1. A fixed cost per fresh process, independent of the query. A zero-hit query takes 23 ms in ours (8 ms Otzaria, 28 ms
   FtsLib) and a 2-hit query 33 ms (8 / 43 ms). In one process the same searches repeated take 1 ms (0 hits), 1 ms (2 hits)
   and 3-5 ms for a 4,222-hit word, so the cost is the first search of a process (SQLite corpus open, JIT, first FFI
   call) and not the engine.
2. A cost that grows with the hit count, warm in process: first batch 4-8 ms for 25,268 hits, 30-35 ms for 230,328 hits and
   44-48 ms for 798,997 hits, while the engine's collect is 1, 5 and 17-34 ms. The first page is produced only after
   all hits are collected and listed (Otzaria answers a top-K query); the rest of the growth is the work around the
   collect, not separated further (no per-stage counter for it).
3. Compared per query (more than 10% and more than 5 ms apart), ours is slower than Otzaria on the first batch in 28 of the 43
   large-set queries and in 16 of 19 rules queries; against FtsLib it is never slower on the positions index (0 of 61, 0 of 19)
   and slower on 5 of 61 on the no-positions index (large q1, q41 with 1.03M hits, and the fuzzy q53-q55). In the words set the Otzaria median is higher
   than ours (72 ms vs 42 ms), because its first page for several words takes up to 149 ms (words q5 132 ms, q8 149 ms).

The earlier table of this section (a Windows profile on `tantivy9`: first batch 350-420 ms for a 802,677-hit word, Otzaria 55-110 ms)
listed a pooled SQLite corpus and a pre-sorted index as built but not yet measured; they are in this run's index and host, and the
first batch is now 41 ms (median, large set). It is another machine and DB, so the two are not a like-for-like before / after.

Proposed, not done (the runbook forbids engine changes without the user's OK): start the first page from a top-K pass
and build the full hit list lazily; time the first batch the same way in both tools (clock after the index is open) so
the comparison measures the engine only.

### Distance queries

For `משה אהרן` with distance 0, 1 and 3 the engine returns the same 14,606 candidate lines (every line holding both
words) and the host fetches 14,621 line texts to check the distance on the text (cold: 158 ms in the locate batches; the
whole host stage 394-407 ms), although the final hits are only 662, 3,517 and 5,593. That is 22 times more texts than hits.
The host path is the same on the positions and the no-positions index (host 407 ms vs 397 ms), so the positions index does
not help this path. FtsLib (`detail=none`, no positions either) does the same work and needs 578 ms; ours is 30% faster
than FtsLib but slower than Otzaria.

The engine's own span query (the path of the hit-count comparison: ordered, gaps, cross-line) answers the same questions
in 11-14 ms and gets Otzaria's counts within 2 hits (ours / Otzaria: 587 / 587, 4,423 / 4,424, 5,465 / 5,463) - but it counts only and
no text is read, so the figures are not a total. Proposed, not done: route distance queries of the host searcher through the
span path when the index has positions, and read text only for the final hits.
