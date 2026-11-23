//! The exact scan: every live slot of a set scored against one query, the
//! best `k` kept. A port of Otzaria's `src/semantic/oxv/scan.rs`
//! (otzaria-semantic-search d2a782a), step for step, `scan_with` included
//! (slots the filter does not reach, named one by one and weighed beside the
//! scan's hits: [`merge_also`]); progress reporting is not ported (no caller
//! needs it).
//!
//! - threads: each scans an equal share of the slots (`std::thread::scope`,
//!   the calling thread running the first share; scan.rs `run_shares`);
//! - a book filter becomes slot ranges: the admitted books' primary ranges
//!   plus the slots their extras and foreign records point to (scan.rs
//!   `Work::plan`);
//! - a lazy key: read only for a vector good enough for the top `k`.
//!
//! The ranking is a total order (score, then key, then where the slot is)
//! computed in integers ([`crate::kernel`]), so the same vectors give the
//! same hits in the same order on every CPU, thread count and segment
//! layout. A cancel is noticed every [`SCAN_CHECK_INTERVAL`] slots.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::kernel::{dot_f32, ordered, select, select_nibbles, unordered, DotKernel, NibbleKernel, PreparedQuery};
use crate::oxv::{Key, OxvFile, ALIAS_WINDOW};
use crate::{Error, Result};

/// cancellation.rs: a scan checks its token every this many slots.
pub const SCAN_CHECK_INTERVAL: usize = 1024;
/// scan.rs: below this many slots a share is not worth a thread of its own.
const MIN_SLOTS_PER_THREAD: usize = 4096;
/// resolve.rs: records kept per hit.
pub const MAX_RECORDS_PER_HIT: usize = 32;
/// scan.rs: a foreign record whose key no older segment holds live.
pub(crate) const LINK_UNRESOLVED: u16 = 1;

/// The books a search may return lines from, by Otzaria book name.
pub type BookSet = HashSet<String>;

/// resolve.rs `SlotRef`: one slot of the set, and the key it holds there.
/// What a resolver hands back for a scan to weigh besides the slots its
/// filter reaches (`CandidateResolver::unreached`); a slot that does not hold
/// its key (another generation) is passed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotRef {
    pub seg: u16,
    pub slot: u32,
    pub key: Key,
}

/// scan.rs `Link`: where one foreign record of a delta resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Link {
    pub seg: u16,
    pub flags: u16,
    pub slot: u32,
}

impl Link {
    pub(crate) fn target(&self) -> Option<(u16, u32)> {
        (self.flags & LINK_UNRESOLVED == 0).then_some((self.seg, self.slot))
    }
}

/// scan.rs `ReverseLink`: a foreign record, seen from the slot it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReverseLink {
    pub target_seg: u16,
    pub target_slot: u32,
    pub from_seg: u16,
    pub foreign: u32,
}

pub(crate) struct ScanSegment<'a> {
    pub segment: &'a OxvFile,
    /// One bit per slot, set for a slot no search may return.
    pub deleted: &'a [u64],
    pub links: &'a [Link],
}

impl ScanSegment<'_> {
    fn is_deleted(&self, slot: u32) -> bool {
        self.deleted.get(slot as usize / 64).is_some_and(|word| word & (1 << (slot % 64)) != 0)
    }
}

/// A set as the scan sees it: segments oldest first, reverse links sorted.
pub(crate) struct ScanSet<'a> {
    pub segments: &'a [ScanSegment<'a>],
    pub reverse_links: &'a [ReverseLink],
}

/// What to scan for (scan.rs `ScanRequest`).
#[derive(Debug, Clone, Copy)]
pub struct ScanRequest<'a> {
    pub top_k: usize,
    pub books: Option<&'a BookSet>,
    pub threads: usize,
}

/// Penalties choosing the representative occurrence before the top-k (scan.rs
/// `ScanPenalties`; Otzaria's defaults 0.04 / 0.04, profiles.rs).
#[derive(Debug, Clone, Copy)]
pub struct ScanPenalties {
    pub window: f32,
    pub alias: f32,
}

