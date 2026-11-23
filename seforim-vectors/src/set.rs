//! An Otzaria vector set opened read-only (issue #18): the directory
//! Otzaria installs (`%APPDATA%\otzaria\vectors`: `CURRENT` / `PREVIOUS`,
//! `gen-NNNNNN/set.json` with a `.del` bitmap per segment and a `.links`
//! table per delta, `segments/*.oxv`), or one plain base `.oxv`. A port of
//! the reading half of Otzaria's `src/semantic/segment_set/{mod,files}.rs`
//! (`SegmentSet::open`, `is_live`, links, scan); nothing is written, no
//! recovery, no install, no compaction: the set is Otzaria's own copy.
//!
//! The scan's hits (book, hint, window, key) are resolved to live lines by a
//! [`CandidateResolver`] (Otzaria's `resolve.rs` trait; the engine's resolver
//! reads the index's `chunk_key` column, semantic_resolver.rs);
//! [`SegmentSet::search_lines`] scans, resolves and cuts.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::kernel::PreparedQuery;
use crate::oxv::OxvFile;
use crate::scan::{self, BookSet, Link, ReverseLink, ScanPenalties, ScanRequest, ScanSegment, ScanSet, LINK_UNRESOLVED};
use crate::{Error, Result};

/// Otzaria's default window and alias penalty (config/profiles.rs).
pub const DEFAULT_WINDOW_PENALTY: f32 = 0.04;

const CURRENT: &str = "CURRENT";
const PREVIOUS: &str = "PREVIOUS";
const SET_FILE: &str = "set.json";
const SET_FORMAT: &str = "otzaria-vector-set";
const SET_FORMAT_VERSION: u32 = 1;
const DEL_MAGIC: &[u8; 8] = b"OXVDEL1\n";
const LINKS_MAGIC: &[u8; 8] = b"OXVLNK1\n";

fn corrupt(msg: impl Into<String>) -> Error {
    Error::Format(format!("vector set: {}", msg.into()))
}

/// files.rs `Pointer`.
#[derive(Debug, Deserialize)]
struct Pointer {
    generation: u64,
    set: String,
    set_sha256: String,
}

/// files.rs `SetDocument` (the fields reading needs).
#[derive(Debug, Deserialize)]
struct SetDocument {
    format: String,
    format_version: u32,
    generation: u64,
    library_version: u32,
    library_release_tag: String,
    segments: Vec<SetSegment>,
    #[serde(default)]
    identity: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct SetSegment {
    id: String,
    file: String,
    kind: String,
    from: u32,
    to: u32,
    size: u64,
    slots: u64,
    del: DerivedFile,
    #[serde(default)]
    links: Option<DerivedFile>,
}

#[derive(Debug, Deserialize)]
struct DerivedFile {
    file: String,
    crc32: u32,
    count: u64,
}

/// What an open set holds (JSON at the C ABI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetInfo {
    /// The directory or file opened.
    pub path: String,
    /// The generation read (0: a single `.oxv`).
    pub generation: u64,
    /// True when CURRENT did not open and PREVIOUS did.
    pub recovered_from_previous: bool,
    pub dims: usize,
    pub codec: String,
    pub library_version: u32,
    pub library_release_tag: String,
    pub segments: Vec<SegmentInfo>,
    pub slots_live: u64,
    pub slots_dead: u64,
    /// Distinct book names in the set.
    pub books: u64,
    /// Books of the set the index holds.
    pub books_mapped: u64,
    /// Books of the set the index does not hold.
    pub books_unmapped: u64,
    pub bytes_on_disk: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub file: String,
    pub kind: String,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    pub slots_dead: u64,
    pub foreign: u64,
    pub foreign_unresolved: u64,
    pub size: u64,
}

/// resolve.rs `CandidateResolver`: the live lines of the scanned vectors.
pub trait CandidateResolver {
    /// The set's book names of the host's books `ids` (a filter).
    fn admissible_books(&self, ids: &[u64]) -> BookSet;

    /// The lines `hits` resolve to in the admitted `books` (None: every book):
    /// each line once, at its best occurrence (`score + applied_penalty -
    /// penalty(window)`), best first; or the host rows the check needs first.
    fn resolve(&self, hits: &[scan::VectorHit], books: Option<&BookSet>, options: &LineSearch, cancel: Option<&AtomicBool>) -> Result<Resolution>;

