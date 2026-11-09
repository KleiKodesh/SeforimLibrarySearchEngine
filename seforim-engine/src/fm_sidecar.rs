//! The FM-index sidecar of a line index: `<segment-id>.fmi` next to the
//! segment it was built from, over the dictionary's quote- and mark-stripped
//! term forms (rowid = ordinal + 1; on an index that keeps marks the
//! vocalized spellings of a word are rows of the same text). Built after the merge into one segment;
//! loaded lazily and memory-mapped on the first leading-`*` or fuzzy search.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use tantivy::schema::Field;
use tantivy::index::SegmentId;

use crate::fm_index::{self, FmIndex};
use crate::{Error, Result};

/// File name of the sidecar for a segment.
pub fn sidecar_path(dir: &Path, segment: SegmentId) -> PathBuf {
    dir.join(format!("{}.fmi", segment.uuid_string()))
}

/// Deletes the sidecars (`<segment>.fmi`) of segments not in `live` (issue
/// #29: a merge left the old segment's sidecar behind). Best effort: a file
/// still mapped by a running search cannot be deleted on Windows and is
/// retried at the next write's garbage collection.
pub(crate) fn remove_dead_sidecars(dir: &Path, live: &[SegmentId]) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let live: Vec<String> = live.iter().map(|id| format!("{}.fmi", id.uuid_string())).collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".fmi") && !live.iter().any(|l| l == name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The FM text form of a dictionary term: its quotes and marks dropped
/// (FtsLib's terms carried neither). Candidates found through it are
/// verified against the real term, so this only widens the candidate set,
/// never the result.
pub fn text_form(term: &str) -> String {
    term.chars()
        .filter(|&c| !hebrew_fts::chars::is_term_quote(c) && !hebrew_fts::chars::is_term_mark(c))
        .collect()
}

/// True when `path` holds a sidecar built over `num_terms` terms.
fn sidecar_matches(path: &Path, num_terms: usize) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    // SAFETY: see `FmCache::get`.
    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { return false };
    FmIndex::open(Box::new(map)).is_ok_and(|fm| fm.term_count() as usize == num_terms)
}

/// Builds the sidecar for `index`'s single segment and removes stale ones.
/// Idempotent: a sidecar that already matches the segment is kept as is.
/// Returns true when a sidecar is in place; false (nothing built, searches
/// use the walk) when the index has several segments or has no terms. Err
/// when the sidecar cannot be built (I/O failure, a dictionary the FM-index
/// cannot hold): never a silent skip, the host reports it.
pub fn build(searcher: &tantivy::Searcher, text: Field, dir: &Path) -> Result<bool> {
    build_cancellable(searcher, text, dir, None)
}

/// Removes the temp files of sidecar builds that never finished (a process
/// stopped mid-build): `<segment>.fmi.<pid>-<n>.tmp` (and the older
/// `<segment>.fmi.tmp`). A build holds an OS lock on its temp file while it
/// writes it, so a file that is locked belongs to a live build (in this or
/// another process) and is left alone. Best effort.
pub(crate) fn remove_leftover_temps(dir: &Path) {
    use fs4::fs_std::FileExt;
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let temp = path.file_name().and_then(|n| n.to_str()).is_some_and(is_temp_name);
        if !temp || !path.is_file() {
            continue;
        }
        // a file just created may not be locked yet (create, then lock):
        // a temp younger than a few seconds is never a leftover
        let young = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().map_or(true, |age| age < LEFTOVER_MIN_AGE));
        if young {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
        // a live build holds the lock: skip its file
        if !matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
            continue;
        }
        let _ = FileExt::unlock(&file);
        drop(file);
        let _ = std::fs::remove_file(path);
    }
}

/// A temp file younger than this is never removed as a leftover. Locks are
/// per open file on Windows and Unix `flock`; on NFS (emulated with
/// per-process `fcntl` locks) a same-process live temp is protected by this
/// age only.
const LEFTOVER_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(10);

/// True for the name of a sidecar build's temp file.
fn is_temp_name(name: &str) -> bool {
    name.contains(".fmi.") && name.ends_with(".tmp")
}

/// A temp name no other build uses: the process id and a per-process counter.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("fmi.{}-{n}.tmp", std::process::id()))
}

