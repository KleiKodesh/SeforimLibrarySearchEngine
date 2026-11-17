//! What a vector set's directory holds and how much of the host's library it covers
//! (otzaria_search_engine 7.2.0 `semantic_vectors_info` = sidecar `segment_set::info`, and
//! `semantic_coverage` = `LiveResolver::coverage` on recomputed keys). No database here: the
//! host feeds every book's lines; the engine computes each line's key with the set's recipe.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::chunker::{ChunkKey, Chunker, ChunkerConfig, LineRef};
use crate::install::{self, Kind};
use crate::oxv::OxvFile;
use crate::{Error, Result};

/// One segment of [`VectorsDirInfo`] (Otzaria `SemanticSegmentInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirSegmentInfo {
    pub id: String,
    pub kind: String,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    pub slots_dead: u64,
    pub foreign_unresolved: u64,
    pub size: u64,
}

/// Otzaria's `SemanticVectorsInfo`: what the set's small files say, nothing opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VectorsDirInfo {
    /// Whether anything is installed; every other field is empty when not.
    pub present: bool,
    pub identity_digest: String,
    /// The model family bound into the set's identity (`identity.model.family_id`).
    pub model_family_id: String,
    pub library_version: u32,
    pub library_release_tag: String,
    pub generation: u64,
    pub segments: Vec<DirSegmentInfo>,
    pub slots_live: u64,
    pub slots_dead: u64,
    pub bytes_on_disk: u64,
    pub needs_compaction: bool,
    /// CURRENT's generation could not be read and PREVIOUS's was.
    pub recovered_from_previous: bool,
    /// The set's chunking identity (`identity.model.chunking_identity`), 0 when absent.
    #[serde(default)]
    pub chunking_identity: u64,
}

/// segment_set::info: the generation CURRENT names (one condemned by a scrub's verdict
/// passed over for PREVIOUS's), from its set.json alone. `present: false` when nothing is
/// installed; a `corrupt` error when neither pointer names a generation that reads, and an
/// `incompatible` one for a version-1 artifact (a `manifest.json` and no pointers).
pub fn dir_info(dir: &Path) -> Result<VectorsDirInfo> {
    install::dir_info(dir)
}

/// Otzaria's `SemanticCoverage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Coverage {
    /// Lines the recipe embeds (a key exists): the lines a vector could exist for.
    pub live_keyed_lines: u64,
    /// Those whose key the set holds.
    pub covered_lines: u64,
    /// Books with a keyed line.
    pub books_live: u32,
    /// Books with a covered line.
    pub books_covered: u32,
    pub vectors_library_version: u32,
    /// `covered_lines / live_keyed_lines`, 0 when no line is keyed.
    pub ratio: f64,
}

/// `semantic_coverage`, fed book by book: [`CoverageCounter::open`] reads every key the set
/// holds (every slot of every segment, dead or not, and every alias row's own key), then
/// [`CoverageCounter::add_book`] takes each live book's lines in order (a PDF's not at all).
pub struct CoverageCounter {
    keys: Vec<u64>,
    chunker: Chunker,
    coverage: Coverage,
}