/// One record of a hit's key (resolve.rs `RecordRef`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    pub book: String,
    pub hint: u32,
    pub window: u8,
    /// An occurrence snapshot row's anchor: an alias row's own key (the short
    /// line's), the row's key otherwise; None for a record read off a slot.
    pub anchor: Option<crate::oxv::Key>,
}

/// One scanned vector (resolve.rs `VectorHit`).
#[derive(Debug, Clone, PartialEq)]
pub struct VectorHit {
    pub window_penalty: f32,
    /// Penalty applied to the representative occurrence during the scan.
    pub applied_penalty: f32,
    /// Codec similarity after the least penalty among admitted occurrences.
    pub score: f32,
    pub key: Key,
    /// Admitted books first, then the rest, at most [`MAX_RECORDS_PER_HIT`].
    pub records: Vec<RecordRef>,
    pub window: u8,
    pub seg: u16,
    pub slot: u32,
}

/// scan.rs `default_scan_threads`: half the cores, at most eight (a scan is
/// memory-bound).
pub fn default_scan_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |c| c.get());
    (cores / 2).clamp(1, 8)
}

fn checkpoint(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed)) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

/// scan.rs `scan_all` (without progress): the scan, and the slots of `also`
/// besides ([`merge_also`]); with `also` empty, exactly the scan.
pub(crate) fn scan(
    set: &ScanSet<'_>,
    query: &PreparedQuery,
    request: &ScanRequest<'_>,
    also: &[SlotRef],
    penalties: ScanPenalties,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<VectorHit>> {
    checkpoint(cancel)?;
    if !penalties.window.is_finite() || penalties.window < 0.0 || !penalties.alias.is_finite() || penalties.alias < 0.0 {
        return Err(Error::Invalid("window and alias penalties must be finite and nonnegative".into()));
    }
    if request.top_k == 0 {
        return Ok(Vec::new());
    }
    let work = Work::plan(set, request.books);
    let top_k = request.top_k.min(work.total.max(1));
    let threads = request.threads.max(1).min(work.total.div_ceil(MIN_SLOTS_PER_THREAD).max(1));
    let shares = work.split(threads);
    let books = request.books;
    match query {
        PreparedQuery::Int8 { q16, inv_scale } => {
            let scorer = Int8 { q16, inv_scale: *inv_scale, kernel: select().0 };
            let found = run_shares(set, &scorer, books, penalties, top_k, &shares, cancel)?;
            let found = merge_also(set, &scorer, request, penalties, found, also, cancel)?;
            Ok(hits(set, found, books, penalties, &scorer))
        }
        PreparedQuery::Int8PerVector { q16, inv_scale } => {
            let scorer = Int8PerVector { q16, inv_scale: *inv_scale, kernel: select().0 };
            let found = run_shares(set, &scorer, books, penalties, top_k, &shares, cancel)?;
            let found = merge_also(set, &scorer, request, penalties, found, also, cancel)?;
            Ok(hits(set, found, books, penalties, &scorer))
        }
        PreparedQuery::Rotated { head, inv_head, tail_lo, tail_hi, inv_tail, constant, .. } => {
            let scorer = Rotated {
                head,
                inv_head: *inv_head,
                tail_lo,
                tail_hi,
                inv_tail: *inv_tail,
                constant: *constant,
                dot: select().0,
                nibbles: select_nibbles().0,
            };
            let found = run_shares(set, &scorer, books, penalties, top_k, &shares, cancel)?;
            let found = merge_also(set, &scorer, request, penalties, found, also, cancel)?;
            Ok(hits(set, found, books, penalties, &scorer))
        }
        PreparedQuery::Float { q } => {
            let scorer = Float { q };
            let found = run_shares(set, &scorer, books, penalties, top_k, &shares, cancel)?;
            let found = merge_also(set, &scorer, request, penalties, found, also, cancel)?;
            Ok(hits(set, found, books, penalties, &scorer))
        }
    }
}

/// scan.rs `Scorer`: a stored vector to a rank, a rank back to a score.
trait Scorer: Sync {
    fn rank(&self, vector: &[u8], scale: f32) -> i32;
    fn score(&self, rank: i32) -> f32;
    fn penalize(&self, rank: i32, penalty: f32) -> i32;
}

struct Int8<'a> {
    q16: &'a [i16],
    inv_scale: f32,
    kernel: DotKernel,
}