    /// resolve.rs `CandidateResolver::unreached`: the vectors a scan of the
    /// admitted `books` does not reach although live lines of those books hold
    /// their texts (texts that moved or were copied into an admitted book since
    /// the set was built), slots of the set's generation `set_generation`.
    /// Asked only of a filtered search; weighed beside the scan's hits
    /// ([`SegmentSet::scan_with`]). The default is none.
    fn unreached(&self, _books: &BookSet, _set_generation: u64, _cancel: Option<&AtomicBool>) -> Result<Vec<scan::SlotRef>> {
        Ok(Vec::new())
    }
}

/// What a resolver returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// The lines, and how many admitted records resolved to none.
    Lines { lines: Vec<LineHit>, unresolved: u64 },
    /// The host line ids whose rows the resolver needs before it can answer.
    NeedRows(Vec<u64>),
}

/// A line search's scan, kept so that a resolution waiting for host rows
/// does not scan again.
#[derive(Debug, Clone)]
pub struct ScannedLines {
    hits: Vec<scan::VectorHit>,
    books: Option<BookSet>,
    result: LineResult,
}

impl ScannedLines {
    /// The scan's vectors.
    pub fn hits(&self) -> &[scan::VectorHit] {
        &self.hits
    }

    /// The lines (cut to `options.k` when `cut`), or the rows to fetch first.
    pub fn resolve(&self, resolver: &dyn CandidateResolver, options: &LineSearch, cut: bool, cancel: Option<&AtomicBool>) -> Result<Resolved> {
        let mut result = self.result.clone();
        if self.hits.is_empty() {
            return Ok(Resolved::Done(result));
        }
        match resolver.resolve(&self.hits, self.books.as_ref(), options, cancel)? {
            Resolution::NeedRows(rows) => Ok(Resolved::NeedRows(rows)),
            Resolution::Lines { mut lines, unresolved } => {
                if cut {
                    lines.truncate(options.k);
                }
                result.unmapped = unresolved;
                result.hits = lines;
                Ok(Resolved::Done(result))
            }
        }
    }
}

/// A line search's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Done(LineResult),
    /// Host rows to hand the resolver before asking again.
    NeedRows(Vec<u64>),
}

/// A semantic search over a set (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LineSearch {
    /// Lines to return (the scan keeps this many vectors, Otzaria's top_k).
    pub k: usize,
    /// Only these host books. None = all.
    pub books: Option<Vec<u64>>,
    /// Scan threads; 0 = Otzaria's default (half the cores, at most 8).
    pub threads: usize,
    /// Taken off a window past a line's first (Otzaria 0.04).
    pub window_penalty: f32,
    /// Taken off a short line found through a neighbour's vector (Otzaria 0.04).
    pub alias_penalty: f32,
}

impl Default for LineSearch {
    fn default() -> Self {
        LineSearch { k: 10, books: None, threads: 0, window_penalty: DEFAULT_WINDOW_PENALTY, alias_penalty: DEFAULT_WINDOW_PENALTY }
    }
}

/// One line found.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LineHit {
    pub line_id: u64,
    pub book_id: u64,
    /// The vector's similarity minus this occurrence's penalty.
    pub score: f32,
    /// The occurrence's window: 0 the line's first, n its n-th, bit 31 an
    /// alias (a short line sharing a neighbour's vector).
    pub chunk: u32,
}

/// A line search's hits and counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LineResult {
    pub hits: Vec<LineHit>,
    /// Vectors the scan returned.
    pub vectors: u64,
    /// Records of those vectors read.
    pub records: u64,
    /// Admitted records that resolved to no live line: counted, never guessed.
    pub unmapped: u64,
    /// Slots the scan visited.
    pub scanned: u64,
    /// Scan threads used.
    pub threads: usize,
}

/// An open set (segments mapped read-only).
pub struct SegmentSet {
    path: PathBuf,
    generation: u64,
    recovered: bool,
    library_version: u32,
    tag: String,
    segments: Vec<OxvFile>,
    deleted: Vec<Vec<u64>>,
    links: Vec<Vec<Link>>,
    reverse: Vec<ReverseLink>,
    files: Vec<String>,
    /// The model's `chunking_identity` from set.json (None: a single `.oxv`).
    chunking: Option<u64>,
}

