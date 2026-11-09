//! The engine's own worker pools (rayon, already in the build through
//! tantivy), built lazily on first use so they cost nothing until then.
//!
//! - [`indexing_pool`]: one thread per core, every worker in the OS background
//!   mode for its whole life (entered by the pool's start handler, left by its
//!   exit handler; see `tantivy::indexer::thread_priority`), for parallel
//!   indexing-side work the engine owns. Work that foreground code waits for
//!   (a commit, a lock a reader needs) must not run here, or must wrap itself
//!   in `tantivy::indexer::NormalPrioritySection`.
//! - A normal-priority search pool will sit beside it (issue #24).
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

#[cfg(test)]
mod tests {
    #[test]
    fn indexing_pool_workers_run_in_background_mode() {
        let all = super::indexing_pool()
            .broadcast(|_| tantivy::indexer::thread_priority::is_background());
        assert!(all.into_iter().all(|b| b));
    }
}