impl Scorer for Int8<'_> {
    #[inline]
    fn rank(&self, vector: &[u8], _scale: f32) -> i32 {
        (self.kernel)(self.q16, vector)
    }
    fn score(&self, rank: i32) -> f32 {
        rank as f32 * self.inv_scale
    }
    fn penalize(&self, rank: i32, penalty: f32) -> i32 {
        rank.saturating_sub((penalty / self.inv_scale).round() as i32)
    }
}

struct Int8PerVector<'a> {
    q16: &'a [i16],
    inv_scale: f32,
    kernel: DotKernel,
}

impl Scorer for Int8PerVector<'_> {
    #[inline]
    fn rank(&self, vector: &[u8], scale: f32) -> i32 {
        ordered((self.kernel)(self.q16, vector) as f32 * scale)
    }
    fn score(&self, rank: i32) -> f32 {
        unordered(rank) * self.inv_scale
    }
    fn penalize(&self, rank: i32, penalty: f32) -> i32 {
        ordered(unordered(rank) - penalty / self.inv_scale)
    }
}

struct Rotated<'a> {
    head: &'a [i16],
    inv_head: f32,
    tail_lo: &'a [i16],
    tail_hi: &'a [i16],
    inv_tail: f32,
    constant: f32,
    dot: DotKernel,
    nibbles: NibbleKernel,
}

impl Scorer for Rotated<'_> {
    #[inline]
    fn rank(&self, vector: &[u8], scale: f32) -> i32 {
        let (codes, packed) = vector.split_at(self.head.len());
        let acc8 = (self.dot)(self.head, codes);
        let acc4 = (self.nibbles)(self.tail_lo, self.tail_hi, packed);
        ordered(acc8 as f32 * scale * self.inv_head + (self.constant + acc4 as f32 * self.inv_tail))
    }
    fn score(&self, rank: i32) -> f32 {
        unordered(rank)
    }
    fn penalize(&self, rank: i32, penalty: f32) -> i32 {
        ordered(unordered(rank) - penalty)
    }
}

struct Float<'a> {
    q: &'a [f32],
}

impl Scorer for Float<'_> {
    #[inline]
    fn rank(&self, vector: &[u8], _scale: f32) -> i32 {
        ordered(dot_f32(self.q, vector))
    }
    fn score(&self, rank: i32) -> f32 {
        unordered(rank)
    }
    fn penalize(&self, rank: i32, penalty: f32) -> i32 {
        ordered(unordered(rank) - penalty)
    }
}

/// A run of slots in one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    seg: u16,
    start: u32,
    end: u32,
}

struct Work {
    pieces: Vec<Piece>,
    total: usize,
}

impl Work {
    /// scan.rs `Work::plan`.
    fn plan(set: &ScanSet<'_>, books: Option<&BookSet>) -> Self {
        let mut pieces = Vec::new();
        match books {
            None => {
                for (seg, segment) in set.segments.iter().enumerate() {
                    let slots = segment.segment.slot_count();
                    if slots > 0 {
                        pieces.push(Piece { seg: seg as u16, start: 0, end: slots });
                    }
                }
            }
            Some(books) => {
                let admitted: Vec<Vec<bool>> =
                    set.segments.iter().map(|s| s.segment.books().iter().map(|b| books.contains(&b.name)).collect()).collect();
                let admits = |seg: u16, slot: u32| {
                    let segment = set.segments[seg as usize].segment;
                    admitted[seg as usize][segment.book_of_slot(slot)]
                };
                let mut gathered: HashSet<(u16, u32)> = HashSet::new();
                for (seg, segment) in set.segments.iter().enumerate() {
                    let seg = seg as u16;
                    for (index, book) in segment.segment.books().iter().enumerate() {
                        if !admitted[seg as usize][index] {
                            continue;
                        }
                        if !book.slots.is_empty() {
                            pieces.push(Piece { seg, start: book.slots.start, end: book.slots.end });
                        }
                        for extra in book.extras.clone() {
                            let (slot, _) = segment.segment.extra(extra);
                            if !admits(seg, slot) {
                                gathered.insert((seg, slot));
                            }
                        }
                        for foreign in book.foreign.clone() {
                            if let Some((ts, tslot)) = segment.links.get(foreign as usize).and_then(Link::target) {
                                if (ts as usize) < set.segments.len() && !admits(ts, tslot) {
                                    gathered.insert((ts, tslot));
                                }
                            }
                        }
                    }
                }
                let mut gathered: Vec<(u16, u32)> = gathered.into_iter().collect();
                gathered.sort_unstable();
                pieces.extend(gathered.into_iter().map(|(seg, slot)| Piece { seg, start: slot, end: slot + 1 }));
            }
        }
        let total = pieces.iter().map(|p| (p.end - p.start) as usize).sum();
        Work { pieces, total }
    }

