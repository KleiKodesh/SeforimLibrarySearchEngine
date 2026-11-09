//! The engine's own worker pools (rayon, already in the build through
//! tantivy), built lazily on first use so they cost nothing until then.
//!
//! - [`indexing_pool`]: one thread per core, every worker in the OS background
//!   mode for its whole life (entered by the pool's start handler, left by its
//!   exit handler; see `tantivy::indexer::thread_priority`), for parallel
//!   indexing-side work the engine owns. Work that foreground code waits for
//!   (a commit, a lock a reader needs) must not run here, or must wrap itself
//!   in `tantivy::indexer::NormalPrioritySection`.
//! - [`search_pool`]: one thread per core at normal priority, for a search
//!   batch's text work (line regex matching, snippet locating and count
//!   validation, positions reads). A batch goes parallel only when its texts
//!   reach [`PARALLEL_MIN_BYTES`] ([`LINE_REGEX_PARALLEL_MIN_BYTES`] for a
//!   line regex); below that it runs on the caller's thread
//!   (issue #24).
//!
//! tantivy's own indexing and merge threads are not pool threads: they enter
//! the background mode per thread themselves (`low_priority_threads`).

use std::sync::OnceLock;

/// The background-priority pool for indexing-side parallel work.
pub fn indexing_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("seforim-indexing-{i}"))
            .start_handler(|_| crate::resources::begin_background())
            .exit_handler(|_| crate::resources::end_background())
            .build()
            .expect("the indexing pool's threads could not be spawned")
    })
}

/// The normal-priority pool for a search batch's parallel text work: the
/// threads stay alive between calls, so a batch pays no thread spawn.
pub fn search_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("seforim-search-{i}"))
            .build()
            .expect("the search pool's threads could not be spawned")
    })
}

/// A locate / count batch (snippet window, distance and count validation)
/// whose texts hold fewer bytes than this runs sequentially on the caller's
/// thread; from here on it is split across [`search_pool`].
///
/// Measured (issue #24, `examples/search_pool_bench.rs`, 8 cores, best of 2x10
/// alternated, sequential vs pool): the pool loses below ~12 KB (16 short
/// lines, 1.5 KB: 0.026 vs 0.119 ms; 6 KB: 0.070 vs 0.131 ms; 12 KB: 0.129
/// vs 0.150 ms), wins from ~24 KB (0.259 vs 0.192 ms) and clearly from ~35 KB
/// (long lines: 0.255 vs 0.183 ms; 47 KB: 0.478 vs 0.301 ms), ~3x on big
/// batches (4,096 long lines: 32.6 vs 11.0 ms).
pub const PARALLEL_MIN_BYTES: usize = 32 * 1024;

/// The same cut-off for line-regex matching (feed and full scan): a regex
/// costs more per byte, so the pool wins earlier (3 KB: 0.481 vs 0.380 ms;
/// 6 KB: 0.943 vs 0.563 ms; 12 KB: 1.80 vs 0.95 ms; 4,096 long lines: 608
/// vs 207 ms). Below 3 KB (16 short lines) it is even.
pub const LINE_REGEX_PARALLEL_MIN_BYTES: usize = 8 * 1024;

/// True when a batch of `bytes` text bytes is worth the pool (`min`: the
/// path's cut-off).
pub(crate) fn parallel(bytes: usize, min: usize) -> bool {
    bytes >= min && search_pool().current_num_threads() > 1
}

#[cfg(test)]
mod tests {
    #[test]
    fn indexing_pool_workers_run_in_background_mode() {
        let all = super::indexing_pool()
            .broadcast(|_| tantivy::indexer::thread_priority::is_background());
        assert!(all.into_iter().all(|b| b));
    }
}