/// Writes `bytes` to a fresh temp file next to `path` under an exclusive OS
/// lock (so no cleanup removes it meanwhile) and moves it over `path`.
fn publish(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use fs4::fs_std::FileExt;
    use std::io::Write;
    let tmp = temp_path(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        FileExt::lock_exclusive(&file)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        // still locked while it takes the sidecar's name
        replace_file(&tmp, path)?;
        let _ = FileExt::unlock(&file);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// [`build`] that stops once `cancel` is set (Err, nothing written).
pub(crate) fn build_cancellable(
    searcher: &tantivy::Searcher,
    text: Field,
    dir: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<bool> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let segments = searcher.segment_readers();
    if segments.len() != 1 {
        return Ok(false);
    }
    let segment = &segments[0];
    let dict = segment.inverted_index(text)?;
    let dict = dict.terms();
    if dict.num_terms() == 0 {
        return Ok(false);
    }
    let path = sidecar_path(dir, segment.segment_id());
    if sidecar_matches(&path, dict.num_terms()) {
        return Ok(true);
    }
    let mut forms: Vec<String> = Vec::with_capacity(dict.num_terms());
    let mut stream = dict.stream()?;
    while stream.advance() {
        if forms.len() % 65536 == 0 && cancelled() {
            return Err(Error::Unsupported(format!("FM sidecar not built: {}", fm_index::BUILD_CANCELLED)));
        }
        let term = std::str::from_utf8(stream.key())
            .map_err(|_| Error::Unsupported("FM sidecar: a dictionary term is not UTF-8".into()))?;
        forms.push(text_form(term));
    }
    let bytes = fm_index::build_cancellable(forms.iter().map(|s| s.as_str()), cancel)
        .map_err(|e| Error::Unsupported(format!("FM sidecar not built: {e}")))?;

    // a temp file left by a build that never finished goes first
    remove_leftover_temps(dir);
    // publish atomically: the new file is complete before it takes the
    // sidecar's name, so a search sees the old file (or none), never half
    publish(&path, &bytes)?;
    // then the sidecars of older segments go
    for entry in std::fs::read_dir(dir)? {
        let other = entry?.path();
        if other != path && other.extension().is_some_and(|e| e == "fmi") {
            let _ = std::fs::remove_file(other);
        }
    }
    Ok(true)
}

/// Moves `from` over `to` (the rule of tantivy's `MmapDirectory` atomic
/// write, issue #7): `fs::rename` replaces the target; on Windows an
/// access-denied or sharing-violation error (a reader or a virus scan holding
/// the file for a moment) is retried with growing delays for about 500 ms.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const DELAYS_MS: [u64; 7] = [5, 10, 20, 40, 75, 150, 200];
        let mut attempt = 0;
        loop {
            match std::fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(err) if matches!(err.raw_os_error(), Some(5) | Some(32)) && attempt < DELAYS_MS.len() => {
                    std::thread::sleep(std::time::Duration::from_millis(DELAYS_MS[attempt]));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// The engine's own mending of the sidecar: when a search finds the sidecar
/// of a qualifying index (one segment, a non-empty dictionary) missing or
/// unusable, ONE background build starts (a thread in the OS background
/// mode); searches keep running as walks (same results) until the new file
/// is published, then the next search picks it up.
///
/// - At most one build per index at a time; `build_lock` also serializes
///   it with an explicit `build_fm_sidecar`.
/// - Once per problem: an attempt that leaves no sidecar (a failure, or
///   nothing to build) is recorded against the segment and never retried
///   automatically for it; a new segment (commit, merge) or an explicit
///   `build_fm_sidecar` clears the record. The reason shows in
///   `fm_sidecar_problem`.
/// - Never while a `LineWriter` of this index is open (after a merge to one
///   segment the writer is gone and the next search starts it). Writers in
///   another process are not seen; the build is still safe (it publishes by
///   rename and touches only `.fmi` files).
/// - Read-only index directories: the build fails, the reason is recorded,
///   searches stay on the walk.
pub(crate) struct Repair {
    /// Off: no automatic build (`LineIndex::set_fm_auto_repair`).
    auto: AtomicBool,
    /// Open writers of this index.
    writers: AtomicUsize,
    state: Mutex<RepairState>,
    /// Held by every sidecar build (background or explicit).
    pub(crate) build_lock: Mutex<()>,
    /// Set when the index is dropped: a running background build stops at
    /// its next check. The thread is never joined, so it cannot delay a
    /// drop or the process exit.
    pub(crate) cancel: AtomicBool,
    /// TEST ONLY: how many background builds were started.
    #[cfg(test)]
    pub(crate) test_only_starts: AtomicUsize,
    /// TEST ONLY: a background build waits this long before building.
    #[cfg(test)]
    pub(crate) test_only_delay_ms: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct RepairState {
    running: bool,
    /// The last automatic attempt that left no sidecar: its segment and,
    /// when it failed, the reason.
    attempted: Option<(SegmentId, Option<String>)>,
}

impl Repair {
    pub(crate) fn new(auto: bool) -> Repair {
        Repair {
            auto: AtomicBool::new(auto),
            writers: AtomicUsize::new(0),
            state: Mutex::new(RepairState::default()),
            build_lock: Mutex::new(()),
            cancel: AtomicBool::new(false),
            #[cfg(test)]
            test_only_starts: AtomicUsize::new(0),
            #[cfg(test)]
            test_only_delay_ms: Default::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RepairState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_auto(&self, on: bool) {
        self.auto.store(on, Ordering::Release);
    }

    /// True while a background build runs.
    pub(crate) fn running(&self) -> bool {
        self.state().running
    }

    /// Why the last automatic build for `segment` left no sidecar.
    pub(crate) fn failure(&self, segment: SegmentId) -> Option<String> {
        match &self.state().attempted {
            Some((id, reason)) if *id == segment => reason.clone(),
            _ => None,
        }
    }

    /// Forgets the attempt record (an explicit build).
    pub(crate) fn reset(&self) {
        self.state().attempted = None;
    }

    /// Registers an open writer until the guard drops.
    pub(crate) fn writer_guard(self: &Arc<Self>) -> WriterGuard {
        self.writers.fetch_add(1, Ordering::AcqRel);
        WriterGuard(self.clone())
    }

    /// Starts the background build when `searcher`'s index qualifies and
    /// nothing forbids it (see [`Repair`]). Never blocks the caller.
    pub(crate) fn maybe_start(
        self: &Arc<Self>,
        cache: &Arc<FmCache>,
        searcher: &tantivy::Searcher,
        text: Field,
        dir: &Path,
    ) {
        if !self.auto.load(Ordering::Acquire) || self.writers.load(Ordering::Acquire) > 0 {
            return;
        }
        let segments = searcher.segment_readers();
        if segments.len() != 1 {
            return;
        }
        let segment = segments[0].segment_id();
        if !segments[0].inverted_index(text).is_ok_and(|i| i.terms().num_terms() > 0) {
            return;
        }
        {
            let mut state = self.state();
            if state.running || state.attempted.as_ref().is_some_and(|(id, _)| *id == segment) {
                return;
            }
            state.running = true;
        }
        #[cfg(test)]
        self.test_only_starts.fetch_add(1, Ordering::Relaxed);
        let repair = self.clone();
        let cache = cache.clone();
        let searcher = searcher.clone();
        let dir = dir.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("seforim-fm-repair".into())
            .spawn(move || {
                crate::resources::begin_background();
                let outcome = repair.build_in_background(&cache, &searcher, text, &dir);
                crate::resources::end_background();
                let mut state = repair.state();
                state.running = false;
                if let Some(reason) = outcome {
                    state.attempted = Some((segment, reason));
                }
            });
        if let Err(e) = spawned {
            let mut state = self.state();
            state.running = false;
            state.attempted = Some((segment, Some(format!("FM sidecar rebuild could not start: {e}"))));
        }
    }

    /// The build itself. None: a sidecar is in place, or a writer opened
    /// meanwhile (nothing recorded, the next search decides again).
    /// Some(reason): the attempt left no sidecar.
    fn build_in_background(
        &self,
        cache: &FmCache,
        searcher: &tantivy::Searcher,
        text: Field,
        dir: &Path,
    ) -> Option<Option<String>> {
        let _build = self.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        // a writer opened after this check is harmless: the file is named by
        // this snapshot's segment and published atomically
        if self.writers.load(Ordering::Acquire) > 0 {
            return None;
        }
        // an explicit build that ran first may have mended it already
        // (and a search may map the fresh file: do not rename over it)
        if matches!(cache.lookup(dir, searcher, text), FmLookup::Ready(_)) {
            return None;
        }
        // drop any cached mapping of the file being replaced
        cache.clear();
        #[cfg(test)]
        {
            let until = std::time::Instant::now()
                + std::time::Duration::from_millis(self.test_only_delay_ms.load(Ordering::Relaxed));
            while std::time::Instant::now() < until && !self.cancel.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        let result = build_cancellable(searcher, text, dir, Some(&self.cancel));
        if self.cancel.load(Ordering::Relaxed) {
            // the index was dropped: nothing to record
            return None;
        }
        // the next search re-reads the published file
        cache.clear();
        match result {
            Ok(true) => None,
            Ok(false) => Some(None),
            Err(e) => Some(Some(format!("FM sidecar rebuild failed: {e}"))),
        }
    }
}

/// An open writer of the index (see [`Repair`]).
pub(crate) struct WriterGuard(Arc<Repair>);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.writers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The sidecar state of an index, as a search sees it.
pub enum FmLookup {
    /// A sidecar built over the current single segment's dictionary.
    Ready(Arc<FmIndex>),
    /// No sidecar (none built, or the index has several segments).
    Missing,
    /// A sidecar file for the segment exists but cannot be used (corrupt,
    /// truncated, an unknown format, or built over another dictionary):
    /// searches fall back to the walk and carry
    /// [`NOTICE_FM_SIDECAR_UNUSABLE`]; rebuilding the sidecar fixes it.
    Unusable(String),
}

/// The notice a search carries when the index's FM sidecar exists but cannot
/// be used: leading-`*` and fuzzy words ran as walks (same results, slower).
/// Rebuild the sidecar (`build_fm_sidecar`) to fix it.
pub const NOTICE_FM_SIDECAR_UNUSABLE: &str = "fm_sidecar_unusable";

/// What the cache knows about a segment's sidecar file.
enum Cached {
    Ready(Arc<FmIndex>),
    /// Found unusable; valid while the file keeps this identity (size,
    /// modification time), so a broken file is not re-opened on every search.
    Unusable { identity: (u64, Option<std::time::SystemTime>), reason: String },
}

/// The identity of a file for the unusable cache: its size and mtime.
fn file_identity(path: &Path) -> std::io::Result<(u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.len(), meta.modified().ok()))
}

/// What [`FmCache`] holds under its lock.
#[derive(Default)]
struct CacheState {
    /// The last answer, for the segment it belongs to.
    entry: Option<(SegmentId, Cached)>,
    /// The segment whose sidecar one caller is opening now (single flight:
    /// concurrent callers wait for its answer instead of opening it again).
    loading: Option<SegmentId>,
    /// Bumped by [`FmCache::clear`]: an open that started before a clear
    /// does not store its (possibly stale) answer.
    generation: u64,
}

/// Lazily opened sidecar, keyed by the segment it belongs to (a reload to
/// another segment re-checks). Single flight (issue #30): one caller opens
/// the file, callers arriving meanwhile wait for its answer (ready or
/// unusable alike); the lock is never held while the file is opened.
#[derive(Default)]
pub struct FmCache {
    inner: Mutex<CacheState>,
    /// Signalled when a `loading` open finishes.
    loaded: Condvar,
    /// TEST ONLY: how many times a sidecar file was opened and checked.
    #[cfg(test)]
    pub(crate) test_only_opens: std::sync::atomic::AtomicUsize,
}

impl FmCache {
    /// The sidecar for the index's current single segment, if the index has
    /// one segment, the file exists, and it was built over this dictionary.
    pub fn get(&self, dir: &Path, searcher: &tantivy::Searcher, text: Field) -> Option<Arc<FmIndex>> {
        match self.lookup(dir, searcher, text) {
            FmLookup::Ready(fm) => Some(fm),
            FmLookup::Missing | FmLookup::Unusable(_) => None,
        }
    }

    /// Drops the cached sidecar when it belongs to a segment that is no
    /// longer in `live` (after a merge), so its mapping is released and the
    /// file can be deleted (Windows cannot delete a mapped file, issue #29).
    pub fn forget_unless(&self, live: &[SegmentId]) {
        let mut state = self.state();
        if state.entry.as_ref().is_some_and(|(id, _)| !live.contains(id)) {
            state.entry = None;
            state.generation += 1;
        }
    }

    /// Forgets what is cached (after a sidecar build).
    pub fn clear(&self) {
        let mut state = self.state();
        state.entry = None;
        state.generation += 1;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached answer for `id`, if it still holds.
    fn cached(state: &CacheState, id: SegmentId, path: &Path) -> Option<FmLookup> {
        let (cached_id, cached) = state.entry.as_ref()?;
        if *cached_id != id {
            return None;
        }
        match cached {
            Cached::Ready(fm) => Some(FmLookup::Ready(fm.clone())),
            Cached::Unusable { identity, reason } => match file_identity(path) {
                Ok(now) if now == *identity => Some(FmLookup::Unusable(reason.clone())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(FmLookup::Missing),
                _ => None,
            },
        }
    }

    /// [`get`](Self::get), telling a missing sidecar from an unusable one.
    /// An unusable file is remembered with its identity (size, mtime) and
    /// checked again only when the file changes, the index moves to another
    /// segment, or a sidecar build clears the cache.
    pub fn lookup(&self, dir: &Path, searcher: &tantivy::Searcher, text: Field) -> FmLookup {
        self.lookup_cancellable(dir, searcher, text, None)
    }

    /// [`lookup`](Self::lookup) whose wait for another caller's open of the
    /// same sidecar stops when `cancel` is cancelled (the answer is then
    /// `Missing`; the caller sees its own cancellation next).
    pub(crate) fn lookup_cancellable(
        &self,
        dir: &Path,
        searcher: &tantivy::Searcher,
        text: Field,
        cancel: Option<&crate::CancelToken>,
    ) -> FmLookup {
        let segments = searcher.segment_readers();
        if segments.len() != 1 {
            return FmLookup::Missing;
        }
        let segment = &segments[0];
        let id = segment.segment_id();
        let path = sidecar_path(dir, id);
        let mut state = self.state();
        let generation = loop {
            if let Some(found) = Self::cached(&state, id, &path) {
                return found;
            }
            if state.loading != Some(id) {
                break state.generation;
            }
            if cancel.is_some_and(|t| t.is_cancelled()) {
                return FmLookup::Missing;
            }
            // the token has no waker: wake now and then to check it
            state = self
                .loaded
                .wait_timeout(state, std::time::Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        };
        // this caller opens; the others wait for its answer
        state.loading = Some(id);
        drop(state);
        let flight = Flight { cache: self, id };
        let result = self.open(&path, segment, text);
        let cached = match &result {
            FmLookup::Ready(fm) => Some(Cached::Ready(fm.clone())),
            FmLookup::Unusable(reason) => file_identity(&path)
                .ok()
                .map(|identity| Cached::Unusable { identity, reason: reason.clone() }),
            FmLookup::Missing => None,
        };
        let mut state = self.state();
        if state.generation == generation {
            state.entry = cached.map(|c| (id, c));
        }
        drop(state);
        drop(flight);
        result
    }

    /// TEST ONLY: marks `id` as being opened by another caller (as if an
    /// open were in flight); `false` ends it and wakes the waiters.
    #[cfg(test)]
    pub(crate) fn test_only_set_loading(&self, id: SegmentId, on: bool) {
        self.state().loading = if on { Some(id) } else { None };
        self.loaded.notify_all();
    }

    /// Opens and checks the sidecar file of `segment`.
    fn open(&self, path: &Path, segment: &tantivy::SegmentReader, text: Field) -> FmLookup {
        #[cfg(test)]
        self.test_only_opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FmLookup::Missing,
            Err(e) => return FmLookup::Unusable(format!("cannot open {}: {e}", path.display())),
        };
        // SAFETY: the file is only ever replaced through rename of a new file;
        // an open mapping keeps the old inode alive.
        let map = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => map,
            Err(e) => return FmLookup::Unusable(format!("cannot map {}: {e}", path.display())),
        };
        let fm = match FmIndex::open(Box::new(map)) {
            Ok(fm) => fm,
            Err(e) => return FmLookup::Unusable(format!("{}: {e}", path.display())),
        };
        let num_terms = match segment.inverted_index(text) {
            Ok(inverted) => inverted.terms().num_terms(),
            Err(e) => return FmLookup::Unusable(format!("dictionary: {e}")),
        };
        if fm.term_count() as usize != num_terms {
            return FmLookup::Unusable(format!(
                "{}: built over {} terms, the dictionary has {num_terms}",
                path.display(),
                fm.term_count()
            ));
        }
        FmLookup::Ready(Arc::new(fm))
    }
}

/// Ends a single-flight open (also on a panic): clears the loading mark and
/// wakes the waiters.
struct Flight<'a> {
    cache: &'a FmCache,
    id: SegmentId,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.state();
        if state.loading == Some(self.id) {
            state.loading = None;
        }
        drop(state);
        self.cache.loaded.notify_all();
    }
}