impl SegmentSet {
    /// Opens a set directory (CURRENT, falling back to PREVIOUS as Otzaria's
    /// `open_unlocked`) or one base / compacted `.oxv` file.
    pub fn open(path: &Path) -> Result<SegmentSet> {
        if path.is_file() {
            let segment = OxvFile::open(path)?;
            if segment.kind() == "delta" {
                return Err(corrupt("a delta segment opens only inside its set"));
            }
            let (_, to) = segment.library_versions();
            let tag = segment.release_tag().to_string();
            let words = (segment.slot_count() as usize).div_ceil(64);
            return Ok(SegmentSet {
                path: path.into(),
                generation: 0,
                recovered: false,
                library_version: to,
                tag,
                deleted: vec![vec![0; words]],
                links: vec![Vec::new()],
                reverse: Vec::new(),
                files: vec![path.to_string_lossy().into_owned()],
                segments: vec![segment],
                chunking: None,
            });
        }
        let current = read_pointer(path, CURRENT);
        let previous = read_pointer(path, PREVIOUS);
        let first = match &current {
            Ok(Some(p)) => match Self::open_generation(path, p) {
                Ok(set) => return Ok(set),
                Err(e) => e,
            },
            Ok(None) => corrupt(format!("{}: no vector set is installed here (no CURRENT)", path.display())),
            Err(e) => corrupt(e.clone()),
        };
        match &previous {
            Ok(Some(p)) if !matches!(&current, Ok(Some(c)) if c.generation == p.generation) => match Self::open_generation(path, p) {
                Ok(mut set) => {
                    set.recovered = true;
                    Ok(set)
                }
                Err(_) => Err(first),
            },
            _ => Err(first),
        }
    }

    /// segment_set/mod.rs `open_generation`.
    fn open_generation(dir: &Path, pointer: &Pointer) -> Result<SegmentSet> {
        let gen_dir = format!("gen-{:06}", pointer.generation);
        if pointer.set != format!("{gen_dir}/{SET_FILE}") {
            return Err(corrupt(format!("the pointer names {}, not generation {}'s set", pointer.set, pointer.generation)));
        }
        let bytes = fs::read(dir.join(&pointer.set))?;
        if sha256_hex(&bytes) != pointer.set_sha256.to_ascii_lowercase() {
            return Err(corrupt(format!("{} is not the file its pointer names", pointer.set)));
        }
        let doc: SetDocument = serde_json::from_slice(&bytes).map_err(|e| corrupt(format!("{}: {e}", pointer.set)))?;
        if doc.format != SET_FORMAT || doc.format_version != SET_FORMAT_VERSION {
            return Err(corrupt(format!("{} version {} is not {SET_FORMAT} {SET_FORMAT_VERSION}", doc.format, doc.format_version)));
        }
        if doc.generation != pointer.generation || doc.segments.is_empty() {
            return Err(corrupt("the set document contradicts its pointer or names no segment"));
        }
        let gen_path = dir.join(&gen_dir);
        let (mut segments, mut deleted, mut links, mut files) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (i, entry) in doc.segments.iter().enumerate() {
            if (i == 0) == (entry.kind == "delta") || entry.links.is_some() != (entry.kind == "delta") {
                return Err(corrupt(format!("segment {i} is a {}: a set is one base or compacted segment followed by deltas", entry.kind)));
            }
            if entry.del.file != format!("{}.del", entry.id) || entry.file.contains("..") {
                return Err(corrupt(format!("segment {i}'s files are not named by its id")));
            }
            let segment = OxvFile::open(&dir.join(&entry.file))?;
            let (from, to) = segment.library_versions();
            if segment.size() != entry.size
                || segment.slot_count() as u64 != entry.slots
                || segment.kind() != entry.kind
                || (from, to) != (entry.from, entry.to)
            {
                return Err(corrupt(format!("segment {} is not the one its set names", entry.id)));
            }
            if let Some(first) = segments.first() {
                let first: &OxvFile = first;
                if first.codec_params() != segment.codec_params() || first.dims() != segment.dims() {
                    return Err(corrupt("the segments are of different codec epochs"));
                }
            }
            let del = fs::read(gen_path.join(&entry.del.file))?;
            deleted.push(decode_deleted(&del, &entry.del, entry.slots)?);
            links.push(match &entry.links {
                Some(f) => decode_links(&fs::read(gen_path.join(&f.file))?, f, segment.foreign_count() as u64)?,
                None => Vec::new(),
            });
            files.push(entry.file.clone());
            segments.push(segment);
        }
        let mut reverse = Vec::new();
        for (from, l) in links.iter().enumerate() {
            for (foreign, link) in l.iter().enumerate() {
                if let Some((seg, slot)) = link.target() {
                    if seg as usize >= from || slot >= segments[seg as usize].slot_count() {
                        return Err(corrupt(format!("a link of segment {from} points to segment {seg} slot {slot}")));
                    }
                    reverse.push(ReverseLink { target_seg: seg, target_slot: slot, from_seg: from as u16, foreign: foreign as u32 });
                }
            }
        }
        reverse.sort_unstable();
        Ok(SegmentSet {
            path: dir.into(),
            generation: doc.generation,
            recovered: false,
            library_version: doc.library_version,
            tag: doc.library_release_tag,
            segments,
            deleted,
            links,
            reverse,
            files,
            chunking: doc.identity.pointer("/model/chunking_identity").and_then(|v| v.as_u64()),
        })
    }