    /// scan.rs `Work::split`: `shares` runs of equal slot counts, in order.
    fn split(&self, shares: usize) -> Vec<Vec<Piece>> {
        let mut out: Vec<Vec<Piece>> = vec![Vec::new(); shares];
        let mut pieces = self.pieces.iter().copied();
        let mut current = pieces.next();
        for (index, share) in out.iter_mut().enumerate() {
            let mut want = self.total * (index + 1) / shares - self.total * index / shares;
            while want > 0 {
                let Some(piece) = current.as_mut() else { break };
                let take = want.min((piece.end - piece.start) as usize) as u32;
                share.push(Piece { seg: piece.seg, start: piece.start, end: piece.start + take });
                piece.start += take;
                want -= take as usize;
                if piece.start == piece.end {
                    current = pieces.next();
                }
            }
        }
        out
    }
}

/// scan.rs `run_shares`.
fn run_shares(
    set: &ScanSet<'_>,
    scorer: &impl Scorer,
    books: Option<&BookSet>,
    penalties: ScanPenalties,
    k: usize,
    shares: &[Vec<Piece>],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Candidate>> {
    let results: Vec<Result<TopK>> = std::thread::scope(|scope| {
        let handles: Vec<_> = shares
            .iter()
            .skip(1)
            .map(|share| scope.spawn(move || scan_share(set, scorer, books, penalties, k, share, cancel)))
            .collect();
        let mut results = vec![scan_share(set, scorer, books, penalties, k, &shares[0], cancel)];
        results.extend(handles.into_iter().map(|h| h.join().expect("a scan thread must not panic")));
        results
    });
    let mut all = Vec::with_capacity(k * shares.len());
    for result in results {
        all.extend(result?.heap.into_vec());
    }
    all.sort_unstable_by(Candidate::best_first);
    // one vector per key: the better-ranked copy stands for both
    let mut seen = HashSet::with_capacity(all.len());
    all.retain(|c| seen.insert(c.key));
    all.truncate(k);
    Ok(all)
}

/// scan.rs `merge_also`: the slots of `also` that are live and hold their
/// key, scored as the scan scores (penalized by their least role, under the
/// filter or else over every book); of those whose key the scan did not
/// return, the best `request.top_k` are merged into its hits in the scan's
/// order. None of the scan's hits gives way to them: up to twice
/// `request.top_k` hits.
fn merge_also(
    set: &ScanSet<'_>,
    scorer: &impl Scorer,
    request: &ScanRequest<'_>,
    penalties: ScanPenalties,
    mut found: Vec<Candidate>,
    also: &[SlotRef],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Candidate>> {
    let k = request.top_k;
    if also.is_empty() {
        return Ok(found);
    }
    let returned: HashSet<Key> = found.iter().map(|c| c.key).collect();
    let mut weighed = Vec::new();
    for (visited, wanted) in also.iter().enumerate() {
        if visited % SCAN_CHECK_INTERVAL == 0 {
            checkpoint(cancel)?;
        }
        let Some(segment) = set.segments.get(wanted.seg as usize) else { continue };
        if wanted.slot >= segment.segment.slot_count() || segment.is_deleted(wanted.slot) {
            continue;
        }
        // the key as the segment holds it, at its width: what the scan's hits carry
        let key = segment.segment.key(wanted.slot);
        if !crate::chunker::ChunkKey(key).same_text(crate::chunker::ChunkKey(wanted.key)) || returned.contains(&key) {
            continue;
        }
        let width = segment.segment.codec().bytes_per_vector(segment.segment.dims());
        let start = wanted.slot as usize * width;
        let vector = &segment.segment.vector_bytes()[start..start + width];
        let scale = segment
            .segment
            .vector_scale_bytes()
            .map_or(1.0, |s| f32::from_le_bytes(s[wanted.slot as usize * 4..wanted.slot as usize * 4 + 4].try_into().unwrap()));
        let mut rank = scorer.rank(vector, scale);
        let Some(window) =
            effective_role(set, wanted.seg, wanted.slot, request.books, penalties).or_else(|| effective_role(set, wanted.seg, wanted.slot, None, penalties))
        else {
            continue;
        };
        let applied = penalty_for(window, penalties);
        if applied != 0.0 {
            rank = scorer.penalize(rank, applied);
        }
        weighed.push(Candidate { rank, key, seg: wanted.seg, slot: wanted.slot, window });
    }
    weighed.sort_unstable_by(Candidate::best_first);
    // a key the set holds live twice, or named twice: the better-ranked copy stands
    let mut seen = HashSet::with_capacity(weighed.len());
    weighed.retain(|c| seen.insert(c.key));
    weighed.truncate(k);
    found.extend(weighed);
    // a total order: the same merge whatever order either list came in
    found.sort_unstable_by(Candidate::best_first);
    Ok(found)
}

/// scan.rs `scan_share` + `Pass::run`.
fn scan_share(
    set: &ScanSet<'_>,
    scorer: &impl Scorer,
    books: Option<&BookSet>,
    penalties: ScanPenalties,
    k: usize,
    share: &[Piece],
    cancel: Option<&AtomicBool>,
) -> Result<TopK> {
    let mut top = TopK::new(k);
    let mut visited = 0usize;
    for piece in share {
        let segment = &set.segments[piece.seg as usize];
        let width = segment.segment.codec().bytes_per_vector(segment.segment.dims());
        let vectors = &segment.segment.vector_bytes()[piece.start as usize * width..piece.end as usize * width];
        let scales = segment.segment.vector_scale_bytes().map(|s| &s[piece.start as usize * 4..piece.end as usize * 4]);
        for (offset, vector) in vectors.chunks_exact(width).enumerate() {
            if visited % SCAN_CHECK_INTERVAL == 0 {
                checkpoint(cancel)?;
            }
            visited += 1;
            let slot = piece.start + offset as u32;
            if segment.is_deleted(slot) {
                continue;
            }
            let scale = scales.map_or(1.0, |s| f32::from_le_bytes(s[offset * 4..offset * 4 + 4].try_into().unwrap()));
            let mut rank = scorer.rank(vector, scale);
            // a nonnegative penalty cannot promote a raw score below the heap's worst
            if top.heap.len() == top.k && top.heap.peek().is_some_and(|worst| rank < worst.rank) {
                continue;
            }
            let Some(window) = effective_role(set, piece.seg, slot, books, penalties) else {
                continue;
            };
            let applied = penalty_for(window, penalties);
            if applied != 0.0 {
                rank = scorer.penalize(rank, applied);
            }
            top.offer(rank, piece.seg, slot, window, || segment.segment.key(slot));
        }
    }
    Ok(top)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    rank: i32,
    key: Key,
    seg: u16,
    slot: u32,
    window: u8,
}

impl Candidate {
    /// scan.rs: higher rank first, then the smaller key, then the earlier slot.
    fn best_first(a: &Self, b: &Self) -> Ordering {
        b.rank
            .cmp(&a.rank)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| (a.seg, a.slot).cmp(&(b.seg, b.slot)))
            .then_with(|| a.window.cmp(&b.window))
    }
}

/// Ordered worst first, so a max-heap's top is the candidate to evict.
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        Candidate::best_first(self, other)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct TopK {
    k: usize,
    heap: BinaryHeap<Candidate>,
}

impl TopK {
    fn new(k: usize) -> Self {
        TopK { k, heap: BinaryHeap::with_capacity(k + 1) }
    }

    #[inline]
    fn offer(&mut self, rank: i32, seg: u16, slot: u32, window: u8, key: impl FnOnce() -> Key) {
        if self.heap.len() < self.k {
            self.heap.push(Candidate { rank, key: key(), seg, slot, window });
            return;
        }
        let Some(worst) = self.heap.peek() else { return };
        if rank < worst.rank {
            return;
        }
        let candidate = Candidate { rank, key: key(), seg, slot, window };
        if Candidate::best_first(&candidate, worst) == Ordering::Less {
            self.heap.pop();
            self.heap.push(candidate);
        }
    }
}

fn hits(set: &ScanSet<'_>, found: Vec<Candidate>, books: Option<&BookSet>, penalties: ScanPenalties, scorer: &impl Scorer) -> Vec<VectorHit> {
    found
        .into_iter()
        .map(|c| VectorHit {
            window_penalty: penalties.window,
            applied_penalty: penalty_for(c.window, penalties),
            score: scorer.score(c.rank),
            key: c.key,
            records: records(set, c.seg, c.slot, books, penalties),
            window: c.window,
            seg: c.seg,
            slot: c.slot,
        })
        .collect()
}

pub(crate) fn penalty_for(window: u8, penalties: ScanPenalties) -> f32 {
    match window {
        0 => 0.0,
        ALIAS_WINDOW => penalties.alias,
        _ => penalties.window,
    }
}

fn penalty_sort_key(window: u8, penalties: ScanPenalties) -> u32 {
    let penalty = penalty_for(window, penalties);
    if penalty == 0.0 {
        0
    } else {
        penalty.to_bits()
    }
}

/// scan.rs `effective_role`: the cheapest role among the current occurrence
/// snapshot admitted by this query (None: no admitted occurrence).
fn effective_role(set: &ScanSet<'_>, seg: u16, slot: u32, books: Option<&BookSet>, penalties: ScanPenalties) -> Option<u8> {
    let source = set.segments[seg as usize].segment;
    if books.is_none() && seg as usize + 1 == set.segments.len() {
        let stored = source.slot_window(slot);
        if penalties.alias >= penalties.window || stored == 0 || stored == ALIAS_WINDOW {
            return Some(stored);
        }
        let key = source.key(slot);
        return source.key_occurrences(&key).map(|rows| least_role(rows.iter().map(|r| r.window), penalties)).unwrap_or(Some(stored));
    }
    let snapshots = if books.is_none() { &set.segments[seg as usize + 1..] } else { set.segments };
    let key = source.key(slot);
    for view in snapshots.iter().rev() {
        if let Some(snapshot) = view.segment.key_occurrences(&key) {
            return least_role(
                snapshot
                    .iter()
                    .filter(|r| books.is_none_or(|b| view.segment.books().get(r.book as usize).is_some_and(|x| b.contains(&x.name))))
                    .map(|r| r.window),
                penalties,
            );
        }
    }
    let stored = source.slot_window(slot);
    if penalties.alias < penalties.window && stored > 0 && stored < ALIAS_WINDOW {
        return source.key_occurrences(&key).map(|rows| least_role(rows.iter().map(|r| r.window), penalties)).unwrap_or(Some(stored));
    }
    Some(stored)
}

fn least_role(windows: impl Iterator<Item = u8>, penalties: ScanPenalties) -> Option<u8> {
    windows.min_by(|a, b| penalty_sort_key(*a, penalties).cmp(&penalty_sort_key(*b, penalties)).then_with(|| a.cmp(b)))
}

/// scan.rs `records`: every record of a slot's key (primary, extras, the
/// foreign records of later deltas resolving to it; or the newest snapshot),
/// admitted books first, at most [`MAX_RECORDS_PER_HIT`].
fn records(set: &ScanSet<'_>, seg: u16, slot: u32, books: Option<&BookSet>, penalties: ScanPenalties) -> Vec<RecordRef> {
    let segment = set.segments[seg as usize].segment;
    let key = segment.key(slot);
    for view in set.segments.iter().rev() {
        if let Some(snapshot) = view.segment.key_occurrences(&key) {
            return snapshot_records(view.segment, &snapshot, books, penalties);
        }
    }
    let mut out = Vec::new();
    let window = segment.slot_window(slot);
    out.push(RecordRef { book: segment.books()[segment.book_of_slot(slot)].name.clone(), hint: segment.hint(slot), window, anchor: None });
    if segment.has_extras(slot) {
        for extra in segment.extras_of_slot(slot) {
            let (_, hint) = segment.extra(extra);
            out.push(RecordRef { book: segment.books()[segment.book_of_extra(extra)].name.clone(), hint, window, anchor: None });
        }
    }
    let first = set.reverse_links.partition_point(|l| (l.target_seg, l.target_slot) < (seg, slot));
    for link in &set.reverse_links[first..] {
        if (link.target_seg, link.target_slot) != (seg, slot) {
            break;
        }
        let Some(from) = set.segments.get(link.from_seg as usize) else { continue };
        if link.foreign >= from.segment.foreign_count() {
            continue;
        }
        let (_, book, hint) = from.segment.foreign_record(link.foreign);
        if let Some(book) = from.segment.books().get(book as usize) {
            out.push(RecordRef { book: book.name.clone(), hint, window, anchor: None });
        }
    }
    finish_records(&mut out, books, penalties);
    out
}

/// scan.rs `snapshot_records`: fair representatives of a snapshot, at most
/// 32 parents per book and 32 records per filter class.
fn snapshot_records(segment: &OxvFile, snapshot: &[crate::oxv::Occ], books: Option<&BookSet>, penalties: ScanPenalties) -> Vec<RecordRef> {
    type Ranked = ((usize, u32, u8, u32), RecordRef);
    let flush = |group: &mut Vec<crate::oxv::Occ>, admitted: &mut Vec<Ranked>, other: &mut Vec<Ranked>| {
        let Some(first) = group.first() else { return };
        let Some(book) = segment.books().get(first.book as usize) else {
            group.clear();
            return;
        };
        let selected = if books.is_none_or(|b| b.contains(&book.name)) { admitted } else { other };
        for (round, row) in group.drain(..).enumerate() {
            let rank = (round, penalty_sort_key(row.window, penalties), row.window, row.book);
            let at = selected.partition_point(|(o, _)| *o < rank);
            if at < MAX_RECORDS_PER_HIT {
                selected.insert(at, (rank, RecordRef { book: book.name.clone(), hint: row.hint, window: row.window, anchor: Some(row.anchor) }));
                selected.truncate(MAX_RECORDS_PER_HIT);
            }
        }
    };
    let (mut admitted, mut other, mut group) = (Vec::new(), Vec::new(), Vec::new());
    let mut current = None;
    for row in snapshot {
        if current != Some(row.book) {
            flush(&mut group, &mut admitted, &mut other);
            current = Some(row.book);
        }
        let row_rank = (penalty_sort_key(row.window, penalties), row.window);
        let at = group.partition_point(|o: &crate::oxv::Occ| (penalty_sort_key(o.window, penalties), o.window) <= row_rank);
        if at < MAX_RECORDS_PER_HIT {
            group.insert(at, *row);
            group.truncate(MAX_RECORDS_PER_HIT);
        }
    }
    flush(&mut group, &mut admitted, &mut other);
    admitted.into_iter().chain(other).take(MAX_RECORDS_PER_HIT).map(|(_, r)| r).collect()
}

/// scan.rs `finish_records`: a frequent parent must not crowd every other
/// book out of a shared vector's cap.
fn finish_records(records: &mut Vec<RecordRef>, books: Option<&BookSet>, penalties: ScanPenalties) {
    records.sort_by_key(|r| (penalty_sort_key(r.window, penalties), r.window));
    let mut rounds: HashMap<String, usize> = HashMap::new();
    let mut ranked: Vec<_> = records
        .drain(..)
        .map(|r| {
            let round = rounds.entry(r.book.clone()).or_insert(0usize);
            let rank = (books.is_some_and(|b| !b.contains(&r.book)), *round);
            *round += 1;
            (rank, r)
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    records.extend(ranked.into_iter().take(MAX_RECORDS_PER_HIT).map(|(_, r)| r));
}