impl CoverageCounter {
    pub fn open(dir: &Path, cancel: Option<&AtomicBool>) -> Result<CoverageCounter> {
        let info = install::dir_info(dir)?;
        if !info.present {
            return Err(Error::Format(format!(
                "vector install [{}]: no semantic vectors are installed at {}",
                Kind::Missing.tag(),
                dir.display()
            )));
        }
        let mut keys = Vec::new();
        for segment in install::live_segment_files(dir)? {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(Error::Cancelled);
            }
            let opened = OxvFile::open(&dir.join(&segment))?;
            keys.extend((0..opened.slot_count()).map(|slot| ChunkKey(opened.key(slot)).column_value()));
            keys.extend(opened.alias_anchors().into_iter().map(|key| ChunkKey(key).column_value()));
        }
        keys.sort_unstable();
        keys.dedup();
        let config = ChunkerConfig::by_identity(info.chunking_identity).ok_or_else(|| {
            Error::Format(format!(
                "vector install [{}]: the set's chunking identity {} is not a recipe this build implements",
                Kind::Incompatible.tag(),
                info.chunking_identity
            ))
        })?;
        let chunker = Chunker::new(config).map_err(|e| Error::Invalid(e.0))?;
        Ok(CoverageCounter {
            keys,
            chunker,
            coverage: Coverage { vectors_library_version: info.library_version, ..Default::default() },
        })
    }

    /// One book's lines, in order, each with its section (its key is the recipe's window-0
    /// key, short lines with their section context).
    pub fn add_book(&mut self, lines: &[LineRef<'_>]) {
        let (mut keyed, mut covered) = (false, false);
        for index in 0..lines.len() {
            let Some(text) = self.chunker.embedded_text(lines, index) else { continue };
            let value = ChunkKey::of(&text).column_value();
            self.coverage.live_keyed_lines += 1;
            keyed = true;
            if self.keys.binary_search(&value).is_ok() {
                self.coverage.covered_lines += 1;
                covered = true;
            }
        }
        self.coverage.books_live += u32::from(keyed);
        self.coverage.books_covered += u32::from(covered);
    }

    pub fn result(&self) -> Coverage {
        let mut out = self.coverage.clone();
        out.ratio = if out.live_keyed_lines == 0 { 0.0 } else { out.covered_lines as f64 / out.live_keyed_lines as f64 };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oxv::{self, TestBook, TestCodec, TestSegment, TestSlot};
    use sha2::Digest;
    use std::fs;

    fn sha(bytes: &[u8]) -> String {
        sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A set of one base segment holding `keys`, its set.json given the production recipe's
    /// identity and model family `fam`.
    fn set_with(dir: &Path, keys: &[u64]) {
        let tmp = dir.with_extension("seg");
        let seg = TestSegment {
            dims: 4,
            codec: TestCodec::F32,
            delta: false,
            from: 0,
            to: 5,
            tag: "v5".into(),
            books: vec![TestBook {
                name: "id:1".into(),
                slots: keys.iter().enumerate().map(|(i, &key)| TestSlot { key, hint: i as u32, window: 0, vector: vec![1.0, 0.0, 0.0, 0.0] }).collect(),
                ..Default::default()
            }],
            occurrences: vec![],
            tombstones: vec![],
        };
        oxv::write_segment(&tmp, &seg).unwrap();
        crate::set::write_test_set(dir, 3, &[(&tmp, vec![0], None)]).unwrap();
        let set = dir.join("gen-000003").join("set.json");
        let mut doc: serde_json::Value = serde_json::from_slice(&fs::read(&set).unwrap()).unwrap();
        doc["identity"] = serde_json::json!({"model": {"family_id": "fam", "chunking_identity": ChunkerConfig::production().identity()}});
        doc["identity_digest"] = serde_json::json!("ab");
        doc["stats"] = serde_json::json!({"slots": 3, "slots_dead": 1, "foreign": 0, "foreign_unresolved": 0, "bytes": 0});
        let bytes = serde_json::to_vec_pretty(&doc).unwrap();
        fs::write(&set, &bytes).unwrap();
        let pointer = serde_json::json!({"generation": 3, "set": "gen-000003/set.json", "set_sha256": sha(&bytes)});
        fs::write(dir.join("CURRENT"), pointer.to_string()).unwrap();
    }

    #[test]
    fn dir_info_reads_the_small_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("set");
        assert!(!dir_info(&dir).unwrap().present);
        set_with(&dir, &[1, 2, 3]);
        let info = dir_info(&dir).unwrap();
        assert!(info.present && !info.recovered_from_previous);
        assert_eq!((info.generation, info.library_version, info.library_release_tag.as_str()), (3, 5, "test"));
        assert_eq!((info.model_family_id.as_str(), info.identity_digest.as_str()), ("fam", "ab"));
        assert_eq!((info.segments.len(), info.slots_live, info.slots_dead), (1, 2, 1));
        assert_eq!(info.segments[0].kind, "base");
        assert_eq!(info.chunking_identity, ChunkerConfig::production().identity());
        // dead over 5 % of the slots: the sidecar's default policy compacts it
        assert!(info.needs_compaction);
        // a scrub's verdict condemns the generation, and nothing else is readable
        fs::write(dir.join("segments").join(format!("{}.corrupt", info.segments[0].id)), "{}").unwrap();
        let e = dir_info(&dir).unwrap_err();
        assert_eq!(install::error_kind(&e), Some(Kind::Corrupt));
        // a version-1 artifact is refused
        let v1 = tmp.path().join("v1");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("manifest.json"), "{}").unwrap();
        assert_eq!(install::error_kind(&dir_info(&v1).unwrap_err()), Some(Kind::Incompatible));
    }

    #[test]
    fn coverage_counts_keyed_and_covered_lines_per_book() {
        let tmp = tempfile::tempdir().unwrap();
        let chunker = Chunker::production();
        let long1 = "alpha beta gamma delta epsilon zeta";
        let long2 = "eta theta iota kappa lambda mu nu xi";
        let book = [
            LineRef { text: long1, section: 1 },
            LineRef { text: "abc", section: 1 },
            LineRef { text: "short line", section: 1 },
            LineRef { text: long2, section: 1 },
        ];
        let key = |i: usize| ChunkKey::of(&chunker.embedded_text(&book, i).unwrap()).column_value();
        let dir = tmp.path().join("set");
        assert_eq!(install::error_kind(&CoverageCounter::open(&dir, None).err().unwrap()), Some(Kind::Missing));
        set_with(&dir, &[key(0), key(2), 99]);
        let mut c = CoverageCounter::open(&dir, None).unwrap();
        c.add_book(&book);
        c.add_book(&[LineRef { text: "nothing here is stored anywhere", section: 0 }]);
        c.add_book(&[LineRef { text: "abc", section: 0 }]);
        let r = c.result();
        // "abc" is under min_embeddable_chars: no key; the short line keys with its context
        assert_eq!((r.live_keyed_lines, r.covered_lines, r.books_live, r.books_covered), (4, 2, 2, 1));
        assert_eq!(r.vectors_library_version, 5);
        assert_eq!(r.ratio, 0.5);
        let cancel = AtomicBool::new(true);
        assert!(matches!(CoverageCounter::open(&dir, Some(&cancel)), Err(Error::Cancelled)));
    }

    #[test]
    fn display_window_text_is_the_embedded_window() {
        let chunker = Chunker::production();
        let lines = [
            LineRef { text: "first  line of the section", section: 1 },
            LineRef { text: "short one", section: 1 },
            LineRef { text: "third\tline", section: 1 },
            LineRef { text: "other section", section: 2 },
        ];
        assert_eq!(chunker.display_window_text(&lines, 1, 0).as_deref(), Some("first line of the section short one third line"));
        assert_eq!(chunker.display_window_text(&lines, 1, crate::chunker::ALIAS_WINDOW), None);
        assert_eq!(chunker.display_window_text(&lines[1..2], 0, 0), None);
        assert_eq!(chunker.display_window_text(&lines, 0, 0), None);
        let long: String = (0..200).map(|i| format!("w{i:03}")).collect::<Vec<_>>().join(" ");
        let alone = [LineRef { text: &long, section: 0 }];
        let windows = chunker.embedded_windows(&alone, 0);
        assert!(windows.len() > 2);
        let w1 = &windows[1];
        let piece: String = long.chars().skip(w1.chars.start as usize).take((w1.chars.end - w1.chars.start) as usize).collect();
        assert_eq!(chunker.display_window_text(&alone, 0, w1.ordinal), Some(format!("\u{2026}{}\u{2026}", piece.trim())));
        let last = windows.last().unwrap();
        assert!(chunker.display_window_text(&alone, 0, last.ordinal).unwrap().ends_with("w199"));
        assert_eq!(chunker.display_window_text(&alone, 0, 0), None);
    }
}