    pub fn dims(&self) -> usize {
        self.segments[0].dims()
    }

    /// The segments, their dead-slot words and their links (install).
    pub(crate) fn parts(&self) -> (&[OxvFile], &[Vec<u64>], &[Vec<Link>]) {
        (&self.segments, &self.deleted, &self.links)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// segment_set/mod.rs `is_live`.
    pub fn is_live(&self, seg: u16, slot: u32) -> bool {
        self.segments.get(seg as usize).is_some_and(|s| slot < s.slot_count())
            && self.deleted[seg as usize].get(slot as usize / 64).is_none_or(|w| w & (1 << (slot % 64)) == 0)
    }

    /// The set's chunking identity (model.json `chunking_identity`, from set.json).
    pub fn chunking_identity(&self) -> Option<u64> {
        self.chunking
    }

    /// The distinct book names of the set's segments.
    pub fn book_names(&self) -> Vec<String> {
        let mut names: HashSet<&str> = HashSet::new();
        for s in &self.segments {
            names.extend(s.books().iter().map(|b| b.name.as_str()));
        }
        names.into_iter().map(str::to_string).collect()
    }

    /// What the set holds; `held` (if given) tells the books the index holds.
    pub fn info(&self, held: Option<&dyn Fn(&str) -> bool>) -> SetInfo {
        let mut names: HashSet<&str> = HashSet::new();
        for s in &self.segments {
            names.extend(s.books().iter().map(|b| b.name.as_str()));
        }
        let segments: Vec<SegmentInfo> = self
            .segments
            .iter()
            .zip(&self.deleted)
            .zip(&self.links)
            .zip(&self.files)
            .map(|(((s, d), l), f)| {
                let (from, to) = s.library_versions();
                SegmentInfo {
                    file: f.clone(),
                    kind: s.kind().into(),
                    from_library_version: from,
                    to_library_version: to,
                    slots: s.slot_count() as u64,
                    slots_dead: d.iter().map(|w| w.count_ones() as u64).sum(),
                    foreign: s.foreign_count() as u64,
                    foreign_unresolved: l.iter().filter(|x| x.flags & LINK_UNRESOLVED != 0).count() as u64,
                    size: s.size(),
                }
            })
            .collect();
        let slots: u64 = segments.iter().map(|s| s.slots).sum();
        let dead: u64 = segments.iter().map(|s| s.slots_dead).sum();
        let mapped = held.map_or(0, |h| names.iter().filter(|n| h(n)).count() as u64);
        SetInfo {
            path: self.path.to_string_lossy().into_owned(),
            generation: self.generation,
            recovered_from_previous: self.recovered,
            dims: self.dims(),
            codec: self.segments[0].codec().name().into(),
            library_version: self.library_version,
            library_release_tag: self.tag.clone(),
            bytes_on_disk: segments.iter().map(|s| s.size).sum(),
            segments,
            slots_live: slots - dead,
            slots_dead: dead,
            books: names.len() as u64,
            books_mapped: mapped,
            books_unmapped: names.len() as u64 - mapped,
        }
    }

    /// The exact scan (segment_set/mod.rs `scan_with_penalties`): the best
    /// `request.top_k` vectors for `query` (a unit vector of the set's width).
    pub fn scan(&self, query: &[f32], request: &ScanRequest<'_>, penalties: ScanPenalties, cancel: Option<&AtomicBool>) -> Result<Vec<scan::VectorHit>> {
        self.scan_with(query, request, &[], penalties, cancel)
    }

    /// segment_set/mod.rs `scan_with`: [`Self::scan`], and the slots of `also`
    /// besides, each live one holding its key scored as the scan scores; the
    /// best `request.top_k` of those the scan did not return are merged into
    /// its hits (none of its hits gives way to them).
    pub fn scan_with(
        &self,
        query: &[f32],
        request: &ScanRequest<'_>,
        also: &[scan::SlotRef],
        penalties: ScanPenalties,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<scan::VectorHit>> {
        let prepared = PreparedQuery::new(query, self.segments[0].codec(), self.dims())?;
        let views: Vec<ScanSegment<'_>> = self
            .segments
            .iter()
            .zip(&self.deleted)
            .zip(&self.links)
            .map(|((segment, deleted), links)| ScanSegment { segment, deleted, links })
            .collect();
        let set = ScanSet { segments: &views, reverse_links: &self.reverse };
        scan::scan(&set, &prepared, request, also, penalties, cancel)
    }

    /// Semantic search by line: the scan's top `k` vectors (admitted books only,
    /// under a filter) resolved to live lines by `resolver` (Otzaria's
    /// coordinator `candidates_of` over `CandidateResolver::resolve`), one
    /// result per line (its best occurrence), best first, at most `k` lines;
    /// or the host rows the resolver needs (scan again, or keep the
    /// [`ScannedLines`] of [`scan_lines`](Self::scan_lines) to resolve again).
    pub fn search_lines(&self, query: &[f32], resolver: &dyn CandidateResolver, options: &LineSearch, cancel: Option<&AtomicBool>) -> Result<Resolved> {
        self.scan_lines(query, resolver, options, cancel)?.resolve(resolver, options, true, cancel)
    }

    /// The scan of a line search, before resolution.
    pub fn scan_lines(&self, query: &[f32], resolver: &dyn CandidateResolver, options: &LineSearch, cancel: Option<&AtomicBool>) -> Result<ScannedLines> {
        let names = options.books.as_ref().map(|ids| resolver.admissible_books(ids));
        let threads = if options.threads == 0 { scan::default_scan_threads() } else { options.threads };
        let mut result = LineResult { threads, ..Default::default() };
        if names.as_ref().is_some_and(|n| n.is_empty()) || options.k == 0 {
            return Ok(ScannedLines { hits: Vec::new(), books: names, result });
        }
        let request = ScanRequest { top_k: options.k, books: names.as_ref(), threads };
        let penalties = ScanPenalties { window: options.window_penalty, alias: options.alias_penalty };
        result.scanned = self.slots_to_scan(names.as_ref());
        // hybrid/coordinator.rs `candidates`: under a filter, the vectors the
        // resolver names as unreached are weighed beside the scan
        let unreached = match &names {
            Some(books) => resolver.unreached(books, self.generation, cancel)?,
            None => Vec::new(),
        };
        let hits = self.scan_with(query, &request, &unreached, penalties, cancel)?;
        result.vectors = hits.len() as u64;
        result.records = hits
            .iter()
            .flat_map(|h| &h.records)
            .filter(|r| names.as_ref().is_none_or(|n| n.contains(&r.book)))
            .count() as u64;
        Ok(ScannedLines { hits, books: names, result })
    }

    /// The slot count a scan under `books` visits (primary ranges; the few
    /// gathered extra / foreign slots are not counted).
    fn slots_to_scan(&self, books: Option<&BookSet>) -> u64 {
        self.segments
            .iter()
            .map(|s| match books {
                None => s.slot_count() as u64,
                Some(b) => s.books().iter().filter(|x| b.contains(&x.name)).map(|x| x.slots.len() as u64).sum(),
            })
            .sum()
    }
}

/// files.rs `read_pointer`.
fn read_pointer(dir: &Path, name: &str) -> std::result::Result<Option<Pointer>, String> {
    match fs::read(dir.join(name)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("{name} is not a pointer: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{name} could not be read: {e}")),
    }
}

/// files.rs `trailer`: the body once magic and CRC check out.
fn trailer<'a>(bytes: &'a [u8], magic: &[u8; 8], expected_crc: u32) -> Result<&'a [u8]> {
    if bytes.len() < 20 || &bytes[..8] != magic {
        return Err(corrupt("a derived file is not the file its name says"));
    }
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    let crc = u32::from_le_bytes(crc.try_into().unwrap());
    if crc != crc32fast::hash(body) || crc != expected_crc {
        return Err(corrupt("a derived file's CRC does not match"));
    }
    Ok(body)
}

/// files.rs `Deleted::decode`.
fn decode_deleted(bytes: &[u8], expected: &DerivedFile, slots: u64) -> Result<Vec<u64>> {
    let body = trailer(bytes, DEL_MAGIC, expected.crc32)?;
    let declared_slots = u64::from_le_bytes(body[8..16].try_into().unwrap());
    let declared_dead = u64::from_le_bytes(body[16..24].try_into().unwrap());
    if declared_slots != slots || body.len() != 24 + slots.div_ceil(64) as usize * 8 {
        return Err(corrupt(format!("a .del describes {declared_slots} slots, the segment has {slots}")));
    }
    let words: Vec<u64> = body[24..].chunks_exact(8).map(|w| u64::from_le_bytes(w.try_into().unwrap())).collect();
    if slots % 64 != 0 && words.last().is_some_and(|w| w >> (slots % 64) != 0) {
        return Err(corrupt("a .del marks slots past the segment's end"));
    }
    let dead: u64 = words.iter().map(|w| w.count_ones() as u64).sum();
    if dead != declared_dead || dead != expected.count {
        return Err(corrupt("a .del's dead count disagrees with its set"));
    }
    Ok(words)
}

/// files.rs `decode_links`.
fn decode_links(bytes: &[u8], expected: &DerivedFile, foreign: u64) -> Result<Vec<Link>> {
    let body = trailer(bytes, LINKS_MAGIC, expected.crc32)?;
    let count = u64::from_le_bytes(body[8..16].try_into().unwrap());
    if count != foreign || body.len() as u64 != 16 + count * 8 {
        return Err(corrupt(format!("a .links holds {count} links, the segment {foreign} foreign records")));
    }
    let links: Vec<Link> = body[16..]
        .chunks_exact(8)
        .map(|e| Link {
            seg: u16::from_le_bytes([e[0], e[1]]),
            flags: u16::from_le_bytes([e[2], e[3]]),
            slot: u32::from_le_bytes(e[4..8].try_into().unwrap()),
        })
        .collect();
    if links.iter().filter(|l| l.flags & LINK_UNRESOLVED != 0).count() as u64 != expected.count {
        return Err(corrupt("a .links' unresolved count disagrees with its set"));
    }
    Ok(links)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Test writer: `OXVDEL1\n`, slots, dead count, words, CRC (files.rs
/// `Deleted::encode`). Returns (bytes, crc, dead).
/// files.rs `Deleted::encode` of a bitmap: (bytes, crc, dead).
pub(crate) fn encode_deleted_words(slots: u64, words: &[u64]) -> (Vec<u8>, u32, u64) {
    let dead: u64 = words.iter().map(|w| w.count_ones() as u64).sum();
    let mut bytes = DEL_MAGIC.to_vec();
    bytes.extend_from_slice(&slots.to_le_bytes());
    bytes.extend_from_slice(&dead.to_le_bytes());
    for w in words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    (bytes, crc, dead)
}

/// files.rs `encode_links`: (bytes, crc, unresolved).
pub(crate) fn encode_link_list(links: &[Link]) -> (Vec<u8>, u32, u64) {
    let mut bytes = LINKS_MAGIC.to_vec();
    bytes.extend_from_slice(&(links.len() as u64).to_le_bytes());
    for l in links {
        bytes.extend_from_slice(&l.seg.to_le_bytes());
        bytes.extend_from_slice(&l.flags.to_le_bytes());
        bytes.extend_from_slice(&l.slot.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    let unresolved = links.iter().filter(|l| l.flags & LINK_UNRESOLVED != 0).count() as u64;
    (bytes, crc, unresolved)
}

#[doc(hidden)]
pub fn encode_deleted(slots: u64, dead_slots: &[u32]) -> (Vec<u8>, u32, u64) {
    let mut words = vec![0u64; slots.div_ceil(64) as usize];
    for &s in dead_slots {
        words[s as usize / 64] |= 1 << (s % 64);
    }
    let dead: u64 = words.iter().map(|w| w.count_ones() as u64).sum();
    let mut bytes = DEL_MAGIC.to_vec();
    bytes.extend_from_slice(&slots.to_le_bytes());
    bytes.extend_from_slice(&dead.to_le_bytes());
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    (bytes, crc, dead)
}

/// Test writer: `OXVLNK1\n`, count, `{seg, flags, slot}` each, CRC (files.rs
/// `encode_links`). `links`: Some((seg, slot)) or None (unresolved).
#[doc(hidden)]
pub fn encode_links(links: &[Option<(u16, u32)>]) -> (Vec<u8>, u32, u64) {
    let mut bytes = LINKS_MAGIC.to_vec();
    bytes.extend_from_slice(&(links.len() as u64).to_le_bytes());
    let mut unresolved = 0;
    for l in links {
        let (seg, flags, slot) = match l {
            Some((s, x)) => (*s, 0u16, *x),
            None => {
                unresolved += 1;
                (0, LINK_UNRESOLVED, 0)
            }
        };
        bytes.extend_from_slice(&seg.to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&slot.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    (bytes, crc, unresolved)
}

/// Test writer: installs `segments` (already written `.oxv` files, oldest
/// first, with their dead slots and, for a delta, links) as generation
/// `generation` of a set directory in Otzaria's layout, CURRENT pointing at it.
#[doc(hidden)]
pub fn write_test_set(dir: &Path, generation: u64, segments: &[(&Path, Vec<u32>, Option<Vec<Option<(u16, u32)>>>)]) -> Result<()> {
    let gen_dir = format!("gen-{generation:06}");
    fs::create_dir_all(dir.join(&gen_dir))?;
    fs::create_dir_all(dir.join("segments"))?;
    let mut entries = Vec::new();
    let mut last_to = 0;
    for (i, (src, dead, links)) in segments.iter().enumerate() {
        let seg = OxvFile::open(src)?;
        let id = format!("{:032x}", i + 1);
        let file = format!("segments/{id}.oxv");
        fs::copy(src, dir.join(&file))?;
        let (del, del_crc, del_count) = encode_deleted(seg.slot_count() as u64, dead);
        fs::write(dir.join(&gen_dir).join(format!("{id}.del")), del)?;
        let (from, to) = seg.library_versions();
        last_to = to;
        let mut e = serde_json::json!({
            "id": id, "file": file, "kind": seg.kind(), "from": from, "to": to, "sha256": "", "size": seg.size(),
            "slots": seg.slot_count(), "provenance": {}, "del": {"file": format!("{id}.del"), "crc32": del_crc, "count": del_count},
        });
        if let Some(l) = links {
            let (bytes, crc, unresolved) = encode_links(l);
            fs::write(dir.join(&gen_dir).join(format!("{id}.links")), bytes)?;
            e["links"] = serde_json::json!({"file": format!("{id}.links"), "crc32": crc, "count": unresolved});
        }
        entries.push(e);
    }
    let doc = serde_json::json!({
        "format": SET_FORMAT, "format_version": SET_FORMAT_VERSION, "generation": generation, "identity": {},
        "identity_digest": "", "codec_params_sha256": "", "library_version": last_to, "library_release_tag": "test",
        "segments": entries, "stats": {}, "created_at": "",
    });
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| Error::Invalid(e.to_string()))?;
    fs::write(dir.join(&gen_dir).join(SET_FILE), &bytes)?;
    let pointer = serde_json::json!({"generation": generation, "set": format!("{gen_dir}/{SET_FILE}"), "set_sha256": sha256_hex(&bytes)});
    fs::write(dir.join(CURRENT), pointer.to_string())?;
    Ok(())
}
