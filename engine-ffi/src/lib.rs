//! C ABI over `seforim-engine`.
//!
//! Conventions (stable for C#, Dart, C):
//! - Objects are opaque pointers: `se_*_open/create` returns one (null on
//!   error), `se_*_close` frees it. Never use one after closing it.
//! - Text crosses as UTF-8 `ptr + len` (no terminator needed).
//! - Status-returning calls return 0 on success, -1 on error.
//! - On any error, `se_last_error()` returns the message for the calling
//!   thread (UTF-8, null-terminated, valid until the next call on that
//!   thread). It never needs freeing.
//! - Strings returned by the engine (JSON) are null-terminated UTF-8 and
//!   must be freed with `se_string_free`.
//! - Text the engine returns that may hold any character (a query's own
//!   text, e.g. `se_reduce_paired_readings`) comes back as owned bytes
//!   `ptr + out_len`, freed with `se_bytes_free`: an embedded NUL survives.
//!   JSON results cannot hold a raw NUL (JSON escapes it), so they stay
//!   null-terminated; an error message's NUL reads as a space.
//! - Ids (`line_id`, `position`, `book_id`, `section_id`) are u64 in the ABI
//!   but their domain is 0..=i64::MAX (SQLite rowids; C# `long`, Dart
//!   `int`): a larger value is rejected with an error at ingest. u64::MAX
//!   is reserved as "none" in outputs (`se_search_copy_next_lines`).
//! - Threading (issue #31; README "Threading and writers"):
//!   - An INDEX (`LineIndex`) is shared by threads: searches, counts,
//!     `se_index_reload`, info and sidecar calls on one index may run
//!     concurrently from any number of threads.
//!   - Every other object (writer, search session) is used by one thread at
//!     a time: one per thread, or a host lock. Different sessions are
//!     independent; each keeps the snapshot it started with.
//!   - One writer per index directory: a second `se_writer_open` /
//!     `se_writer_open_ex` (same index pointer, another pointer opened on the
//!     same directory, or another process) returns null and
//!     `se_last_error()` holds tantivy's lockfile error ("Failed to acquire
//!     Lockfile: LockBusy. ..."); the first writer is not affected.
//!   - Freshness: a commit, rollback or merge through a writer of this index
//!     pointer reloads its reader before returning (the next search sees
//!     it); a write through another pointer or process needs
//!     `se_index_reload` (or a reopen).
//!   - Rollback drops everything queued since the last commit (never seen by
//!     searches); a crash before a commit loses only that queue.

use seforim_engine::semantic::{HybridSession, HybridStep, HybridWindowStep};
mod otzaria_text;
mod semantic_aux;
pub use semantic_aux::*;
pub use otzaria_text::*;

use std::cell::RefCell;
use std::ffi::{c_char, CString};
use std::path::PathBuf;
use std::ptr;

use seforim_engine::{
    CancelToken, HitLines, IndexOptions, LineDoc, LineIndex, LineWriter, SearchOptions, SearchRequest, SearchResult, Snippet, WordWindow,
};

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
    static LAST_CANCELLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn set_error(message: impl std::fmt::Display) {
    let cancelled = message.to_string() == seforim_engine::Error::Cancelled.to_string();
    LAST_CANCELLED.with(|c| c.set(cancelled));
    let text = message.to_string().replace('\0', " ");
    LAST_ERROR.with(|e| *e.borrow_mut() = CString::new(text).unwrap_or_default());
}

/// The calling thread's last error message ("" when none).
#[no_mangle]
pub extern "C" fn se_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// Frees a string returned by the engine.
///
/// # Safety
/// `s` must come from this library and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn se_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// Library version, for diagnostics. Static; do not free.
#[no_mangle]
pub extern "C" fn se_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// The C ABI version. Bump it on EVERY change to the C ABI (a function
/// added, removed or renamed, a signature, a struct layout, a JSON shape or
/// an enum / flag value the hosts read). The C# and Dart packages carry the
/// value they were built for (`NativeAbi.Expected` / `seforimAbiVersion`) and
/// refuse to load a library with another value. `tools/release/check_sync.py`
/// fails CI when the three disagree.
pub const SE_ABI_VERSION: u32 = 32; // 32: lazy semantic resolver like Otzaria (issue #18): index column `chunk_key` (se_writer_book_chunk_keys at build and per-book update), se_vectors_add_rows / se_hybrid_add_rows (`need_rows` of se_semantic_search / se_hybrid_page / se_hybrid_window), se_vectors_map_book / se_vectors_map_book_rows / se_vectors_set_mapping removed, info `books_mapped` = set books the index holds; 31: se_vectors_coverage_add_book_rows (a coverage book from its raw rows + notes, Otzaria semantic_coverage), se_passage_window_texts (a semantic hit's embedded window text, Otzaria embedded_window_texts), se_vectors_book_lines (a book's lines and heading-count sections as Otzaria indexes them), live keys for compaction (se_live_keys_open / _add_book_rows / _free, se_vectors_compact takes `live`: Otzaria's live_library_version re-anchoring), issue #18; 30: search word option `fuzzy_distance` (0..=2, 0 = Otzaria's distance-0 fuzzy word), hybrid `fuzzy_max_distance` 0 accepted (Otzaria's smart search; several words on an index without positions: the adjacency boost from the line texts); 29: hybrid session options `retrieval`, `lexical_mode`, `fuzzy_max_distance`, `match_nikud`, `match_teamim`; page fields `lexical_truncated`, `total_count`, `counts_are_exact`, `requested_mode`, `executed_mode`, `semantic_available`, `fallback_reason`, `fallback_kind`, `candidate_window_truncated`, `stale_dropped`; hit fields `matched_window`, `section_id`; no vector set = Otzaria's lexical fallback; se_hybrid_window (otzaria_search_engine 7.2.0 search_semantic port, issue #18); 28: se_vectors_coverage_add_book requires one section per line, a passage target's missing section means no context window (Otzaria context_window); 27: semantic passage highlights (se_passage_plan, se_passage_mark), vector set directory info and coverage (se_vectors_dir_info, se_vectors_coverage_open / _add_book / _result / _free; vector install error kind `missing`), otzaria_search_engine 7.2.0 port (issue #18); 26: search request fuzzy group field `exact_quotes` (default off: Otzaria's quote-transparent fuzzy rule); 25: se_search_group_terms, index highlight matchers (se_matcher_open kinds index_highlight / index_fuzzy / index_smart, se_text_helper ops fuzzy_index_highlight_tokens / smart_index_highlight_words); 24: Otzaria's text / query helpers (se_text_helper, se_matcher_open / _find / _free, se_book_content_fingerprint, otzaria_search_engine 7.2.0 port); 23: se_vectors_compact (vector set compaction, Otzaria sidecar port, issue #18); 22: se_hybrid_open options `ranking` (Otzaria's RankingProfile) and `foundational_books` replace `fusion` / the penalties (issue #18); 21: Otzaria's hybrid search session (se_hybrid_open, se_hybrid_page, se_hybrid_add_texts, se_hybrid_close; se_search_fuse_semantic removed, issue #18); 20: se_vectors_install report `needs_compaction` (Otzaria sidecar compaction policy, issue #18); 19: user index (se_user_index_create / _open / _handle / _close, se_index_attach_user, se_writer_put_document, se_writer_delete_documents, se_index_documents, se_index_line_sources, se_index_line_texts, se_user_id_base), search option `sources`, info `user_index` (issue #44); 18: se_oxv_verify and se_vectors_install take a cancel token, se_vectors_install the query model's model.json (issue #18); 17: se_oxv_verify, se_vectors_install (vector set install, Otzaria sidecar port, issue #18); 16: se_vectors_map_book_rows (Otzaria's text check + newline-row split, issue #18); 15: semantic search like Otzaria over its own vector set (se_vectors_open, se_vectors_close, se_vectors_map_book, se_vectors_set_mapping; se_vectors_info / se_semantic_search JSON changed; se_vectors_update and se_vectors_import_oxv removed, issue #18); 14: search options `position_ranges`, `decline_over_cap`, `substring`, info `declined` (issue G-K1); 13: Otzaria vector import (se_oxv_expand, se_oxv_info, se_vectors_import_oxv), several vectors (chunks) per line (se_vectors_update `chunks`, hit `chunk`, info `vectors`, option `chunk_penalty`, issue #18); 12: se_index_release_memory (issue #40); 11: semantic search (se_vectors_update, se_vectors_info, se_semantic_search, se_search_fuse_semantic)

/// The C ABI version of this library (see [`SE_ABI_VERSION`]).
#[no_mangle]
pub extern "C" fn se_abi_version() -> u32 {
    SE_ABI_VERSION
}

/// The file path this library was loaded from (UTF-8, null-terminated;
/// static, do not free; "" when the OS cannot tell). Hosts that load the
/// library through a packaging layer (Dart native assets) open it again by
/// this path, and name it in load errors.
#[no_mangle]
pub extern "C" fn se_native_library_path() -> *const c_char {
    static PATH: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    PATH.get_or_init(|| CString::new(module_path::own_path().unwrap_or_default().replace('\0', " ")).unwrap_or_default())
        .as_ptr()
}

mod module_path {
    #[cfg(windows)]
    pub fn own_path() -> Option<String> {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
            fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
        }
        const FROM_ADDRESS: u32 = 0x4;
        const UNCHANGED_REFCOUNT: u32 = 0x2;
        let mut module = std::ptr::null_mut();
        let anchor = super::se_abi_version as *const u16;
        // SAFETY: plain Win32 calls with valid out pointers and a buffer of the size passed.
        unsafe {
            if GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, anchor, &mut module) == 0 {
                return None;
            }
            let mut buf = vec![0u16; 32768];
            let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if n == 0 || n >= buf.len() {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..n]))
        }
    }

    #[cfg(unix)]
    pub fn own_path() -> Option<String> {
        use std::ffi::{c_char, c_void, CStr};
        #[repr(C)]
        struct DlInfo {
            fname: *const c_char,
            fbase: *mut c_void,
            sname: *const c_char,
            saddr: *mut c_void,
        }
        extern "C" {
            fn dladdr(addr: *const c_void, info: *mut DlInfo) -> i32;
        }
        let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
        // SAFETY: dladdr fills `info`; fname points into the loader's static data.
        unsafe {
            if dladdr(super::se_abi_version as *const c_void, &mut info) == 0 || info.fname.is_null() {
                return None;
            }
            Some(CStr::from_ptr(info.fname).to_string_lossy().into_owned())
        }
    }

    #[cfg(not(any(windows, unix)))]
    pub fn own_path() -> Option<String> {
        None
    }
}

/// Otzaria's query sanitizer step `keep_second_of_paired_readings`: every
/// paired reading `(X) [Y]` (ketiv and qere) in `query` becomes ` Y `, as the
/// engine's query parser does. A host that numbers the query's words itself
/// (per-word options, per-pair spacing) runs it first, so its words are the
/// engine's. Free the result (`ptr + out_len`) with `se_bytes_free`; null on error.
///
/// # Safety
/// `query` must be valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_reduce_paired_readings(query: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    guard(ptr::null_mut(), || {
        let q = str_arg(query, len)?;
        if out_len.is_null() {
            return Err("null out_len".into());
        }
        Ok(out_bytes(seforim_engine::reduce_paired_readings(q).into_owned(), out_len))
    })
}

/// Frees bytes returned by the engine with their length (`ptr + out_len`
/// outputs such as `se_reduce_paired_readings`).
///
/// # Safety
/// `data` / `len` must come from this library and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn se_bytes_free(data: *mut u8, len: usize) {
    if !data.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(data, len)));
    }
}

/// Owned bytes for a `ptr + out_len` output. Never null on success: an
/// empty text is a dangling, non-null pointer with length 0 (still freed).
unsafe fn out_bytes(s: String, out_len: *mut usize) -> *mut u8 {
    let bytes = s.into_bytes().into_boxed_slice();
    *out_len = bytes.len();
    Box::into_raw(bytes) as *mut u8
}

/// Ids cross the ABI as u64 but live in 0..=i64::MAX (see the module docs).
fn check_id(name: &str, value: u64) -> Result<u64, String> {
    if value > i64::MAX as u64 {
        Err(format!("{name} {value} is out of range: ids are 0..=9223372036854775807 (i64::MAX)"))
    } else {
        Ok(value)
    }
}

/// Otzaria's `parse_user_regex` kind for `query`, as a JSON string:
/// `"none"` (not `/.../`), `"invalid"` (searched as plain text),
/// `"word"`, `"sequence"`, `"line"` (matched on each line's text: see `se_search_line_status`). A host sets
/// `"user_regex": true` in the search to search `/.../` as a regex. Free
/// with `se_string_free`.
///
/// # Safety
/// `query` must be valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_user_regex_kind(query: *const u8, len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let q = str_arg(query, len)?;
        Ok(out_string(serde_json::to_string(seforim_engine::user_regex_kind(q)).map_err(err)?))
    })
}

unsafe fn str_arg<'a>(ptr: *const u8, len: usize) -> Result<&'a str, String> {
    if ptr.is_null() {
        return if len == 0 { Ok("") } else { Err("null string".into()) };
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len))
        .map_err(|_| "string is not valid UTF-8".to_string())
}

fn out_string(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

/// Runs `f`, catching panics and recording errors. Returns `fallback` on error.
/// Clears the thread's last error first, so a non-empty `se_last_error`
/// after a call always belongs to that call.
fn guard<T>(fallback: T, f: impl FnOnce() -> Result<T, String>) -> T {
    LAST_ERROR.with(|e| *e.borrow_mut() = CString::default());
    LAST_CANCELLED.with(|c| c.set(false));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            set_error(e);
            fallback
        }
        Err(_) => {
            set_error("internal panic in seforim engine");
            fallback
        }
    }
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// A vector install error; a cancellation as the engine's (`se_last_error_cancelled`).
/// An install / verify error for the host: cancelled as every cancelled call, else the
/// message tagged with its kind (`vector install [<kind>]: ...`, seforim-vectors
/// `install::Kind`), which the hosts map onto their own error kinds.
fn install_err(e: seforim_vectors::Error) -> String {
    use seforim_vectors::install::error_kind;
    match error_kind(&e) {
        None => err(seforim_engine::Error::Cancelled),
        Some(kind) => {
            let text = e.to_string();
            if text.contains(&format!("vector install [{}]", kind.tag())) {
                err(e)
            } else {
                err(format!("vector install [{}]: {text}", kind.tag()))
            }
        }
    }
}

// ── Index ───────────────────────────────────────────────────────────────

/// Creates a fresh index in `dir` (wiping it). `options_json`:
/// `{"positions":true,"keep_marks":false}`. Both keys are optional (an
/// absent key, or empty `options_json`, takes the documented default), but
/// the JSON is strict: malformed JSON, a non-object, an unknown key or a
/// non-boolean value fails with an error instead of becoming a default.
///
/// # Safety
/// Pointers must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_index_create(
    dir: *const u8,
    dir_len: usize,
    options_json: *const u8,
    options_len: usize,
) -> *mut LineIndex {
    guard(ptr::null_mut(), || {
        let dir = PathBuf::from(str_arg(dir, dir_len)?);
        let options = se_index_options(str_arg(options_json, options_len)?)?;
        let index = LineIndex::create(&dir, options).map_err(err)?;
        Ok(Box::into_raw(Box::new(index)))
    })
}

/// The index options of `se_index_create` / `se_user_index_create`.
fn se_index_options(json: &str) -> Result<IndexOptions, String> {
        #[derive(serde::Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Opts {
            positions: Option<bool>,
            keep_marks: Option<bool>,
        }
        let o: Opts = if json.trim().is_empty() {
            Opts::default()
        } else {
            let value: serde_json::Value =
                serde_json::from_str(json).map_err(|e| format!("invalid index options: {e}"))?;
            if !value.is_object() {
                return Err("invalid index options: expected a JSON object".into());
            }
            serde_json::from_value(value).map_err(|e| format!("invalid index options: {e}"))?
        };
        let defaults = IndexOptions::default();
        let options = IndexOptions {
            positions: o.positions.unwrap_or(defaults.positions),
            keep_marks: o.keep_marks.unwrap_or(defaults.keep_marks),
        };
        Ok(options)
}

/// Opens an existing index.
///
/// # Safety
/// Pointers must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_index_open(dir: *const u8, dir_len: usize) -> *mut LineIndex {
    guard(ptr::null_mut(), || {
        let dir = PathBuf::from(str_arg(dir, dir_len)?);
        let index = LineIndex::open(&dir).map_err(err)?;
        Ok(Box::into_raw(Box::new(index)))
    })
}

/// 1 when `dir` holds an index, else 0.
///
/// # Safety
/// Pointers must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_index_exists(dir: *const u8, dir_len: usize) -> i32 {
    guard(0, || Ok(LineIndex::exists(&PathBuf::from(str_arg(dir, dir_len)?)) as i32))
}

/// Closes an index.
///
/// # Safety
/// `index` must come from `se_index_create/open` and not be closed twice.
#[no_mangle]
pub unsafe extern "C" fn se_index_close(index: *mut LineIndex) {
    if !index.is_null() {
        drop(Box::from_raw(index));
    }
}

/// JSON: `{"num_lines":N,"positions":bool,"keep_marks":bool,"line_data":bool,"user_index":bool,"payload":string|null}`
/// (`line_data`: the per-line content edges, flags and section id exist, for
/// cross-line matching and the same-section scope).
/// The payload is what the host committed with its last commit.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_info(index: *const LineIndex) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let options = index.options();
        let info = serde_json::json!({
            "num_lines": index.num_lines().map_err(err)?,
            "positions": options.positions,
            "keep_marks": options.keep_marks,
            "line_data": index.has_line_data(),
            "user_index": index.is_user_index(),
            "payload": index.committed_payload().map_err(err)?,
        });
        Ok(out_string(info.to_string()))
    })
}

/// Builds the FM-index sidecar over the term dictionary (FtsLib's
/// `BuildFmSidecar`), which serves leading-`*` wildcards and fuzzy words
/// without walking the whole dictionary. Needs a single segment: call after
/// `se_writer_merge_and_close`. Idempotent. Returns 1 when a sidecar is in
/// place, 0 when the index has several segments or no terms (searches then
/// use the walk), -1 on error, a sidecar that cannot be built included
/// (`se_last_error` holds the reason).
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_build_fm_sidecar(index: *const LineIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(index.build_fm_sidecar().map_err(err)? as i32)
    })
}

/// 1 when a sidecar matching the index's current single segment exists.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_has_fm_sidecar(index: *const LineIndex) -> i32 {
    guard(0, || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(index.has_fm_sidecar() as i32)
    })
}

/// Turns the engine's automatic FM sidecar rebuild on (`on` != 0, the
/// default) or off. 0 on success, -1 on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_set_fm_auto_repair(index: *const LineIndex, on: i32) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        index.set_fm_auto_repair(on != 0);
        Ok(0)
    })
}

/// 1 while the engine's background FM sidecar rebuild runs, 0 otherwise.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_fm_sidecar_rebuilding(index: *const LineIndex) -> i32 {
    guard(0, || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(index.fm_sidecar_rebuilding() as i32)
    })
}

/// Why the index's FM sidecar file cannot be used (the reason behind the
/// search notice `fm_sidecar_unusable`), `rebuilding: <reason>` while the
/// engine's background rebuild runs, `FM sidecar rebuild failed: ...` after it
/// failed, or "" when it is usable or absent with nothing tried.
/// Free with `se_string_free`; null on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_fm_sidecar_problem(index: *const LineIndex) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(out_string(index.fm_sidecar_problem().unwrap_or_default()))
    })
}

/// Number of lines in the index, from the index's shared reader (as of the
/// last commit this engine made or the last `se_index_reload`). No JSON, no
/// payload: a cheap count. -1 on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_num_lines(index: *const LineIndex) -> i64 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(index.num_lines().map_err(err)? as i64)
    })
}

/// Reloads the index's shared reader to the last commit on disk. Needed
/// only when ANOTHER process (or another index handle on the same dir)
/// wrote the index; writes through this handle's writers reload it
/// themselves. Running searches keep their snapshot. 0 = ok, -1 on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_reload(index: *const LineIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        index.reload().map_err(err)?;
        Ok(0)
    })
}

/// Drops the index's rebuildable caches without closing it (issue #40): the
/// FM sidecar mapping, compiled user-regex words, the line-search cache, the
/// vector store mapping and the reader's segment readers, each reopened
/// lazily. Safe while searches run (they keep what they hold); open sessions
/// stay valid. 0 = ok, -1 on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_index_release_memory(index: *const LineIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        index.release_memory().map_err(err)?;
        Ok(0)
    })
}

/// Starts the background full merge after an update (issue #29): on its own
/// thread at background priority, merging every segment into one and
/// rebuilding the FM sidecar; searches go on meanwhile and switch to the
/// merged segment when it is published (atomically). `book_ids` null: a
/// plain merge (the lines keep their positions; nothing to do on one
/// segment). Not null (`count` ids, may be 0): a REPACK in that book order
/// (the re-sort's rules; every position renumbered densely), needed after an
/// update that changed a book's line count or added / removed a book. A
/// recorded unfinished repack is kept when a plain merge is asked. The
/// request is recorded beside the index; `se_index_open` restarts an
/// unfinished one (or merges an index with several segments). Opening a
/// writer stops a running merge first (a repack at once, a plain merge is
/// waited for). `flags` bit 0 = RECORD only, do not start (a host about to
/// write an update that needs a repack records it first, so a process that
/// ends mid-update still repacks at the next open or start; returns 0).
/// Returns 1 when started, 0 when there is nothing to do, -1 on error.
///
/// # Safety
/// `index` must be live; `book_ids` valid for `count` u64 or null.
#[no_mangle]
pub unsafe extern "C" fn se_index_start_background_merge(
    index: *const LineIndex,
    book_ids: *const u64,
    count: usize,
    flags: u32,
) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        let order = if book_ids.is_null() {
            None
        } else if count == 0 {
            Some(Vec::new())
        } else {
            Some(std::slice::from_raw_parts(book_ids, count).to_vec())
        };
        if flags & 1 != 0 {
            index.record_background_merge(order).map_err(err)?;
            return Ok(0);
        }
        Ok(index.start_background_merge(order).map_err(err)? as i32)
    })
}

/// The background merge's state: bit 0 = running, bit 1 = pending (recorded
/// and not finished: running, stopped, failed or interrupted). -1 on error.
///
/// # Safety
/// `index` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_index_background_merge_state(index: *const LineIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(index.background_merge_running() as i32 | (index.background_merge_pending() as i32) << 1)
    })
}

/// Waits for the background merge (blocking; call it off the UI thread).
/// 0 = finished (or none ran), -1 = it failed (`se_last_error`).
///
/// # Safety
/// `index` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_index_wait_background_merge(index: *const LineIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        index.wait_background_merge().map_err(err)?;
        Ok(0)
    })
}

/// Stops the background merge (a repack at its next check; a plain merge
/// already running is waited for). Its record stays, so the next open or
/// start restarts it. 0 = ok, -1 on error.
///
/// # Safety
/// `index` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_index_stop_background_merge(index: *const LineIndex) -> i32 {
    guard(-1, || {
        index.as_ref().ok_or("null index")?.stop_background_merge();
        Ok(0)
    })
}

// ── Writer ──────────────────────────────────────────────────────────────

/// Starts a writer. `heap_bytes` is the total budget (≥15 MB per thread).
///
/// # Safety
/// `index` must be a live index; it must outlive the writer.
#[no_mangle]
pub unsafe extern "C" fn se_writer_open(
    index: *const LineIndex,
    heap_bytes: usize,
    num_threads: usize,
) -> *mut LineWriter {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let writer = index.writer(heap_bytes, num_threads).map_err(err)?;
        Ok(Box::into_raw(Box::new(writer)))
    })
}

/// Flag of [`se_writer_open_ex`]: the indexing threads and the background
/// merges run in the OS background mode (best effort per OS).
pub const SE_WRITER_LOW_PRIORITY: u32 = 1;

/// Flag of [`se_writer_open_ex`]: the auto mode (issue #19). One thread per
/// core in the OS background mode, and a memory budget that follows the
/// machine: large while idle, the minimum under memory pressure or while the
/// user is active (the workers flush their segments to free the buffer).
/// `heap_bytes` and `num_threads` must be 0.
pub const SE_WRITER_AUTO: u32 = 2;

/// [`se_writer_open`] with `flags` (`SE_WRITER_LOW_PRIORITY`,
/// `SE_WRITER_AUTO`; other bits must be 0).
///
/// # Safety
/// `index` must be a live index; it must outlive the writer.
#[no_mangle]
pub unsafe extern "C" fn se_writer_open_ex(
    index: *const LineIndex,
    heap_bytes: usize,
    num_threads: usize,
    flags: u32,
) -> *mut LineWriter {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        if flags & !(SE_WRITER_LOW_PRIORITY | SE_WRITER_AUTO) != 0 {
            return Err(format!("unknown writer flags {flags:#x}"));
        }
        let writer = if flags & SE_WRITER_AUTO != 0 {
            if heap_bytes != 0 || num_threads != 0 {
                return Err("SE_WRITER_AUTO needs heap_bytes = 0 and num_threads = 0".into());
            }
            index.writer_auto().map_err(err)?
        } else {
            let low = flags & SE_WRITER_LOW_PRIORITY != 0;
            index.writer_with(heap_bytes, num_threads, low).map_err(err)?
        };
        Ok(Box::into_raw(Box::new(writer)))
    })
}

/// The writer's current memory budget per indexing thread, in bytes (it
/// moves in the auto mode); 0 on error.
///
/// # Safety
/// `writer` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_writer_memory_budget(writer: *const LineWriter) -> u64 {
    guard(0, || {
        let writer = writer.as_ref().ok_or("null writer")?;
        Ok(writer.memory_budget_per_thread() as u64)
    })
}

/// Puts the CALLING thread into the OS background mode (low CPU, and where
/// the OS supports it disk and memory priority), e.g. the host thread that
/// reads the corpus and feeds the writer. Nesting-safe; pair every call with
/// [`se_thread_background_end`] on the SAME thread. Commits and the FM
/// sidecar build leave the mode for their duration by themselves.
#[no_mangle]
pub extern "C" fn se_thread_background_begin() {
    guard((), || {
        seforim_engine::resources::begin_background();
        Ok(())
    })
}

/// Ends what the matching [`se_thread_background_begin`] began on this thread.
#[no_mangle]
pub extern "C" fn se_thread_background_end() {
    guard((), || {
        seforim_engine::resources::end_background();
        Ok(())
    })
}

/// Queues one line. Not visible until commit.
///
/// # Safety
/// `writer` must be live; `text` valid for `text_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_writer_add(
    writer: *mut LineWriter,
    line_id: u64,
    position: u64,
    book_id: u64,
    text: *const u8,
    text_len: usize,
) -> i32 {
    guard(-1, || {
        let writer = writer.as_mut().ok_or("null writer")?;
        let doc = LineDoc {
            line_id: check_id("line_id", line_id)?,
            position: check_id("position", position)?,
            book_id: check_id("book_id", book_id)?,
            text: str_arg(text, text_len)?.to_string(),
            section_id: None,
            no_cross: false,
        };
        writer.add(&doc).map_err(err)?;
        Ok(0)
    })
}

/// Line flag for `se_writer_add_line`: no phrase continues into or out of
/// the line (a heading).
pub const SE_LINE_NO_CROSS: u32 = 1;
/// Line flag for `se_writer_add_line`: `section_id` is set.
pub const SE_LINE_HAS_SECTION: u32 = 2;

/// `se_writer_add` with the host's per-line data: `flags` =
/// `SE_LINE_NO_CROSS` (1) for a line no phrase may continue into or out of
/// (a heading), `SE_LINE_HAS_SECTION` (2) when `section_id` is the line's
/// section (same-section scope; without it the line is a section of its
/// own). The line's content edges (cross-line) are computed by the engine.
///
/// # Safety
/// As `se_writer_add`.
#[no_mangle]
pub unsafe extern "C" fn se_writer_add_line(
    writer: *mut LineWriter,
    line_id: u64,
    position: u64,
    book_id: u64,
    section_id: u64,
    flags: u32,
    text: *const u8,
    text_len: usize,
) -> i32 {
    guard(-1, || {
        let writer = writer.as_mut().ok_or("null writer")?;
        let doc = LineDoc {
            line_id: check_id("line_id", line_id)?,
            position: check_id("position", position)?,
            book_id: check_id("book_id", book_id)?,
            text: str_arg(text, text_len)?.to_string(),
            section_id: if flags & SE_LINE_HAS_SECTION != 0 {
                Some(check_id("section_id", section_id)?)
            } else {
                None
            },
            no_cross: flags & SE_LINE_NO_CROSS != 0,
        };
        writer.add(&doc).map_err(err)?;
        Ok(0)
    })
}

/// One line of a `se_writer_add_lines` batch: the fields of
/// `se_writer_add_line`, its text being the next `text_len` bytes of the
/// batch's text buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SeLineRecord {
    pub line_id: u64,
    pub position: u64,
    pub book_id: u64,
    pub section_id: u64,
    pub text_len: u64,
    pub flags: u32,
    pub reserved: u32,
}

// The layout hosts declare: 5 u64 + 2 u32 = 48 bytes on every target.
const _: () = assert!(std::mem::size_of::<SeLineRecord>() == 48);

/// Queues `count` lines in one call (`se_writer_add_line` per record); the
/// texts are concatenated in `texts` (UTF-8, `texts_len` bytes in all, each
/// record taking its `text_len` bytes in order). Returns the number of lines
/// queued (= `count`), or -1: the error names the failing record, and the
/// records before it stay queued (roll back to drop them); lengths that do
/// not add up to `texts_len` fail before any record is queued. Little-endian
/// layout on every supported target.
///
/// # Safety
/// `writer` must be live; `records` valid for `count` records and `texts`
/// for `texts_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_writer_add_lines(
    writer: *mut LineWriter,
    records: *const SeLineRecord,
    count: usize,
    texts: *const u8,
    texts_len: usize,
) -> i64 {
    guard(-1, || {
        let writer = writer.as_mut().ok_or("null writer")?;
        if count == 0 {
            return Ok(0);
        }
        if records.is_null() {
            return Err("null records".into());
        }
        let records = std::slice::from_raw_parts(records, count);
        let texts: &[u8] = if texts.is_null() {
            if texts_len != 0 {
                return Err("null texts".into());
            }
            &[]
        } else {
            std::slice::from_raw_parts(texts, texts_len)
        };
        // the lengths must add up to the buffer before anything is queued
        let need = records.iter().try_fold(0u64, |a, r| a.checked_add(r.text_len));
        if need != Some(texts.len() as u64) {
            return Err(format!("texts_len {} but the records take {need:?} bytes", texts.len()));
        }
        let mut at = 0usize;
        for (i, r) in records.iter().enumerate() {
            let fail = |e: String| format!("record {i}: {e}");
            let len = usize::try_from(r.text_len).map_err(|_| fail("text_len out of range".into()))?;
            let end = at
                .checked_add(len)
                .filter(|&e| e <= texts.len())
                .ok_or_else(|| fail("text runs past texts_len".into()))?;
            let text = std::str::from_utf8(&texts[at..end]).map_err(|_| fail("text is not valid UTF-8".into()))?;
            at = end;
            let doc = LineDoc {
                line_id: check_id("line_id", r.line_id).map_err(fail)?,
                position: check_id("position", r.position).map_err(fail)?,
                book_id: check_id("book_id", r.book_id).map_err(fail)?,
                text: text.to_string(),
                section_id: if r.flags & SE_LINE_HAS_SECTION != 0 {
                    Some(check_id("section_id", r.section_id).map_err(fail)?)
                } else {
                    None
                },
                no_cross: r.flags & SE_LINE_NO_CROSS != 0,
            };
            writer.add(&doc).map_err(|e| fail(err(e)))?;
        }
        Ok(count as i64)
    })
}

/// One book's semantic chunk keys (Otzaria's `chunkKey` column, issue #18):
/// its raw rows (`lineIndex, id` order; content as stored, zstd decoded, NULL as
/// empty) at `content[offsets[i] .. offsets[i + 1]]`, `row_line_ids[i]` the line
/// id of row `i`, and its `line_note` rows as `notes_json` (may be empty;
/// `line_index` = the row's `lineIndex`, as `se_vectors_coverage_add_book_rows`).
/// The engine rebuilds the book's lines as Otzaria indexes them and keys each by
/// the production recipe. Call before the book's lines are added (build and
/// per-book update). 0, or -1 on error.
///
/// # Safety
/// `writer` live; `row_line_ids` valid for `row_count`, `offsets` for
/// `row_count + 1`, `content` for `content_len`; `notes_json` null with 0 or
/// valid UTF-8.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn se_writer_book_chunk_keys(
    writer: *mut LineWriter,
    row_line_ids: *const u64,
    row_count: usize,
    content: *const u8,
    content_len: usize,
    offsets: *const u64,
    notes_json: *const u8,
    notes_len: usize,
) -> i32 {
    guard(-1, || {
        let writer = writer.as_mut().ok_or("null writer")?;
        let ids = slice_arg(row_line_ids, row_count, "row_line_ids")?;
        for &l in ids {
            check_id("line_id", l)?;
        }
        let rows = book_rows_arg(content, content_len, offsets, row_count)?;
        let notes = book_notes_arg(notes_json, notes_len)?;
        writer.set_book_chunk_keys(ids, &rows, &notes).map_err(err)?;
        Ok(0)
    })
}

/// Queues deletion of a line id (an update = delete + add).
///
/// # Safety
/// `writer` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_writer_delete(writer: *mut LineWriter, line_id: u64) -> i32 {
    guard(-1, || {
        let line_id = check_id("line_id", line_id)?;
        writer.as_mut().ok_or("null writer")?.delete(line_id);
        Ok(0)
    })
}

/// Queues the deletion of every line of a book (issue #29, a per-book
/// update): delete the book, add its new lines, then ONE commit (issue #32;
/// never a commit in between). Lines added after this call are kept. 0 = ok,
/// -1 on error.
///
/// # Safety
/// `writer` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_writer_delete_book(writer: *mut LineWriter, book_id: u64) -> i32 {
    guard(-1, || {
        let book_id = check_id("book_id", book_id)?;
        writer.as_mut().ok_or("null writer")?.delete_book(book_id).map_err(err)?;
        Ok(0)
    })
}

/// Commits, storing `payload` (may be null) atomically with the documents.
///
/// Book updates (issue #32, host policy): delete the book's lines, add its
/// new lines, then ONE commit; never commit or `se_writer_merge_and_close` in
/// between. A crash before that commit leaves the old book whole; the payload
/// is the host's pending/done record to resume from.
/// 0 means the commit is durable. The index's shared reader reloads right
/// after; if that reload fails the commit still returns 0, searches keep the
/// previous snapshot and the next search or count retries the reload
/// (`se_index_reload` forces it and reports its error).
///
/// # Safety
/// `writer` must be live; `payload` valid for `payload_len` bytes or null.
#[no_mangle]
pub unsafe extern "C" fn se_writer_commit(
    writer: *mut LineWriter,
    payload: *const u8,
    payload_len: usize,
) -> i32 {
    guard(-1, || {
        let writer = writer.as_mut().ok_or("null writer")?;
        let payload = if payload.is_null() {
            None
        } else {
            Some(str_arg(payload, payload_len)?)
        };
        writer.commit(payload).map_err(err)?;
        Ok(0)
    })
}

/// Drops everything queued since the last commit.
///
/// # Safety
/// `writer` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_writer_rollback(writer: *mut LineWriter) -> i32 {
    guard(-1, || {
        writer.as_mut().ok_or("null writer")?.rollback().map_err(err)?;
        Ok(0)
    })
}

/// Commits pending adds and deletes (keeping the last committed payload),
/// merges all segments into one, waits, and CLOSES the writer (it must not
/// be used or closed afterwards).
///
/// # Safety
/// `writer` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_writer_merge_and_close(writer: *mut LineWriter) -> i32 {
    if writer.is_null() {
        set_error("null writer");
        return -1;
    }
    let writer = Box::from_raw(writer);
    guard(-1, || {
        writer.merge_all().map_err(err)?;
        Ok(0)
    })
}

/// Progress of a re-sort (`se_writer_resort_and_close`), read by the host
/// from any thread while the re-sort runs.
#[derive(Default)]
pub struct ResortProgress {
    stage: std::sync::atomic::AtomicU32,
    done: std::sync::atomic::AtomicU64,
    total: std::sync::atomic::AtomicU64,
}

/// A new progress object for `se_writer_resort_and_close`. Free with
/// `se_resort_progress_free` after the re-sort returned.
#[no_mangle]
pub extern "C" fn se_resort_progress_new() -> *mut ResortProgress {
    Box::into_raw(Box::default())
}

/// Reads a re-sort's progress (any thread): `stage` = 0 not started,
/// 1 plan, 2 postings (`done` / `total` terms), 3 store, 4 per-line fields,
/// 5 publish, 6 FM sidecar, 7 done (`done` = lines). Returns 0, or -1 for a
/// null pointer.
///
/// # Safety
/// `progress` must be live; the out pointers valid or null.
#[no_mangle]
pub unsafe extern "C" fn se_resort_progress_get(
    progress: *const ResortProgress,
    stage: *mut u32,
    done: *mut u64,
    total: *mut u64,
) -> i32 {
    use std::sync::atomic::Ordering;
    let Some(p) = progress.as_ref() else { return -1 };
    if !stage.is_null() {
        *stage = p.stage.load(Ordering::Acquire);
    }
    if !done.is_null() {
        *done = p.done.load(Ordering::Relaxed);
    }
    if !total.is_null() {
        *total = p.total.load(Ordering::Relaxed);
    }
    0
}

/// Frees a progress object.
///
/// # Safety
/// `progress` must come from `se_resort_progress_new`, not be freed twice,
/// and no re-sort may still be using it.
#[no_mangle]
pub unsafe extern "C" fn se_resort_progress_free(progress: *mut ResortProgress) {
    if !progress.is_null() {
        drop(Box::from_raw(progress));
    }
}

/// Re-sorts the index into a new catalog order and CLOSES the writer (it
/// must not be used or closed afterwards; optional, a build never needs
/// it). `book_ids` = every indexed book id in the new catalog order
/// (`count` ids; books without lines may appear; indexed books not listed
/// go last in their old order; a book listed twice is an error). Each book keeps its lines in their order
/// and spacing; books are packed from position 0 in the new order. One
/// reordering merge, published atomically (the order stamp changes, so a
/// resume from an old page fails as stale); the FM sidecar is rebuilt.
/// `token` (null = not cancellable) stops it before the publish, leaving
/// the old index (`se_last_error_cancelled() == 1`); `progress` (null =
/// none) is updated as it runs. Runs at background priority when the writer
/// does. Blocking: call it off the UI thread. Returns the number of lines,
/// or -1 on error (`se_last_error`).
///
/// # Safety
/// `writer` must be live; `book_ids` valid for `count` u64 (or null with
/// count 0); `token` and `progress` live or null for the whole call.
#[no_mangle]
pub unsafe extern "C" fn se_writer_resort_and_close(
    writer: *mut LineWriter,
    book_ids: *const u64,
    count: usize,
    token: *const CancelToken,
    progress: *const ResortProgress,
) -> i64 {
    if writer.is_null() {
        set_error("null writer");
        return -1;
    }
    let writer = Box::from_raw(writer);
    guard(-1, || {
        let order: &[u64] = if count == 0 {
            &[]
        } else if book_ids.is_null() {
            return Err("null book_ids".to_string());
        } else {
            std::slice::from_raw_parts(book_ids, count)
        };
        let cancel = token.as_ref().cloned();
        // the host keeps `progress` alive for the call; the callback runs
        // only during it (the merge thread is joined before returning)
        let cell = progress as usize;
        let report: Option<seforim_engine::ResortProgressFn> = (!progress.is_null()).then(|| {
            let f: seforim_engine::ResortProgressFn = std::sync::Arc::new(move |stage, done, total| {
                use std::sync::atomic::Ordering;
                let p = &*(cell as *const ResortProgress);
                p.done.store(done, Ordering::Relaxed);
                p.total.store(total, Ordering::Relaxed);
                p.stage.store(stage as u32, Ordering::Release);
            });
            f
        });
        let lines = writer.resort_and_close(order, cancel, report).map_err(err)?;
        Ok(lines as i64)
    })
}

/// Closes a writer, discarding anything uncommitted.
///
/// # Safety
/// `writer` must come from `se_writer_open` and not be closed twice.
#[no_mangle]
pub unsafe extern "C" fn se_writer_close(writer: *mut LineWriter) {
    if !writer.is_null() {
        drop(Box::from_raw(writer));
    }
}

// ── Search ──────────────────────────────────────────────────────────────

/// Runs a search. `request_json` is the public [`SearchOptions`] object
/// (recognised by its `"query"` key; every other field optional):
/// ```json
/// {"query":"משה תורה|ספר","grammar_prefixes":true,
///  "words":{"1":{"grammar_prefixes":false,"alternatives":["אורייתא"]}},
///  "proximity":{"mode":"any_order","distance":3},"context_words":8}
/// ```
/// or, without `"query"`, the low-level [`SearchRequest`] (slots built by
/// the caller):
/// ```json
/// {"groups":[{"kind":"word","word":"תורה","options":{"grammar_prefixes":true}},
///            {"kind":"wildcard","pattern":"שלו*"}],
///  "max_word_distance":5,"require_ordered":false,"context_words":8}
/// ```
/// Both take `"after_position":N` (resume, FtsLib's `afterPosition`): only
/// lines at catalog positions after N.
/// Both take `"max_expanded_terms":N`: the expansion cap per query word,
/// one cap across all segments, shortest first, the exact word typed always
/// kept (default 2000, FtsLib's; 0 = uncapped).
/// Returns a search object holding every matching line (in the requested order, catalog by default) and
/// the snippet session.
///
/// # Safety
/// `index` must be live; the request valid for its length.
#[no_mangle]
pub unsafe extern "C" fn se_search(
    index: *const LineIndex,
    request_json: *const u8,
    request_len: usize,
) -> *mut SearchResult {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let json: serde_json::Value =
            serde_json::from_str(str_arg(request_json, request_len)?).map_err(err)?;
        let result = if json.get("query").is_some() {
            let options: SearchOptions = serde_json::from_value(json).map_err(err)?;
            index.search_options(&options).map_err(err)?
        } else {
            let request: SearchRequest = serde_json::from_value(json).map_err(err)?;
            index.search(&request).map_err(err)?
        };
        Ok(Box::into_raw(Box::new(result)))
    })
}

/// 1 when the calling thread's last error was a cancellation (the search's
/// cancel token was cancelled), else 0.
#[no_mangle]
pub extern "C" fn se_last_error_cancelled() -> i32 {
    LAST_CANCELLED.with(|c| c.get() as i32)
}

/// A new cancel token (Otzaria's cancellation token): pass it to
/// `se_search_cancellable`; `se_cancel_token_cancel` from any thread stops
/// that search and its later locate / count calls (they fail with
/// `se_last_error_cancelled() == 1`). Free with `se_cancel_token_free`.
#[no_mangle]
pub extern "C" fn se_cancel_token_new() -> *mut CancelToken {
    Box::into_raw(Box::new(CancelToken::new()))
}

/// Cancels `token` (any thread; returns at once). Cannot be undone.
///
/// # Safety
/// `token` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_cancel_token_cancel(token: *const CancelToken) {
    if let Some(t) = token.as_ref() {
        t.cancel();
    }
}

/// Frees a token. A search that holds it keeps its own reference.
///
/// # Safety
/// `token` must come from `se_cancel_token_new` and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn se_cancel_token_free(token: *mut CancelToken) {
    if !token.is_null() {
        drop(Box::from_raw(token));
    }
}

/// `se_search` with a cancel token (null = not cancellable): the search
/// checks it before the walks, per segment and every 4096 collected docs;
/// the returned session's locate / count batches check it per chunk.
///
/// # Safety
/// As `se_search`; `token` null or live.
#[no_mangle]
pub unsafe extern "C" fn se_search_cancellable(
    index: *const LineIndex,
    request_json: *const u8,
    request_len: usize,
    token: *const CancelToken,
) -> *mut SearchResult {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let cancel = token.as_ref().cloned();
        let json: serde_json::Value =
            serde_json::from_str(str_arg(request_json, request_len)?).map_err(err)?;
        let result = if json.get("query").is_some() {
            let options: SearchOptions = serde_json::from_value(json).map_err(err)?;
            index.search_options_cancellable(&options, cancel).map_err(err)?
        } else {
            let request: SearchRequest = serde_json::from_value(json).map_err(err)?;
            index.search_cancellable(&request, cancel).map_err(err)?
        };
        Ok(Box::into_raw(Box::new(result)))
    })
}

/// `se_search_cancellable` that returns as soon as the first hits are
/// decided (issue #21): on an index with positions the rest stream in, in
/// the search's order, while the host reads them; `se_search_stream_pull`
/// takes them in. Every other `se_search_*` call works on the hits taken in
/// so far (`se_search_hit_count`, the copies, locates and counts: the
/// counts' `exact` is true once the stream is complete). Cancelling
/// `token` stops the stream. On an index without positions (or with
/// cross-line matching) the result is complete at once.
///
/// # Safety
/// As `se_search_cancellable`.
#[no_mangle]
pub unsafe extern "C" fn se_search_stream(
    index: *const LineIndex,
    request_json: *const u8,
    request_len: usize,
    token: *const CancelToken,
) -> *mut SearchResult {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let cancel = token.as_ref().cloned();
        let json: serde_json::Value =
            serde_json::from_str(str_arg(request_json, request_len)?).map_err(err)?;
        let result = if json.get("query").is_some() {
            let options: SearchOptions = serde_json::from_value(json).map_err(err)?;
            index.search_options_streaming(&options, cancel).map_err(err)?
        } else {
            let request: SearchRequest = serde_json::from_value(json).map_err(err)?;
            index.search_streaming(&request, cancel).map_err(err)?
        };
        Ok(Box::into_raw(Box::new(result)))
    })
}

/// Takes in the streamed hits decided so far (see `se_search_stream`),
/// waiting until at least `min_hits` hits are in, the search is complete,
/// or `timeout_ms` passes (negative: no limit; 0: only what has arrived).
/// Returns 1 when every hit is in (complete), 0 when more are coming, -1 on
/// error (`se_last_error`; a cancelled search: `se_last_error_cancelled`).
/// A search that does not stream returns 1 at once.
///
/// # Safety
/// `search` must be live and not used by another thread during the call.
#[no_mangle]
pub unsafe extern "C" fn se_search_stream_pull(search: *mut SearchResult, min_hits: u64, timeout_ms: i64) -> i32 {
    guard(-1, || {
        let search = search.as_mut().ok_or("null search")?;
        let timeout = (timeout_ms >= 0).then(|| std::time::Duration::from_millis(timeout_ms as u64));
        let min = usize::try_from(min_hits).unwrap_or(usize::MAX);
        Ok(search.pull(min, timeout).map_err(err)? as i32)
    })
}

/// Number of matching lines (taken in so far, for a streamed search).
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_hit_count(search: *const SearchResult) -> u64 {
    guard(0, || Ok(search.as_ref().ok_or("null search")?.hits.len() as u64))
}

/// Copies hits `[offset, offset+count)` into `out` as 3 u64 each:
/// position, line_id, book_id (the only ids the index keeps; any other
/// placement the host reads from SQLite by line id). Returns how many hits
/// were copied.
///
/// # Safety
/// `out` must hold `count * 3` u64.
#[no_mangle]
pub unsafe extern "C" fn se_search_copy_hits(
    search: *const SearchResult,
    offset: u64,
    count: u64,
    out: *mut u64,
) -> u64 {
    guard(0, || {
        let search = search.as_ref().ok_or("null search")?;
        let start = (offset as usize).min(search.hits.len());
        let end = start.saturating_add(count as usize).min(search.hits.len());
        let out = std::slice::from_raw_parts_mut(out, (end - start) * 3);
        for (i, h) in search.hits[start..end].iter().enumerate() {
            out[i * 3] = h.position;
            out[i * 3 + 1] = h.line_id;
            out[i * 3 + 2] = h.book_id;
        }
        Ok((end - start) as u64)
    })
}

/// Copies the next line ids of hits `[offset, offset+count)` into `out`, one
/// u64 per hit: the next line's id when the hit's match may continue onto
/// it (cross-line matching; locate it with `se_search_locate_pairs`),
/// `u64::MAX` otherwise. Returns how many were copied.
///
/// # Safety
/// `out` must hold `count` u64.
#[no_mangle]
pub unsafe extern "C" fn se_search_copy_next_lines(
    search: *const SearchResult,
    offset: u64,
    count: u64,
    out: *mut u64,
) -> u64 {
    guard(0, || {
        let search = search.as_ref().ok_or("null search")?;
        let start = (offset as usize).min(search.hits.len());
        let end = start.saturating_add(count as usize).min(search.hits.len());
        if end == start || out.is_null() {
            return Ok(0);
        }
        let out = std::slice::from_raw_parts_mut(out, end - start);
        for (o, h) in out.iter_mut().zip(&search.hits[start..end]) {
            *o = h.next_line_id.unwrap_or(u64::MAX);
        }
        Ok((end - start) as u64)
    })
}

/// Copies the word-level descriptors of hits `[offset, offset+count)` into
/// `out`, three u32 per hit: matcher, left, right (the best window decided
/// from the index positions during the search, as the word positions of its
/// first and last matched words; `u32::MAX` x3 = none, a cross-line pair).
/// Kept only when the request asked (`"word_windows": true`) on an index
/// with positions. Returns how many hits were copied (0: none kept).
///
/// # Safety
/// `out` must hold `3 * count` u32.
#[no_mangle]
pub unsafe extern "C" fn se_search_copy_windows(
    search: *const SearchResult,
    offset: u64,
    count: u64,
    out: *mut u32,
) -> u64 {
    guard(0, || {
        let search = search.as_ref().ok_or("null search")?;
        let windows = search.word_windows(offset as usize, count as usize);
        if windows.is_empty() || out.is_null() {
            return Ok(0);
        }
        let out = std::slice::from_raw_parts_mut(out, windows.len() * 3);
        for (o, w) in out.chunks_exact_mut(3).zip(windows) {
            o[0] = w.matcher;
            o[1] = w.left;
            o[2] = w.right;
        }
        Ok(windows.len() as u64)
    })
}

/// Number of u32 a located snippet occupies in the flat form, beyond its marks:
/// score, word_distance, window_word_count, start, end, flags, mark_count.
pub const SNIPPET_HEADER_U32: usize = 7;

fn write_snippet(s: &Snippet, out: &mut Vec<u32>) {
    out.push(s.score.min(u32::MAX as usize) as u32);
    out.push(s.word_distance.min(u32::MAX as usize) as u32);
    out.push(s.window_word_count as u32);
    out.push(s.start as u32);
    out.push(s.end as u32);
    out.push(s.flags);
    out.push((s.marks.len() / 3) as u32);
    out.extend_from_slice(&s.marks);
}

/// A flat u32 buffer the engine owns; read it, then free it with
/// `se_buffer_free`.
#[repr(C)]
pub struct SeBuffer {
    pub data: *mut u32,
    pub len: usize,
    cap: usize,
}

// The layout hosts declare (C# `SeBuffer`, Dart `SeBuffer`): three
// pointer-sized words, on 32-bit targets (ARM32) as on 64-bit.
const _: () = assert!(std::mem::size_of::<SeBuffer>() == 3 * std::mem::size_of::<usize>());
const _: () = assert!(std::mem::align_of::<SeBuffer>() == std::mem::align_of::<usize>());

impl SeBuffer {
    fn from_vec(mut v: Vec<u32>) -> SeBuffer {
        let b = SeBuffer {
            data: v.as_mut_ptr(),
            len: v.len(),
            cap: v.capacity(),
        };
        std::mem::forget(v);
        b
    }
}

/// Frees a buffer returned by the engine.
///
/// # Safety
/// `buffer` must come from this library and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn se_buffer_free(buffer: SeBuffer) {
    if !buffer.data.is_null() {
        drop(Vec::from_raw_parts(buffer.data, buffer.len, buffer.cap));
    }
}

/// `count` texts given as parallel pointer and length arrays. Texts are NOT
/// re-validated: the contract is that the host passes UTF-8 it just encoded
/// (C# Encoding.UTF8, Dart utf8.encode). Re-validating 1.8 GB of text cost
/// 3.8 s on an 800k-hit search. Null pointers are still rejected.
unsafe fn text_args<'a>(
    texts: *const *const u8,
    lens: *const usize,
    count: usize,
) -> std::result::Result<Vec<&'a str>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if texts.is_null() || lens.is_null() {
        return Err("null text array".into());
    }
    let texts = std::slice::from_raw_parts(texts, count);
    let lens = std::slice::from_raw_parts(lens, count);
    let mut strs = Vec::with_capacity(count);
    for i in 0..count {
        if texts[i].is_null() {
            if lens[i] != 0 {
                return Err("null text".into());
            }
            strs.push("");
        } else {
            strs.push(std::str::from_utf8_unchecked(std::slice::from_raw_parts(texts[i], lens[i])));
        }
    }
    Ok(strs)
}

/// Locates the snippets of a batch of matched lines. `texts` is `count`
/// UTF-8 strings given as parallel pointer and length arrays. Output, per
/// text: `[0]` when the line does not pass (filters), else `[1]` followed by
/// the snippet (SNIPPET_HEADER_U32 values, then mark_count × (start, length,
/// term_id)). Offsets are UTF-16 units into each text.
///
/// # Safety
/// Arrays must hold `count` entries; each text valid for its length and
/// VALID UTF-8 (not re-checked, for speed: hosts pass text they just encoded).
#[no_mangle]
pub unsafe extern "C" fn se_search_locate_batch(
    search: *mut SearchResult,
    texts: *const *const u8,
    lens: *const usize,
    count: usize,
) -> SeBuffer {
    let empty = SeBuffer {
        data: ptr::null_mut(),
        len: 0,
        cap: 0,
    };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        let timing = std::env::var_os("SEFORIM_ENGINE_TIMING").is_some();
        let t0 = std::time::Instant::now();
        let strs = text_args(texts, lens, count)?;
        let t1 = std::time::Instant::now();
        let located = search.session.try_locate_many(&strs).map_err(err)?;
        let t2 = std::time::Instant::now();
        let mut out = Vec::with_capacity(count * 16);
        for snippet in located {
            match snippet {
                Some(s) => {
                    out.push(1);
                    write_snippet(&s, &mut out);
                }
                None => out.push(0),
            }
        }
        if timing {
            let bytes: usize = strs.iter().map(|s| s.len()).sum();
            eprintln!(
                "  [ffi] locate_batch {count} texts ({bytes} bytes): utf8 check {:.2} ms, locate {:.2} ms, pack {:.2} ms",
                (t1 - t0).as_secs_f64() * 1000.0,
                (t2 - t1).as_secs_f64() * 1000.0,
                t2.elapsed().as_secs_f64() * 1000.0
            );
        }
        Ok(SeBuffer::from_vec(out))
    })
}

/// Match mode of a search's locate calls (`se_search_locate_batch` /
/// `_lines` / `_pairs`; issues #14 / #23): 0 = best window (the default),
/// 1 = all matches (the whole text, start 0, end its UTF-16 length, a mark
/// for every token of the text that is a query term; which hits pass does
/// not change). Returns 0, or -1 on error.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_set_match_mode(search: *mut SearchResult, mode: u32) -> i32 {
    guard(-1, || {
        let search = search.as_mut().ok_or("null search")?;
        let mode = match mode {
            0 => hebrew_fts::MatchMode::BestWindow,
            1 => hebrew_fts::MatchMode::AllMatches,
            m => return Err(format!("unknown match mode {m}")),
        };
        search.session.set_match_mode(mode);
        Ok(0)
    })
}

/// `se_find_all_matches` flag: only runs of the words in order at
/// consecutive positions (a phrase); else every word anywhere.
pub const SE_FIND_PHRASE: u32 = 1;
/// `se_find_all_matches` flag: quotes (geresh / gershayim) ignored.
pub const SE_FIND_IGNORE_QUOTES: u32 = 2;
/// `se_find_all_matches` flag: nikud / te'amim must match as typed (the
/// default ignores them on both sides).
pub const SE_FIND_MATCH_MARKS: u32 = 4;

thread_local! {
    /// Find-in-text locators, one per thread: [plain, marks kept].
    static FIND_LOCATORS: RefCell<[hebrew_fts::SnippetWindowLocator; 2]> = RefCell::new([
        hebrew_fts::SnippetWindowLocator::new(hebrew_fts::DEFAULT_CONTEXT_WORDS),
        hebrew_fts::SnippetWindowLocator::keeping_marks(hebrew_fts::DEFAULT_CONTEXT_WORDS),
    ]);
}

/// Find in an open text, no session or index (issues #14 / #23): every
/// occurrence of `words` (`word_count` UTF-8 strings as pointer / length
/// arrays; each is normalized by the line scanner, its words in order) in
/// `text` (UTF-8, `text_len` bytes), with the locator's word rules (HTML,
/// entities, marks, quotes). `flags`: `SE_FIND_*`. Output: flat u32
/// triples (start, length, term ordinal = first-seen distinct word),
/// UTF-16 offsets into `text`, document order; an empty buffer when none.
/// Free with `se_buffer_free`.
///
/// # Safety
/// `text` valid for `text_len` bytes; the word arrays hold `word_count`
/// entries. All UTF-8 (not re-checked).
#[no_mangle]
pub unsafe extern "C" fn se_find_all_matches(
    text: *const u8,
    text_len: usize,
    words: *const *const u8,
    word_lens: *const usize,
    word_count: usize,
    flags: u32,
) -> SeBuffer {
    let empty = SeBuffer {
        data: ptr::null_mut(),
        len: 0,
        cap: 0,
    };
    guard(empty, || {
        let text = text_args(&text, &text_len, 1)?[0];
        let words = text_args(words, word_lens, word_count)?;
        let (phrase, quotes, marks) =
            (flags & SE_FIND_PHRASE != 0, flags & SE_FIND_IGNORE_QUOTES != 0, flags & SE_FIND_MATCH_MARKS != 0);
        let prepared = hebrew_fts::PreparedQueryGroups::from_text_words(&words, marks, quotes);
        let out: Vec<u32> = FIND_LOCATORS.with(|l| {
            let mut l = l.borrow_mut();
            let locator = &mut l[usize::from(marks)];
            locator.find_all_matches(text, &prepared, phrase, quotes).iter().map(|&v| v as u32).collect()
        });
        Ok(SeBuffer::from_vec(out))
    })
}

/// Locates a batch of hits given as their LINES (`SnippetSession::try_locate_lines`):
/// hit `h` is `line_counts[h]` consecutive entries of the flat `line_ids` /
/// `texts` / `lens` arrays (the matched line, and for a short window its
/// neighbour lines, in document order), located over the non-empty texts
/// joined by single spaces (offsets into that joined text). `hit_indexes[h]`
/// is the hit's index in the search's hits, or -1 (null array: all -1).
/// On an index with positions the matched words come from the postings
/// (no word of the text is hashed); otherwise, and for a line the index
/// does not hold, the text is searched as by `se_search_locate_batch`. Same
/// output as `se_search_locate_batch`, one entry per hit.
///
/// # Safety
/// `hit_indexes` (if not null) and `line_counts` hold `hit_count` entries;
/// `line_ids`, `texts` and `lens` hold the sum of `line_counts`; texts as
/// for `se_search_locate_batch` (valid UTF-8).
#[no_mangle]
pub unsafe extern "C" fn se_search_locate_lines(
    search: *mut SearchResult,
    hit_indexes: *const i64,
    line_counts: *const usize,
    hit_count: usize,
    line_ids: *const u64,
    texts: *const *const u8,
    lens: *const usize,
) -> SeBuffer {
    let empty = SeBuffer {
        data: ptr::null_mut(),
        len: 0,
        cap: 0,
    };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        if hit_count > 0 && line_counts.is_null() {
            return Err("null line_counts".into());
        }
        let counts: &[usize] = if hit_count == 0 { &[] } else { std::slice::from_raw_parts(line_counts, hit_count) };
        let total: usize = counts.iter().sum();
        if total > 0 && line_ids.is_null() {
            return Err("null line_ids".into());
        }
        let strs = text_args(texts, lens, total)?;
        let ids: &[u64] = if total == 0 { &[] } else { std::slice::from_raw_parts(line_ids, total) };
        let lines: Vec<(u64, &str)> = ids.iter().copied().zip(strs).collect();
        let hit_of = |h: usize| -> Option<usize> {
            if hit_indexes.is_null() {
                return None;
            }
            let v = *hit_indexes.add(h);
            (v >= 0).then_some(v as usize)
        };
        let mut items = Vec::with_capacity(hit_count);
        let mut at = 0;
        for (h, &n) in counts.iter().enumerate() {
            items.push(HitLines { hit: hit_of(h), lines: &lines[at..at + n] });
            at += n;
        }
        let located = search.session.try_locate_lines(&items).map_err(err)?;
        let mut out = Vec::with_capacity(hit_count * 16);
        for snippet in located {
            match snippet {
                Some(s) => {
                    out.push(1);
                    write_snippet(&s, &mut out);
                }
                None => out.push(0),
            }
        }
        Ok(SeBuffer::from_vec(out))
    })
}

/// Hits with their next line's text: `texts` as for `se_search_locate_batch`,
/// `next_texts` / `next_lens` parallel to them, a null pointer for a hit
/// with no next line. Pairs of texts as `(line, next)`.
unsafe fn pair_args<'a>(
    texts: *const *const u8,
    lens: *const usize,
    next_texts: *const *const u8,
    next_lens: *const usize,
    count: usize,
) -> std::result::Result<Vec<(&'a str, Option<&'a str>)>, String> {
    let lines = text_args(texts, lens, count)?;
    if count == 0 || next_texts.is_null() || next_lens.is_null() {
        return Ok(lines.into_iter().map(|t| (t, None)).collect());
    }
    let ptrs = std::slice::from_raw_parts(next_texts, count);
    let nlens = std::slice::from_raw_parts(next_lens, count);
    Ok(lines
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let next = (!ptrs[i].is_null())
                .then(|| std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptrs[i], nlens[i])));
            (t, next)
        })
        .collect())
}

/// `se_search_locate_batch` for hits given with their next line's text
/// (cross-line matching): the line alone is tried first. Output per hit:
/// `[0]` (no pass), `[1, snippet...]` (in-line, offsets into the line) or
/// `[2, break, snippet...]` (cross-line: offsets into `line + "\n" + next`,
/// `break` = the UTF-16 offset of that `\n`).
///
/// # Safety
/// As `se_search_locate_batch`; `next_texts` / `next_lens` hold `count`
/// entries (or are null: no next lines).
#[no_mangle]
pub unsafe extern "C" fn se_search_locate_pairs(
    search: *mut SearchResult,
    texts: *const *const u8,
    lens: *const usize,
    next_texts: *const *const u8,
    next_lens: *const usize,
    count: usize,
) -> SeBuffer {
    let empty = SeBuffer {
        data: ptr::null_mut(),
        len: 0,
        cap: 0,
    };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        let items = pair_args(texts, lens, next_texts, next_lens, count)?;
        let located = search.session.try_locate_pairs(&items).map_err(err)?;
        let mut out = Vec::with_capacity(count * 16);
        for snippet in located {
            match snippet {
                Some(s) => {
                    match s.break_at {
                        Some(at) => {
                            out.push(2);
                            out.push(at as u32);
                        }
                        None => out.push(1),
                    }
                    write_snippet(&s, &mut out);
                }
                None => out.push(0),
            }
        }
        Ok(SeBuffer::from_vec(out))
    })
}

/// Turns word-level descriptors (`se_search_copy_windows`) plus the hits'
/// texts into final snippets without searching for the window again: per
/// hit, `windows` holds its three u32 (matcher, left, right), `texts` its
/// line and `next_texts` its next line (cross-line, or null). A hit whose
/// descriptor is none (`u32::MAX`) is located as `se_search_locate_pairs`
/// does. Output as `se_search_locate_pairs`.
///
/// # Safety
/// As `se_search_locate_pairs`; `windows` holds `3 * count` u32.
#[no_mangle]
pub unsafe extern "C" fn se_search_finish_windows(
    search: *mut SearchResult,
    windows: *const u32,
    texts: *const *const u8,
    lens: *const usize,
    next_texts: *const *const u8,
    next_lens: *const usize,
    count: usize,
) -> SeBuffer {
    let empty = SeBuffer {
        data: ptr::null_mut(),
        len: 0,
        cap: 0,
    };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        if count > 0 && windows.is_null() {
            return Err("null windows".into());
        }
        let items = pair_args(texts, lens, next_texts, next_lens, count)?;
        let w = if count == 0 { &[][..] } else { std::slice::from_raw_parts(windows, count * 3) };
        let items: Vec<(WordWindow, &str, Option<&str>)> = items
            .into_iter()
            .enumerate()
            .map(|(i, (t, n))| (WordWindow { matcher: w[3 * i], left: w[3 * i + 1], right: w[3 * i + 2] }, t, n))
            .collect();
        let located = search.finish_windows(&items).map_err(err)?;
        let mut out = Vec::with_capacity(count * 16);
        for snippet in located {
            match snippet {
                Some(s) => {
                    match s.break_at {
                        Some(at) => {
                            out.push(2);
                            out.push(at as u32);
                        }
                        None => out.push(1),
                    }
                    write_snippet(&s, &mut out);
                }
                None => out.push(0),
            }
        }
        Ok(SeBuffer::from_vec(out))
    })
}

/// `se_search_count_batch` for hits given with their next line's text (as
/// `se_search_locate_pairs` takes them).
///
/// # Safety
/// As `se_search_locate_pairs`.
#[no_mangle]
pub unsafe extern "C" fn se_search_count_batch_pairs(
    search: *mut SearchResult,
    texts: *const *const u8,
    lens: *const usize,
    next_texts: *const *const u8,
    next_lens: *const usize,
    count: usize,
) -> i64 {
    guard(-1, || {
        let search = search.as_mut().ok_or("null search")?;
        let items = pair_args(texts, lens, next_texts, next_lens, count)?;
        Ok(search.count_batch_pairs(&items).map_err(err)? as i64)
    })
}

/// True (1) when the exact count needs the hits' texts (fed in hit
/// order with `se_search_count_batch`), 0 when every hit passes and
/// `se_search_counts` is exact at once; -1 on error.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_count_needs_text(search: *const SearchResult) -> i32 {
    guard(-1, || Ok(search.as_ref().ok_or("null search")?.count_needs_text() as i32))
}

/// Counts the next `count` hits (hit order, continuing from the
/// previous call) given their texts, as `se_search_locate_batch` takes
/// them: the passing lines are added to the exact totals. Returns how many
/// passed, or -1 on error (more texts than hits left).
///
/// # Safety
/// As `se_search_locate_batch`.
#[no_mangle]
pub unsafe extern "C" fn se_search_count_batch(
    search: *mut SearchResult,
    texts: *const *const u8,
    lens: *const usize,
    count: usize,
) -> i64 {
    guard(-1, || {
        let search = search.as_mut().ok_or("null search")?;
        let strs = text_args(texts, lens, count)?;
        Ok(search.count_batch(&strs).map_err(err)? as i64)
    })
}

/// The exact totals so far as JSON:
/// `{"total":N,"books":[[book_id,lines],...],"validated":K,"exact":true}`
/// (books ascending; `exact` once every hit is counted). Free with
/// `se_string_free`.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_counts(search: *const SearchResult) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let search = search.as_ref().ok_or("null search")?;
        Ok(out_string(serde_json::to_string(&search.counts()).map_err(err)?))
    })
}

/// How the search was served, as JSON:
/// `{"notices":["..."],"truncated":false,"declined":false,"order_stamp":"..."}`.
/// `declined`: the index could not answer exactly and gave no hit (options
/// `decline_over_cap` / `substring`, issue G-K1); the host reads the lines itself.
/// `notices` are not errors: `"relevance_needs_positions"` = relevance order
/// was asked for a phrase / proximity / multi-word fuzzy query on an index
/// without positions, so the hits are in catalog order; `"fm_sidecar_unusable"` =
/// the FM sidecar file is corrupt or stale, so leading-`*` and fuzzy words ran
/// as walks (same hits, slower; rebuild it). `truncated`: a walk
/// hit its expansion cap (`max_expanded_terms`, default 2000, one cap
/// across all segments), so terms beyond it were not
/// searched. Free with `se_string_free`.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_info(search: *const SearchResult) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let search = search.as_ref().ok_or("null search")?;
        Ok(out_string(serde_json::to_string(&search.info()).map_err(err)?))
    })
}

// -- User regex, Otzaria's Line kind (see seforim_engine::line_search) --

/// The state of a line-regex search as JSON
/// `{"full_scan":false,"scope_lines":N,"fed":N,"matched":N,"truncated":false,"done":false}`,
/// or `null` for any other search. `full_scan` false: feed the candidates'
/// texts (the search's hits, in order) with `se_search_line_match_batch`;
/// true: stream every line in scope (`scope_lines` of them, catalog order)
/// with `se_search_line_scan_batch`. Free with `se_string_free`.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_line_status(search: *const SearchResult) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let search = search.as_ref().ok_or("null search")?;
        Ok(out_string(serde_json::to_string(&search.line_regex()).map_err(err)?))
    })
}

fn write_ranges(found: Vec<Vec<(u32, u32)>>) -> SeBuffer {
    let mut out = Vec::with_capacity(found.len() * 3);
    for ranges in found {
        out.push(ranges.len() as u32);
        for (s, e) in ranges {
            out.push(s);
            out.push(e);
        }
    }
    SeBuffer::from_vec(out)
}

/// Line regex, candidate path: matches the texts of the next `count`
/// candidates (hit order, continuing from the previous call). Output per
/// text: `n`, then `n` UTF-16 `(start, end)` pairs into that text (`n` = 0:
/// no match, or past the 100,000-line cap).
///
/// # Safety
/// As `se_search_locate_batch`.
#[no_mangle]
pub unsafe extern "C" fn se_search_line_match_batch(
    search: *mut SearchResult,
    texts: *const *const u8,
    lens: *const usize,
    count: usize,
) -> SeBuffer {
    let empty = SeBuffer { data: ptr::null_mut(), len: 0, cap: 0 };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        let strs = text_args(texts, lens, count)?;
        Ok(write_ranges(search.line_match_batch(&strs).map_err(err)?))
    })
}

/// Line regex, full-scan path: matches `count` lines the host streams
/// itself; `lines` holds 3 u64 per line (position, line_id, book_id).
/// Output as `se_search_line_match_batch`.
///
/// # Safety
/// `lines` must hold `count * 3` u64; texts as `se_search_locate_batch`.
#[no_mangle]
pub unsafe extern "C" fn se_search_line_scan_batch(
    search: *mut SearchResult,
    lines: *const u64,
    texts: *const *const u8,
    lens: *const usize,
    count: usize,
) -> SeBuffer {
    let empty = SeBuffer { data: ptr::null_mut(), len: 0, cap: 0 };
    guard(empty, || {
        let search = search.as_mut().ok_or("null search")?;
        if count > 0 && lines.is_null() {
            return Err("null lines".into());
        }
        let raw = if count == 0 { &[][..] } else { std::slice::from_raw_parts(lines, count * 3) };
        let hits: Vec<seforim_engine::PlacedHit> = raw
            .chunks_exact(3)
            .map(|c| seforim_engine::PlacedHit { position: c[0], line_id: c[1], book_id: c[2], next_line_id: None })
            .collect();
        let strs = text_args(texts, lens, count)?;
        Ok(write_ranges(search.line_scan_batch(&hits, &strs).map_err(err)?))
    })
}

/// Copies the matching lines kept so far `[offset, offset+count)` (feed
/// order) into `out` as 3 u64 each (position, line_id, book_id). Returns
/// how many were copied. The number kept is the status's `matched`.
///
/// # Safety
/// `out` must hold `count * 3` u64.
#[no_mangle]
pub unsafe extern "C" fn se_search_copy_line_hits(
    search: *const SearchResult,
    offset: u64,
    count: u64,
    out: *mut u64,
) -> u64 {
    guard(0, || {
        let search = search.as_ref().ok_or("null search")?;
        let hits = search.line_hits();
        let start = (offset as usize).min(hits.len());
        let end = start.saturating_add(count as usize).min(hits.len());
        if end <= start || out.is_null() {
            return Ok(0);
        }
        let out = std::slice::from_raw_parts_mut(out, (end - start) * 3);
        for (i, h) in hits[start..end].iter().enumerate() {
            out[i * 3] = h.position;
            out[i * 3 + 1] = h.line_id;
            out[i * 3 + 2] = h.book_id;
        }
        Ok((end - start) as u64)
    })
}

/// Terms added to the search's term table since the previous call, as a
/// JSON array of strings in id order (ids continue from the previous call).
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_take_new_terms(search: *mut SearchResult) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let search = search.as_mut().ok_or("null search")?;
        let terms = search.session.take_new_terms();
        Ok(out_string(serde_json::to_string(&terms).map_err(err)?))
    })
}

/// Each of the request's word groups' matched index terms (the walks' matched
/// terms), aligned with the search request's `groups` (one per query word): JSON `[[term, ...], ...]`, a group that
/// was not searched or matched nothing empty. Free with `se_string_free`; null on error.
///
/// # Safety
/// `search` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_search_group_terms(search: *const SearchResult) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let search = search.as_ref().ok_or("null search")?;
        Ok(out_string(serde_json::to_string(search.session.group_terms()).map_err(err)?))
    })
}

/// Closes a search.
///
/// # Safety
/// `search` must come from `se_search` and not be closed twice.
#[no_mangle]
pub unsafe extern "C" fn se_search_close(search: *mut SearchResult) {
    if !search.is_null() {
        drop(Box::from_raw(search));
    }
}

// ---------------------------------------------------------------------------
// semantic search (issue #18): an exact scan over Otzaria's own vector set
// (read-only), the host's line mapping, hybrid fusion. The library runs no model.

unsafe fn slice_arg<'a, T>(ptr: *const T, len: usize, name: &str) -> Result<&'a [T], String> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(format!("null {name}"));
    }
    Ok(std::slice::from_raw_parts(ptr, len))
}

/// Opens Otzaria's vector set read-only for the index's semantic search: a
/// set directory as Otzaria installs it (`CURRENT`, `gen-NNNNNN/set.json`,
/// `.del` bitmaps, `segments/*.oxv`; e.g. `%APPDATA%\otzaria\vectors`) or
/// one base `.oxv`. Nothing is converted or written. Replaces a set opened
/// before; the line mapping stays. Returns the set info JSON (see
/// `se_vectors_info`), null on error.
///
/// # Safety
/// `index` live; `path` valid UTF-8 for `path_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_open(index: *const LineIndex, path: *const u8, path_len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let path = str_arg(path, path_len)?;
        let info = index.open_vectors(std::path::Path::new(path)).map_err(err)?;
        Ok(out_string(serde_json::to_string(&info).map_err(err)?))
    })
}

/// Forgets the index's vector set (the line mapping stays). 0, or -1 on error.
///
/// # Safety
/// `index` live.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_close(index: *const LineIndex) -> i32 {
    guard(-1, || {
        index.as_ref().ok_or("null index")?.close_vectors();
        Ok(0)
    })
}

/// Host rows a semantic resolution asked for (`need_rows` of
/// `se_semantic_search`): `line_ids[i]`'s raw content (as stored, zstd decoded,
/// NULL as empty) at `content[offsets[i] .. offsets[i + 1]]`, and the rows'
/// `line_note` rows as `notes_json` (may be empty), `line_index` = the row's
/// index in this call. Kept for the index's current generation; search again.
/// Needs an open set. 0, or -1 on error.
///
/// # Safety
/// `index` live; `line_ids` valid for `count`, `offsets` for `count + 1`,
/// `content` for `content_len`; `notes_json` null with 0 or valid UTF-8.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn se_vectors_add_rows(
    index: *const LineIndex,
    line_ids: *const u64,
    count: usize,
    content: *const u8,
    content_len: usize,
    offsets: *const u64,
    notes_json: *const u8,
    notes_len: usize,
) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        let rows = host_rows_arg(line_ids, count, content, content_len, offsets, notes_json, notes_len)?;
        index.add_semantic_rows(rows).map_err(err)?;
        Ok(0)
    })
}

/// Rows by line id with their own notes (`line_index` = the row's index in
/// the call), as `se_vectors_add_rows` takes them.
///
/// # Safety
/// As `se_vectors_add_rows`.
unsafe fn host_rows_arg(
    line_ids: *const u64,
    count: usize,
    content: *const u8,
    content_len: usize,
    offsets: *const u64,
    notes_json: *const u8,
    notes_len: usize,
) -> Result<Vec<seforim_engine::semantic::HostRow>, String> {
    let ids = slice_arg(line_ids, count, "line_ids")?;
    let raw = book_rows_arg(content, content_len, offsets, count)?;
    let notes = book_notes_arg(notes_json, notes_len)?;
    let mut rows: Vec<seforim_engine::semantic::HostRow> =
        ids.iter().zip(raw).map(|(&line_id, r)| seforim_engine::semantic::HostRow { line_id, raw: r.to_vec(), notes: Vec::new() }).collect();
    for mut n in notes {
        let row = usize::try_from(n.line_index).ok().filter(|&r| r < rows.len()).ok_or_else(|| format!("note line_index {} is not a row of the call", n.line_index))?;
        n.line_index = 0;
        rows[row].notes.push(n);
    }
    Ok(rows)
}

/// A book's raw rows: `content` (`content_len` bytes) cut at `offsets[0..=row_count]`
/// (row `i` = `offsets[i]..offsets[i+1]`).
///
/// # Safety
/// `offsets` valid for `row_count + 1`; `content` for `content_len`.
pub(crate) unsafe fn book_rows_arg<'a>(content: *const u8, content_len: usize, offsets: *const u64, row_count: usize) -> Result<Vec<&'a [u8]>, String> {
    let bytes = slice_arg(content, content_len, "content")?;
    let cuts = slice_arg(offsets, row_count + 1, "offsets")?;
    let mut rows: Vec<&[u8]> = Vec::with_capacity(row_count);
    for w in cuts.windows(2) {
        let (a, b) = (w[0] as usize, w[1] as usize);
        if a > b || b > bytes.len() {
            return Err("offsets out of order or past content".into());
        }
        rows.push(&bytes[a..b]);
    }
    Ok(rows)
}

/// A book's `line_note` rows as JSON `[{"line_index":N,"char_pos":N,"has_marker":bool,
/// "body":".."}]` (empty = none).
///
/// # Safety
/// `notes_json` null with 0 or valid UTF-8 for `notes_len`.
pub(crate) unsafe fn book_notes_arg(notes_json: *const u8, notes_len: usize) -> Result<Vec<seforim_engine::semantic::BookNote>, String> {
    #[derive(serde::Deserialize)]
    struct Note {
        line_index: i64,
        #[serde(default = "minus_one")]
        char_pos: i64,
        #[serde(default)]
        has_marker: bool,
        body: String,
    }
    fn minus_one() -> i64 {
        -1
    }
    let text = str_arg(notes_json, notes_len)?;
    let notes: Vec<Note> = if text.trim().is_empty() { Vec::new() } else { serde_json::from_str(text).map_err(err)? };
    Ok(notes
        .into_iter()
        .map(|n| seforim_engine::semantic::BookNote { line_index: n.line_index, char_pos: n.char_pos, has_marker: n.has_marker, body: n.body })
        .collect())
}

/// The open vector set: JSON `{"path","generation":N,"recovered_from_previous":bool,
/// "dims":N,"codec":"i8-sym-vec"|"rot192-i8i4"|..,"library_version":N,
/// "library_release_tag":"..","segments":[{"file","kind","from_library_version",
/// "to_library_version","slots","slots_dead","foreign","foreign_unresolved","size"}],
/// "slots_live":N,"slots_dead":N,"books":N,"books_mapped":N (books of the set the
/// index holds),"books_unmapped":N,
/// "bytes_on_disk":N}`, or `null` (the JSON literal) when no set is open.
/// Free with `se_string_free`; a null pointer on error.
///
/// # Safety
/// `index` must be a live index.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_info(index: *const LineIndex) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(out_string(serde_json::to_string(&index.vector_info().map_err(err)?).map_err(err)?))
    })
}

/// Semantic search like Otzaria: an exact scan of every live vector of the
/// set against the host's query vector (`query`, `dims` floats from
/// Otzaria's query model; normalized if not unit length) with Otzaria's
/// integer kernels, the best `k` vectors resolved to live lines by the index's
/// `chunk_key` column (Otzaria's resolver; nothing mapped at open).
/// `options_json` (may be empty): `{"k":10,"books":[..]|null,"threads":0,
/// "window_penalty":0.04,"alias_penalty":0.04}`: `books` = the lexical
/// search's filter (scans those books' slot ranges); `threads` 0 = half the
/// cores, at most 8. Returns JSON `{"hits":[{"line_id","book_id","score",
/// "chunk"}],"vectors":N,"records":N,"unmapped":N,"scanned":N,"threads":N,
/// "generation":N}` (plus `"need_rows":[line ids]` with no hits: the host rows a
/// text check needs first, see `se_vectors_add_rows`): one hit per line (its best occurrence: `chunk` 0 its
/// first window, n its n-th, bit 31 found through a neighbour's vector),
/// best first; no set: no hits. Null on error. `token` null or live.
///
/// # Safety
/// `index` live; `query` valid for `dims` floats; `options_json` valid
/// for `options_len` bytes; `token` null or live.
#[no_mangle]
pub unsafe extern "C" fn se_semantic_search(
    index: *const LineIndex,
    query: *const f32,
    dims: usize,
    options_json: *const u8,
    options_len: usize,
    token: *const CancelToken,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let query = slice_arg(query, dims, "query")?;
        let text = str_arg(options_json, options_len)?;
        let options: seforim_engine::semantic::SemanticOptions =
            if text.trim().is_empty() { Default::default() } else { serde_json::from_str(text).map_err(err)? };
        let result = index.semantic_search(query, &options, token.as_ref()).map_err(err)?;
        Ok(out_string(serde_json::to_string(&result).map_err(err)?))
    })
}

/// Opens Otzaria's hybrid search session (issue #18): `query` (the lexical
/// text) and `vector` (`dims` floats: the query's embedding by the host's
/// query model; 0 dims with a set open = a failed semantic side, served as
/// Otzaria degrades; no open vector set = Otzaria's lexical fallback).
/// `options_json` (may be empty): `{"books":[..]|null,"foundational_books":[..]|null,
/// "threads":0,"retrieval":"hybrid"|"semantic_only"|"lexical_only",
/// "lexical_mode":"exact"|"fuzzy","fuzzy_max_distance":1|2,"match_nikud":bool,
/// "match_teamim":bool,"ranking":{..}}` (Otzaria's `search_semantic` options;
/// defaults hybrid, exact, 1, false, false; match_nikud / match_teamim need a
/// keep_marks index): `ranking` = Otzaria's `RankingProfile` (every field
/// optional, defaults = otzaria_search_engine's): `"fusion_strategy":{"strategy":"rrf",
/// "k":60}|{"strategy":"weighted"}|{"strategy":"adaptive"}`, `"alpha_override"`,
/// `"alpha_by_query_type":{"quoted_phrase","exact_reference","short","mixed",
/// "conceptual","unknown"}`, `"bm25_saturation_k"`, `"semantic_threshold"`,
/// `"agreement_bonus"`, `"phrase_match_bonus"`, `"rare_term_bonus"`,
/// `"section_coverage_bonus"`, `"duplicate_penalty"`, `"metadata_ranking_enabled"`,
/// `"candidate_window_multiplier"`, `"foundational_bonus"`,
/// `"foundational_candidate_share"`, `"window_penalty"`, `"alias_penalty"` (out of
/// range: an error, as Otzaria's `validate`). Pages come from
/// `se_hybrid_page`; free with `se_hybrid_close`. Null on error.
///
/// # Safety
/// `index` live; `query` valid UTF-8 for `query_len`; `vector` valid for
/// `dims` floats; `options_json` valid for `options_len`.
#[no_mangle]
pub unsafe extern "C" fn se_hybrid_open(
    index: *const LineIndex,
    query: *const u8,
    query_len: usize,
    vector: *const f32,
    dims: usize,
    options_json: *const u8,
    options_len: usize,
) -> *mut HybridSession {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let query = str_arg(query, query_len)?;
        let vector = slice_arg(vector, dims, "vector")?;
        let text = str_arg(options_json, options_len)?;
        let options: seforim_engine::semantic::HybridOptions =
            if text.trim().is_empty() { Default::default() } else { serde_json::from_str(text).map_err(err)? };
        let session = index.hybrid_session(query, vector, options).map_err(err)?;
        Ok(Box::into_raw(Box::new(session)))
    })
}

/// The results `offset .. offset + limit` of a hybrid session, as Otzaria
/// pages them (its candidate window grows to hold one result past the
/// page: `max(2 * (offset + limit) + 1, 2 * window)`, at most 10,000).
/// Returns JSON `{"hits":[{"line_id","book_id","position":N|null,"fused",
/// "lexical":x|null,"semantic":x|null,"source":"lexical"|"semantic"|"both",
/// "matched_window":N,"section_id":N}],"more":bool,"window":N,"exhausted":bool,
/// "lexical_total":N,"lexical_truncated":bool,"total_count":N,"counts_are_exact":bool,
/// "requested_mode":M,"executed_mode":M,"semantic_available":bool,
/// "fallback_reason":s|null,"fallback_kind":"not_configured"|"query_failed"|null,
/// "candidate_window_truncated":bool,"stale_dropped":N}` (M = a retrieval
/// mode; Otzaria's response fields; no vector set: its lexical fallback,
/// fused 1 / (rank on the page + 1), `window` 0), or (an index
/// without positions, a query of several words: the phrase is scored from
/// the line texts; or a profile whose text bonuses / duplicate penalty read
/// the lines) `{"need_texts":[line ids]}`: give them with
/// `se_hybrid_add_texts` and ask again, or `{"need_rows":[line ids]}` (the semantic
/// resolver checks a window from the row's text): give them with
/// `se_hybrid_add_rows` and ask again. Null on error. `token` null or live.
///
/// # Safety
/// `session` from `se_hybrid_open`; `token` null or live.
#[no_mangle]
pub unsafe extern "C" fn se_hybrid_page(session: *mut HybridSession, offset: u32, limit: u32, token: *const CancelToken) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let session = session.as_mut().ok_or("null session")?;
        let json = match session.page(offset, limit, token.as_ref()).map_err(err)? {
            HybridStep::Page(p) => serde_json::to_string(&p).map_err(err)?,
            HybridStep::NeedTexts(ids) => serde_json::json!({ "need_texts": ids }).to_string(),
            HybridStep::NeedRows(ids) => serde_json::json!({ "need_rows": ids }).to_string(),
        };
        Ok(out_string(json))
    })
}

/// One fused window of a hybrid session (`window` candidates, at most
/// 10,000), apart from its pages: for a host that groups results itself
/// (Otzaria's grouping counts the window in groups). JSON `{"hits":[..as
/// se_hybrid_page..],"window":N,"lexical_done":bool,"semantic_done":bool,
/// "fused_count":N, ..the page's meta fields..}` (stale lines left out,
/// counted in `stale_dropped`, this window's count; use either
/// `se_hybrid_window` or `se_hybrid_page` on one session), or `{"need_texts":[..]}` as `se_hybrid_page`.
/// Null on error.
///
/// # Safety
/// `session` from `se_hybrid_open`; `token` null or live.
#[no_mangle]
pub unsafe extern "C" fn se_hybrid_window(session: *mut HybridSession, window: u32, token: *const CancelToken) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let session = session.as_mut().ok_or("null session")?;
        let json = match session.window(window, token.as_ref()).map_err(err)? {
            HybridWindowStep::Window(w) => serde_json::to_string(&w).map_err(err)?,
            HybridWindowStep::NeedTexts(ids) => serde_json::json!({ "need_texts": ids }).to_string(),
            HybridWindowStep::NeedRows(ids) => serde_json::json!({ "need_rows": ids }).to_string(),
        };
        Ok(out_string(json))
    })
}

/// Gives a hybrid session the texts it asked for (`need_texts`): `count`
/// lines, `line_ids[i]`'s text (UTF-8, as stored: the row's content) at
/// `content[offsets[i] .. offsets[i + 1]]`. 0, or -1 on error.
///
/// # Safety
/// `session` from `se_hybrid_open`; `line_ids` valid for `count`, `offsets`
/// for `count + 1`, `content` for `content_len`.
#[no_mangle]
pub unsafe extern "C" fn se_hybrid_add_texts(
    session: *mut HybridSession,
    line_ids: *const u64,
    count: usize,
    content: *const u8,
    content_len: usize,
    offsets: *const u64,
) -> i32 {
    guard(-1, || {
        let session = session.as_mut().ok_or("null session")?;
        let ids = slice_arg(line_ids, count, "line_ids")?;
        let bytes = slice_arg(content, content_len, "content")?;
        let cuts = slice_arg(offsets, count + 1, "offsets")?;
        let mut texts = Vec::with_capacity(count);
        for (i, w) in cuts.windows(2).enumerate() {
            let (a, b) = (w[0] as usize, w[1] as usize);
            if a > b || b > bytes.len() {
                return Err("offsets out of order or past content".into());
            }
            texts.push((ids[i], String::from_utf8_lossy(&bytes[a..b]).into_owned()));
        }
        session.add_texts(texts);
        Ok(0)
    })
}

/// Gives a hybrid session the rows it asked for (`need_rows`), as
/// `se_vectors_add_rows` takes them. 0, or -1 on error.
///
/// # Safety
/// `session` from `se_hybrid_open`; the rest as `se_vectors_add_rows`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn se_hybrid_add_rows(
    session: *mut HybridSession,
    line_ids: *const u64,
    count: usize,
    content: *const u8,
    content_len: usize,
    offsets: *const u64,
    notes_json: *const u8,
    notes_len: usize,
) -> i32 {
    guard(-1, || {
        let session = session.as_mut().ok_or("null session")?;
        let rows = host_rows_arg(line_ids, count, content, content_len, offsets, notes_json, notes_len)?;
        session.add_rows(rows).map_err(err)?;
        Ok(0)
    })
}

/// Frees a hybrid session (null is a no-op).
///
/// # Safety
/// `session` from `se_hybrid_open`, not used after.
#[no_mangle]
pub unsafe extern "C" fn se_hybrid_close(session: *mut HybridSession) {
    if !session.is_null() {
        drop(Box::from_raw(session));
    }
}

/// Expands Otzaria's downloaded vector file (`.oxv.zst`, zstd) to a plain
/// `.oxv` at `dst` (via `dst.tmp`). Returns the bytes written, -1 on error.
///
/// # Safety
/// Both paths valid UTF-8 for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_oxv_expand(src: *const u8, src_len: usize, dst: *const u8, dst_len: usize) -> i64 {
    guard(-1, || {
        let src = str_arg(src, src_len)?;
        let dst = str_arg(dst, dst_len)?;
        let n = seforim_vectors::oxv::expand_zst(std::path::Path::new(src), std::path::Path::new(dst)).map_err(err)?;
        Ok(n as i64)
    })
}

/// What an Otzaria vector file (`.oxv`, base segment, formats 2 / 3) holds:
/// JSON `{"format":N,"kind":"base","dims":N,"codec":"..","library_release_tag":"..",
/// "from_library_version":N,"to_library_version":N,"slots":N,"extras":N,
/// "occurrences":N,"books":[{"name":"id:123","records":N,"lines":N}]}`;
/// `lines` = highest line ordinal + 1, the line count the host's mapping
/// must cover. Null on error (a delta segment is refused).
///
/// # Safety
/// `path` valid UTF-8 for `path_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_oxv_info(path: *const u8, path_len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let path = str_arg(path, path_len)?;
        let file = seforim_vectors::oxv::OxvFile::open(std::path::Path::new(path)).map_err(err)?;
        Ok(out_string(serde_json::to_string(&file.info()).map_err(err)?))
    })
}

/// Verifies a plain `.oxv` (Otzaria's sidecar: header and directory, every
/// 1 MiB block CRC, and, with `manifest_json` (its release manifest; length 0 =
/// none), the header against the manifest). JSON `{"segment_id","kind",
/// "from_library_version","to_library_version","slots","blocks",
/// "manifest_checked"}`; null on error (the reason in `se_last_error`;
/// `se_last_error_cancelled` when `token`, may be null, was cancelled).
///
/// # Safety
/// The strings valid UTF-8 for their lengths; `token` null or a live cancel token.
#[no_mangle]
pub unsafe extern "C" fn se_oxv_verify(
    path: *const u8,
    path_len: usize,
    manifest: *const u8,
    manifest_len: usize,
    token: *const CancelToken,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let path = str_arg(path, path_len)?;
        let manifest = if manifest_len == 0 { None } else { Some(str_arg(manifest, manifest_len)?) };
        let flag = token.as_ref().map(|t| t.flag());
        let r = seforim_vectors::install::verify_segment_cancellable(std::path::Path::new(path), manifest, flag.as_deref())
            .map_err(install_err)?;
        Ok(out_string(serde_json::to_string(&r).map_err(err)?))
    })
}

/// Installs a verified download into the vector set directory `set_dir`, as
/// Otzaria's sidecar does (`install_package`): a base starts a new generation, a
/// delta applies its tombstones, duplicates and foreign links to the current
/// one; `gen-NNNNNN/set.json`, `.del`, `.links`, `PREVIOUS`, `CURRENT`, garbage
/// collected. `segment`: the `.oxv`, or `.oxv.zst` (expanded into `incoming/` first, as
/// Otzaria's engine does; moved when it lies in
/// `<set_dir>/incoming/`, else copied); `manifest_json`: its release manifest;
/// `published_sha256` (length 0 = none): the manifest's published SHA-256;
/// `model_json` (length 0 = none): the query model's model.json, which the release
/// must be of (Otzaria's `InstallExpectation`); `token` (may be null): cancels the
/// install between blocks, hashed chunks and slot ranges, leaving the set as it was.
/// JSON `{"kind","library_version","generation","segments","slots_added",
/// "tombstones_applied","duplicates_removed","foreign_unresolved",
/// "bytes_on_disk","already_applied"}`; null on error.
///
/// # Safety
/// The strings valid UTF-8 for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_install(
    set_dir: *const u8,
    set_dir_len: usize,
    segment: *const u8,
    segment_len: usize,
    manifest: *const u8,
    manifest_len: usize,
    published_sha256: *const u8,
    published_sha256_len: usize,
    model_json: *const u8,
    model_json_len: usize,
    token: *const CancelToken,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let dir = str_arg(set_dir, set_dir_len)?;
        let segment = str_arg(segment, segment_len)?;
        let manifest = str_arg(manifest, manifest_len)?;
        let published = if published_sha256_len == 0 { None } else { Some(str_arg(published_sha256, published_sha256_len)?) };
        let model = if model_json_len == 0 { None } else { Some(str_arg(model_json, model_json_len)?) };
        let flag = token.as_ref().map(|t| t.flag());
        let opts = seforim_vectors::install::InstallOptions { published_sha256: published, expected_model_json: model, cancel: flag.as_deref() };
        let r = seforim_vectors::install::install_with(std::path::Path::new(dir), std::path::Path::new(segment), manifest, &opts)
            .map_err(install_err)?;
        Ok(out_string(serde_json::to_string(&r).map_err(err)?))
    })
}

/// Compacts the vector set at `set_dir` into one segment when the policy asks, as
/// Otzaria's sidecar does (`segment_set::compact`): the live slots copied byte for byte
/// (every block of every source verified first), records gathered book by book from the
/// newest snapshot of each key, a new generation committed (`PREVIOUS` = the old one),
/// the old segments collected. `policy_json` (length 0 = the sidecar's defaults):
/// `{"max_delta_ratio","max_segments","max_dead_ratio","min_free_space_factor",
/// "refresh_hints","force"}`, any field optional. `live` (may be null): the host's live
/// library (`se_live_keys_open`, Otzaria's `live_library_version`); when it holds the set's
/// library version every record is re-anchored on the live line holding its text and records
/// whose book no longer holds it are pruned; null or another version: records are kept as
/// they are (Otzaria's behaviour without a live library version).
/// `token` (may be null): cancels it, leaving the set as it was. JSON
/// `{"compacted","reason","generation","bytes_before","bytes_after","slots_before",
/// "slots_after","records_pruned","hints_refreshed","elapsed_ms"}`; null on error
/// (tagged as an install error: `busy`, `no_space`, `corrupt`, ...). A session open on
/// the set keeps the old generation until it reopens.
///
/// # Safety
/// The strings valid UTF-8 for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_compact(
    set_dir: *const u8,
    set_dir_len: usize,
    policy_json: *const u8,
    policy_json_len: usize,
    token: *const CancelToken,
    live: *const seforim_vectors::coverage::HostLiveKeys,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let dir = str_arg(set_dir, set_dir_len)?;
        let policy: seforim_vectors::install::compact::CompactionPolicy = if policy_json_len == 0 {
            Default::default()
        } else {
            serde_json::from_str(str_arg(policy_json, policy_json_len)?).map_err(err)?
        };
        let flag = token.as_ref().map(|t| t.flag());
        let r = seforim_vectors::install::compact::compact(std::path::Path::new(dir), &policy, live.as_ref().map(|l| l as &dyn seforim_vectors::install::compact::LiveKeySource), flag.as_deref()).map_err(install_err)?;
        Ok(out_string(serde_json::to_string(&r).map_err(err)?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn user_index_through_c_abi() {
        let base = std::env::temp_dir().join(format!("se_ffi_user_{}", std::process::id()));
        let (l, u) = (base.join("lib").to_string_lossy().into_owned(), base.join("user").to_string_lossy().into_owned());
        let s = |p: *mut c_char| -> String {
            assert!(!p.is_null(), "{:?}", unsafe { CStr::from_ptr(se_last_error()) });
            let v = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
            unsafe { se_string_free(p) };
            v
        };
        unsafe {
            let lib = se_index_create(l.as_ptr(), l.len(), ptr::null(), 0);
            let w = se_writer_open(lib, 30_000_000, 1);
            let text = "alpha beta";
            assert_eq!(se_writer_add(w, 1, 0, 1, text.as_ptr(), text.len()), 0);
            assert_eq!(se_writer_put_document(w, b"{}".as_ptr(), 2), -1); // a library takes no documents
            assert_eq!(se_writer_commit(w, ptr::null(), 0), 0);
            se_writer_close(w);
            let user = se_user_index_create(u.as_ptr(), u.len(), ptr::null(), 0);
            assert!(!user.is_null());
            let uh = se_user_index_handle(user);
            let w = se_writer_open(uh, 30_000_000, 1);
            let doc = r#"{"key":"c:/a.pdf","stamp":"h1","lines":[{"text":"gamma","locator":1},{"text":"alpha","locator":2}]}"#;
            assert_eq!(se_writer_put_document(w, doc.as_ptr(), doc.len()), se_user_id_base() as i64);
            assert_eq!(se_writer_commit(w, ptr::null(), 0), 0);
            se_writer_close(w);
            assert_eq!(se_index_attach_user(lib, user), 0);
            se_user_index_close(user); // the library keeps it
            let docs = s(se_index_documents(lib));
            assert!(docs.contains("\"key\":\"c:/a.pdf\"") && docs.contains("\"stamp\":\"h1\""), "{docs}");
            for (sources, want) in [("all", vec![1, se_user_id_base() + 1]), ("user", vec![se_user_id_base() + 1]), ("library", vec![1])] {
                let q = format!(r#"{{"query":"alpha","sources":"{sources}"}}"#);
                let r = se_search(lib, q.as_ptr(), q.len());
                assert!(!r.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
                let n = se_search_hit_count(r);
                let mut out = vec![0u64; n as usize * 3];
                se_search_copy_hits(r, 0, n, out.as_mut_ptr());
                let ids: Vec<u64> = out.chunks(3).map(|c| c[1]).collect();
                assert_eq!(ids, want, "{sources}");
                se_search_close(r);
            }
            let ids = [se_user_id_base() + 1, 1];
            let src = s(se_index_line_sources(lib, ids.as_ptr(), 2));
            assert_eq!(src, r#"[{"key":"c:/a.pdf","locator":2,"line":1},null]"#);
            assert_eq!(s(se_index_line_texts(lib, ids.as_ptr(), 2)), r#"["alpha",null]"#);
            se_index_close(lib);
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn roundtrip_through_c_abi() {
        let dir = std::env::temp_dir().join(format!("se_ffi_{}", std::process::id()));
        let d = dir.to_string_lossy().into_owned();
        unsafe {
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
            let writer = se_writer_open(index, 30_000_000, 1);
            let text = "והתורה ניתנה למשה";
            assert_eq!(se_writer_add(writer, 7, 0, 1, text.as_ptr(), text.len()), 0);
            let payload = "done=1";
            assert_eq!(se_writer_commit(writer, payload.as_ptr(), payload.len()), 0);
            se_writer_close(writer);

            let info = se_index_info(index);
            let info_s = CStr::from_ptr(info).to_str().unwrap().to_string();
            se_string_free(info);
            assert!(info_s.contains("\"payload\":\"done=1\""), "{info_s}");

            let req = r#"{"groups":[{"kind":"word","word":"תורה","options":{"grammar_prefixes":true}}]}"#;
            let search = se_search(index, req.as_ptr(), req.len());
            assert!(!search.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
            assert_eq!(se_search_hit_count(search), 1);
            let mut hits = [0u64; 3];
            assert_eq!(se_search_copy_hits(search, 0, 1, hits.as_mut_ptr()), 1);
            assert_eq!(hits[1], 7);

            let ptrs = [text.as_ptr()];
            let lens = [text.len()];
            let buf = se_search_locate_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1);
            let data = std::slice::from_raw_parts(buf.data, buf.len).to_vec();
            se_buffer_free(buf);
            assert_eq!(data[0], 1); // passed
            assert_eq!(data[7], 1); // one mark
            assert_eq!(&data[8..10], &[0, 6]); // והתורה at 0, length 6

            let terms = se_search_take_new_terms(search);
            assert_eq!(CStr::from_ptr(terms).to_str().unwrap(), r#"["והתורה"]"#);
            se_string_free(terms);

            // exact counts: no text-level filter, so exact without texts
            assert_eq!(se_search_count_needs_text(search), 0);
            let c = se_search_counts(search);
            assert_eq!(
                CStr::from_ptr(c).to_str().unwrap(),
                r#"{"total":1,"books":[[1,1]],"validated":1,"exact":true}"#
            );
            se_string_free(c);
            assert_eq!(se_search_count_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1), 1);
            assert_eq!(se_search_count_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1), -1);
            assert_eq!(se_last_error_cancelled(), 0);
            se_search_close(search);

            // resume: the one line is at position 0, so after it nothing is left
            let resumed = req.replacen('{', r#"{"after_position":0,"#, 1);
            let search = se_search(index, resumed.as_ptr(), resumed.len());
            assert!(!search.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
            assert_eq!(se_search_hit_count(search), 0);
            se_search_close(search);

            // cancellation: a cancelled token stops the session's locates and
            // counts, and a search started with it fails at once
            let token = se_cancel_token_new();
            let search = se_search_cancellable(index, req.as_ptr(), req.len(), token);
            assert!(!search.is_null());
            se_cancel_token_cancel(token);
            let buf = se_search_locate_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1);
            assert!(buf.data.is_null());
            assert_eq!(se_last_error_cancelled(), 1);
            assert_eq!(se_search_count_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1), -1);
            assert_eq!(se_last_error_cancelled(), 1);
            se_search_close(search);
            assert!(se_search_cancellable(index, req.as_ptr(), req.len(), token).is_null());
            assert_eq!(se_last_error_cancelled(), 1);
            se_cancel_token_free(token);

            // the public SearchOptions object: per-word override wins
            for (json, hits) in [
                (r#"{"query":"תורה","grammar_prefixes":true}"#, 1),
                (r#"{"query":"תורה","grammar_prefixes":true,"words":{"0":{"grammar_prefixes":false}}}"#, 0),
                (r#"{"query":"ספר","words":{"0":{"alternatives":["%תורה"]}}}"#, 1),
                // a negative word: the global options are its default, its own override wins
                (r#"{"query":"ניתנה","exclude":{"query":"תורה"}}"#, 1),
                (r#"{"query":"ניתנה","grammar_prefixes":true,"exclude":{"query":"תורה"}}"#, 0),
                (r#"{"query":"ניתנה","exclude":{"query":"תורה","words":{"0":{"grammar_prefixes":true}}}}"#, 0),
                (r#"{"query":"ניתנה","grammar_prefixes":true,"exclude":{"query":"תורה","words":{"0":{"grammar_prefixes":false}}}}"#, 1),
                (r#"{"query":"ניתנה","exclude":{"query":"ספר","words":{"0":{"alternatives":["למשה"]}}}}"#, 0),
                (r#"{"query":"ניתנה","exclude":{"query":"","phrase_alternatives":[{"query":"ניתנה למשה"}]}}"#, 0),
            ] {
                let search = se_search(index, json.as_ptr(), json.len());
                assert!(!search.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
                assert_eq!(se_search_hit_count(search), hits, "{json}");
                se_search_close(search);
            }
            // index options are strict: an unknown key or a wrong type is an error
            let other = format!("{d}_opts");
            for bad in [r#"{"position":true}"#, r#"{"positions":1}"#, "[]", "null", "{"] {
                let i = se_index_create(other.as_ptr(), other.len(), bad.as_ptr(), bad.len());
                assert!(i.is_null(), "{bad}");
                let e = CStr::from_ptr(se_last_error()).to_str().unwrap().to_string();
                assert!(e.starts_with("invalid index options"), "{bad}: {e}");
            }
            let _ = std::fs::remove_dir_all(&other);

            // proximity gaps: u32::MAX is unbounded (no overflow, same hits as
            // MAX - 1); a value outside u32 is rejected with an error
            let gap = |g: &str| format!(r#"{{"query":"והתורה למשה","proximity":{{"mode":"ordered","gaps":[{g}]}}}}"#);
            let hits_of = |json: String| {
                let search = se_search(index, json.as_ptr(), json.len());
                assert!(!search.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
                let n = se_search_hit_count(search);
                se_search_close(search);
                n
            };
            assert_eq!(hits_of(gap("4294967295")), hits_of(gap("4294967294")));
            assert_eq!(hits_of(gap("4294967295")), 1);
            for bad in ["4294967296", "-1", "1.5"] {
                let json = gap(bad);
                assert!(se_search(index, json.as_ptr(), json.len()).is_null(), "{bad}");
                let e = CStr::from_ptr(se_last_error()).to_str().unwrap().to_string();
                assert!(e.contains("u32"), "{bad}: {e}");
            }
            se_index_close(index);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn abi_edges() {
        unsafe {
            // a helper's text output keeps an embedded NUL (ptr + length)
            let q = "a b";
            let mut n = usize::MAX;
            let out = se_reduce_paired_readings(q.as_ptr(), q.len(), &mut n);
            assert!(!out.is_null());
            assert_eq!(std::slice::from_raw_parts(out, n), b"a b");
            se_bytes_free(out, n);
            let out = se_reduce_paired_readings(ptr::null(), 0, &mut n);
            assert!(!out.is_null());
            assert_eq!(n, 0);
            se_bytes_free(out, n);
            assert!(se_reduce_paired_readings(q.as_ptr(), q.len(), ptr::null_mut()).is_null());

            // ids above i64::MAX are rejected at ingest; i64::MAX itself is fine
            let dir = std::env::temp_dir().join(format!("se_ffi_edges_{}", std::process::id()));
            let d = dir.to_string_lossy().into_owned();
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null());
            let writer = se_writer_open(index, 30_000_000, 1);
            let text = "abc";
            let big = i64::MAX as u64 + 1;
            for (id, pos, book) in [(big, 0, 0), (0, big, 0), (0, 0, big), (u64::MAX, 0, 0)] {
                assert_eq!(se_writer_add(writer, id, pos, book, text.as_ptr(), text.len()), -1);
                let e = CStr::from_ptr(se_last_error()).to_str().unwrap().to_string();
                assert!(e.contains("out of range"), "{e}");
            }
            assert_eq!(se_writer_add_line(writer, 1, 0, 0, big, SE_LINE_HAS_SECTION, text.as_ptr(), text.len()), -1);
            // without the flag the section id is not read
            assert_eq!(se_writer_add_line(writer, 1, 0, 0, big, 0, text.as_ptr(), text.len()), 0);
            assert_eq!(se_writer_delete(writer, big), -1);
            let max = i64::MAX as u64;
            assert_eq!(se_writer_add(writer, max, max, max, text.as_ptr(), text.len()), 0);
            // a batch: texts concatenated, each record takes its text_len bytes
            let rec = |id: u64, len: u64, flags: u32| SeLineRecord {
                line_id: id,
                position: id,
                book_id: 1,
                section_id: 9,
                text_len: len,
                flags,
                reserved: 0,
            };
            let texts = "abcde";
            let recs = [rec(10, 2, SE_LINE_HAS_SECTION), rec(11, 0, SE_LINE_NO_CROSS), rec(12, 3, 0)];
            assert_eq!(se_writer_add_lines(writer, recs.as_ptr(), 3, texts.as_ptr(), texts.len()), 3);
            assert_eq!(se_writer_add_lines(writer, ptr::null(), 0, ptr::null(), 0), 0);
            // a text past the buffer, a leftover byte, an id out of range: -1 naming the record
            let over = [rec(13, 2, 0), rec(14, 9, 0)];
            assert_eq!(se_writer_add_lines(writer, over.as_ptr(), 2, texts.as_ptr(), texts.len()), -1);
            assert!(se_last_error_message().contains("take Some(11) bytes"), "{}", se_last_error_message());
            assert_eq!(se_writer_add_lines(writer, over.as_ptr(), 1, texts.as_ptr(), texts.len()), -1);
            assert!(se_last_error_message().contains("take Some(2) bytes"), "{}", se_last_error_message());
            let bad = [rec(big, 1, 0)];
            assert_eq!(se_writer_add_lines(writer, bad.as_ptr(), 1, texts.as_ptr(), 1), -1);
            assert!(se_last_error_message().contains("record 0: line_id"), "{}", se_last_error_message());
            // a good call clears the error
            assert_eq!(se_writer_add_lines(writer, ptr::null(), 0, ptr::null(), 0), 0);
            assert_eq!(se_last_error_message(), "");
            se_writer_close(writer);
            se_index_close(index);
            let _ = std::fs::remove_dir_all(dir);

            // the last error is per thread: an error on another thread is not seen here
            let null_str = se_reduce_paired_readings(ptr::null(), 1, &mut n);
            assert!(null_str.is_null());
            let here = se_last_error_message();
            assert!(!here.is_empty());
            let other = std::thread::spawn(|| {
                let before = se_last_error_message();
                let mut n = 0usize;
                se_reduce_paired_readings(ptr::null(), 0, &mut n);
                se_bytes_free(ptr::null_mut(), 0);
                (before, se_last_error_message())
            })
            .join()
            .unwrap();
            assert_eq!(other, (String::new(), String::new()));
            assert_eq!(se_last_error_message(), here);
        }
    }

    /// Issues #14 / #23: a session's all-matches mode and find in an open text.
    #[test]
    fn all_matches_through_c_abi() {
        let a = "\u{05d0}\u{05d1}";
        let text = format!("{a} \u{05d2}\u{05d3} <b>{a}</b> x {a}");
        let dir = std::env::temp_dir().join(format!("se_ffi_all_{}", std::process::id()));
        let d = dir.to_string_lossy().into_owned();
        unsafe {
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
            let writer = se_writer_open(index, 30_000_000, 1);
            assert_eq!(se_writer_add(writer, 1, 0, 1, text.as_ptr(), text.len()), 0);
            assert_eq!(se_writer_commit(writer, ptr::null(), 0), 0);
            se_writer_close(writer);
            let req = format!(r#"{{"groups":[{{"kind":"word","word":"{a}"}}],"context_words":0}}"#);
            let search = se_search(index, req.as_ptr(), req.len());
            assert!(!search.is_null(), "{:?}", CStr::from_ptr(se_last_error()));
            let (ptrs, lens) = ([text.as_ptr()], [text.len()]);
            let locate = |search| {
                let buf = se_search_locate_batch(search, ptrs.as_ptr(), lens.as_ptr(), 1);
                let v = std::slice::from_raw_parts(buf.data, buf.len).to_vec();
                se_buffer_free(buf);
                v
            };
            let best = locate(search);
            assert_eq!(best[7], 1); // one mark in a 0-context window
            assert_eq!(se_search_set_match_mode(search, 1), 0);
            let all = locate(search);
            let len16 = text.encode_utf16().count() as u32;
            assert_eq!((all[4], all[5]), (0, len16)); // the whole text
            assert_eq!(all[7], 3);
            let starts: Vec<u32> = all[8..].chunks(3).map(|m| m[0]).collect();
            assert_eq!(starts, vec![0, 9, 18]);
            assert_eq!(se_search_set_match_mode(search, 0), 0);
            assert_eq!(locate(search), best);
            assert_eq!(se_search_set_match_mode(search, 7), -1);
            se_search_close(search);
            se_index_close(index);

            // no session
            let words = [a.as_ptr()];
            let wl = [a.len()];
            let buf = se_find_all_matches(text.as_ptr(), text.len(), words.as_ptr(), wl.as_ptr(), 1, 0);
            let v = std::slice::from_raw_parts(buf.data, buf.len).to_vec();
            se_buffer_free(buf);
            // a mark spans to the next visible char (closing tag included), as every locate
            assert_eq!(v, vec![0, 2, 0, 9, 6, 0, 18, 2, 0]);
            let buf = se_find_all_matches(text.as_ptr(), text.len(), words.as_ptr(), wl.as_ptr(), 0, 0);
            assert_eq!(buf.len, 0);
            se_buffer_free(buf);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Issue #31: a second writer on the same index (same handle, or another
    /// handle on the same directory) returns null with tantivy's lockfile
    /// error in `se_last_error()`; the first writer is untouched.
    #[test]
    fn second_writer_returns_lock_error() {
        let dir = std::env::temp_dir().join(format!("se_ffi_lock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_string_lossy().into_owned();
        unsafe {
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null(), "{}", se_last_error_message());
            let other = se_index_open(d.as_ptr(), d.len());
            assert!(!other.is_null(), "{}", se_last_error_message());
            let first = se_writer_open(index, 30_000_000, 1);
            assert!(!first.is_null(), "{}", se_last_error_message());
            for handle in [index as *const LineIndex, other as *const LineIndex] {
                assert!(se_writer_open(handle, 30_000_000, 1).is_null());
                let message = se_last_error_message();
                assert!(message.contains("LockBusy"), "{message}");
            }
            let text = "alpha";
            assert_eq!(se_writer_add(first, 1, 0, 1, text.as_ptr(), text.len()), 0);
            assert_eq!(se_writer_commit(first, ptr::null(), 0), 0);
            se_writer_close(first);
            let again = se_writer_open(other, 30_000_000, 1);
            assert!(!again.is_null(), "{}", se_last_error_message());
            se_writer_close(again);
            se_index_close(other);
            se_index_close(index);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn se_last_error_message() -> String {
        unsafe { CStr::from_ptr(se_last_error()).to_str().unwrap().to_string() }
    }

    #[test]
    fn semantic_search_through_the_c_abi() {
        unsafe {
            let dir = std::env::temp_dir().join(format!("se_ffi_semantic_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let d = dir.join("index").to_string_lossy().into_owned();
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null());
            let writer = se_writer_open(index, 30_000_000, 1);
            for (id, book, text) in [(1u64, 5u64, "abc def"), (2, 5, "def"), (3, 6, "ghi")] {
                assert_eq!(se_writer_add(writer, id, id, book, text.as_ptr(), text.len()), 0);
            }
            assert_eq!(se_writer_commit(writer, ptr::null(), 0), 0);
            se_writer_close(writer);

            let take = |p: *mut c_char| {
                assert!(!p.is_null(), "{}", se_last_error_message());
                let s = CStr::from_ptr(p).to_str().unwrap().to_string();
                se_string_free(p);
                s
            };
            assert_eq!(take(se_vectors_info(index)), "null");
            let q = [0.0f32, 0.0, 1.0];
            let empty = take(se_semantic_search(index, q.as_ptr(), 3, ptr::null(), 0, ptr::null()));
            assert!(empty.contains("\"hits\":[]"), "{empty}");

            // Otzaria's vectors (one base .oxv): book id:5 line 0 has two windows, line 1 one; id:6 one line
            let oxv = dir.join("lib.oxv");
            let unit = |a: usize| {
                let mut v = vec![0.0f32; 3];
                v[a] = 1.0;
                v
            };
            seforim_vectors::oxv::write_test_segment(
                &oxv,
                3,
                false,
                "v1",
                &[("id:5", vec![(0, 0, unit(1)), (0, 1, unit(2)), (1, 0, unit(0))]), ("id:6", vec![(0, 0, unit(2))])],
            )
            .unwrap();
            let o = oxv.to_string_lossy().into_owned();
            let info = take(se_oxv_info(o.as_ptr(), o.len()));
            assert!(info.contains("\"name\":\"id:5\",\"records\":3,\"lines\":2"), "{info}");
            let info = take(se_vectors_open(index, o.as_ptr(), o.len()));
            assert!(info.contains("\"slots_live\":4") && info.contains("\"books_unmapped\":2"), "{info}");
            let name = "id:5";
            let lines = [1u64, 2];
            assert_eq!(se_vectors_map_book(index, name.as_ptr(), name.len(), 5, lines.as_ptr(), 2), 0);
            let maps = r#"[{"name":"id:6","book_id":6,"line_ids":[3]}]"#;
            assert_eq!(se_vectors_set_mapping(index, maps.as_ptr(), maps.len(), 0), 0);
            assert!(take(se_vectors_info(index)).contains("\"books_unmapped\":0"));

            let opts = r#"{"k":2}"#;
            let sem = take(se_semantic_search(index, q.as_ptr(), 3, opts.as_ptr(), opts.len(), ptr::null()));
            let v: serde_json::Value = serde_json::from_str(&sem).unwrap();
            // line 3 (its own vector on the query) first, then line 1 by its second window, penalized
            assert_eq!(v["hits"][0]["line_id"], 3, "{sem}");
            assert_eq!((v["hits"][1]["line_id"].as_u64(), v["hits"][1]["chunk"].as_u64()), (Some(1), Some(1)), "{sem}");
            assert!((v["hits"][1]["score"].as_f64().unwrap() - 0.96).abs() < 1e-3, "{sem}");
            // book filter
            let opts5 = r#"{"k":3,"books":[5]}"#;
            let sem5 = take(se_semantic_search(index, q.as_ptr(), 3, opts5.as_ptr(), opts5.len(), ptr::null()));
            assert!(!sem5.contains("\"book_id\":6"), "{sem5}");
            // wrong dims: an error
            assert!(se_semantic_search(index, q.as_ptr(), 2, ptr::null(), 0, ptr::null()).is_null());

            // hybrid: Otzaria's session, the lexical side fused with the semantic one
            let query = "abc";
            let h = se_hybrid_open(index, query.as_ptr(), query.len(), q.as_ptr(), 3, ptr::null(), 0);
            assert!(!h.is_null(), "{}", se_last_error_message());
            let page = take(se_hybrid_page(h, 0, 10, ptr::null()));
            let f: serde_json::Value = serde_json::from_str(&page).unwrap();
            let hits = f["hits"].as_array().unwrap();
            assert_eq!(hits.iter().find(|e| e["line_id"] == 1).unwrap()["source"], "both", "{page}");
            assert!(hits.iter().any(|e| e["line_id"] == 3 && e["source"] == "semantic"), "{page}");
            assert_eq!((f["more"].as_bool(), f["window"].as_u64()), (Some(false), Some(21)), "{page}");
            assert_eq!(se_hybrid_add_texts(h, ptr::null(), 0, ptr::null(), 0, [0u64].as_ptr()), 0);
            se_hybrid_close(h);
            se_hybrid_close(ptr::null_mut());

            // a set directory in Otzaria's layout opens the same way; closing forgets it
            let set = dir.join("vectors");
            seforim_vectors::set::write_test_set(&set, 1, &[(&oxv, vec![], None)]).unwrap();
            let s = set.to_string_lossy().into_owned();
            assert!(take(se_vectors_open(index, s.as_ptr(), s.len())).contains("\"generation\":1"));
            assert_eq!(se_vectors_close(index), 0);
            assert_eq!(take(se_vectors_info(index)), "null");
            assert!(se_vectors_open(index, d.as_ptr(), d.len()).is_null());

            let gz = dir.join("lib.oxv.zst");
            std::fs::write(&gz, zstd::encode_all(&std::fs::read(&oxv).unwrap()[..], 1).unwrap()).unwrap();
            let (g, x) = (gz.to_string_lossy().into_owned(), dir.join("x.oxv").to_string_lossy().into_owned());
            assert_eq!(se_oxv_expand(g.as_ptr(), g.len(), x.as_ptr(), x.len()) as u64, std::fs::metadata(&oxv).unwrap().len());
            assert!(se_oxv_info(g.as_ptr(), g.len()).is_null());
            se_index_close(index);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_book_mapped_from_rows_through_the_c_abi() {
        use seforim_vectors::chunker::{book_lines, BookLine, Chunker, ChunkerConfig, LineRef};
        use seforim_vectors::oxv::{TestBook, TestCodec, TestSegment, TestSlot};
        unsafe {
            let dir = std::env::temp_dir().join(format!("se_ffi_rows_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let d = dir.join("index").to_string_lossy().into_owned();
            let index = se_index_create(d.as_ptr(), d.len(), ptr::null(), 0);
            assert!(!index.is_null());
            let rows: Vec<&[u8]> = vec![b"first line that is long enough to stand", b"second line long enough\nthird line long enough to stand alone"];
            let lines = book_lines(&rows, &[]);
            let refs: Vec<LineRef<'_>> = lines.iter().map(BookLine::as_ref).collect();
            let chunker = Chunker::production();
            let slots = (0..lines.len())
                .map(|p| {
                    let key = chunker.window_keys(&refs, p)[0].1;
                    let mut v = vec![0.0f32; 4];
                    v[p] = 1.0;
                    TestSlot { key: u64::from_be_bytes(key.0[..8].try_into().unwrap()), hint: p as u32, window: 0, vector: v }
                })
                .collect();
            let oxv = dir.join("b.oxv");
            std::fs::create_dir_all(&dir).unwrap();
            let seg = TestSegment {
                dims: 4,
                codec: TestCodec::F32,
                delta: false,
                from: 0,
                to: 1,
                tag: "v1".into(),
                books: vec![TestBook { name: "id:9".into(), slots, ..Default::default() }],
                occurrences: vec![],
                tombstones: vec![],
            };
            seforim_vectors::oxv::write_segment(&oxv, &seg).unwrap();
            let take = |p: *mut c_char| {
                assert!(!p.is_null(), "{}", se_last_error_message());
                let s = CStr::from_ptr(p).to_str().unwrap().to_string();
                se_string_free(p);
                s
            };
            let name = "id:9";
            let ids = [7u64, 8];
            let content: Vec<u8> = rows.concat();
            let offsets = [0u64, rows[0].len() as u64, content.len() as u64];
            let identity = ChunkerConfig::production().identity();
            // no set open: an error
            assert!(se_vectors_map_book_rows(index, name.as_ptr(), name.len(), 9, ids.as_ptr(), 2, content.as_ptr(), content.len(), offsets.as_ptr(), ptr::null(), 0, identity)
                .is_null());
            let o = oxv.to_string_lossy().into_owned();
            take(se_vectors_open(index, o.as_ptr(), o.len()));
            let v = take(se_vectors_map_book_rows(index, name.as_ptr(), name.len(), 9, ids.as_ptr(), 2, content.as_ptr(), content.len(), offsets.as_ptr(), ptr::null(), 0, identity));
            assert_eq!(v, r#"{"lines":3,"records":3,"verified":3,"moved":0,"unverified":0}"#);
            let q = [0.0f32, 0.0, 1.0, 0.0];
            let sem = take(se_semantic_search(index, q.as_ptr(), 4, ptr::null(), 0, ptr::null()));
            let j: serde_json::Value = serde_json::from_str(&sem).unwrap();
            // the row's second line (ordinal 2) is the row's line id
            assert_eq!(j["hits"][0]["line_id"], 8, "{sem}");
            // bad offsets
            let bad = [0u64, 999, 5];
            assert!(se_vectors_map_book_rows(index, name.as_ptr(), name.len(), 9, ids.as_ptr(), 2, content.as_ptr(), content.len(), bad.as_ptr(), ptr::null(), 0, identity).is_null());
            se_index_close(index);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// se_oxv_verify + se_vectors_install on the seforim-vectors fixtures: a base and
    /// two deltas installed through the C ABI open as a set; a corrupted block and a
    /// delta without a base are refused; a null manifest of length 0 is "none".
    #[test]
    fn vector_set_install_through_c_abi() {
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../seforim-vectors/tests/data/install");
        let tmp = std::env::temp_dir().join(format!("se_ffi_install_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let take = |p: *mut c_char| -> Option<serde_json::Value> {
            if p.is_null() {
                return None;
            }
            let v = serde_json::from_str(unsafe { CStr::from_ptr(p) }.to_str().unwrap()).unwrap();
            unsafe { se_string_free(p) };
            Some(v)
        };
        let set = tmp.join("set").to_string_lossy().into_owned();
        let mut plain = Vec::new();
        for name in ["base-v1", "delta-v1-v2", "delta-v2-v3"] {
            let src = data.join(format!("{name}.oxv.zst")).to_string_lossy().into_owned();
            let dst = tmp.join(format!("{name}.oxv")).to_string_lossy().into_owned();
            assert!(unsafe { se_oxv_expand(src.as_ptr(), src.len(), dst.as_ptr(), dst.len()) } > 0);
            let manifest = std::fs::read_to_string(data.join(format!("{name}.manifest.json"))).unwrap();
            plain.push((dst, manifest));
        }
        unsafe {
            let (p, m) = &plain[0];
            let r = take(se_oxv_verify(p.as_ptr(), p.len(), m.as_ptr(), m.len(), ptr::null())).unwrap();
            assert_eq!(r["manifest_checked"], true);
            let r = take(se_oxv_verify(p.as_ptr(), p.len(), ptr::null(), 0, ptr::null())).unwrap();
            assert_eq!(r["manifest_checked"], false);
            // a delta first: refused, the reason in se_last_error
            let (d, dm) = &plain[1];
            assert!(take(se_vectors_install(set.as_ptr(), set.len(), d.as_ptr(), d.len(), dm.as_ptr(), dm.len(), ptr::null(), 0, ptr::null(), 0, ptr::null())).is_none());
            assert!(!CStr::from_ptr(se_last_error()).to_bytes().is_empty());
            // the wrong query model, then a cancelled install: refused, nothing installed
            let wrong = r#"{"family_id":"g","chunking_identity":7}"#;
            assert!(take(se_vectors_install(set.as_ptr(), set.len(), p.as_ptr(), p.len(), m.as_ptr(), m.len(), ptr::null(), 0, wrong.as_ptr(), wrong.len(), ptr::null())).is_none());
            let token = se_cancel_token_new();
            se_cancel_token_cancel(token);
            assert!(take(se_vectors_install(set.as_ptr(), set.len(), p.as_ptr(), p.len(), m.as_ptr(), m.len(), ptr::null(), 0, ptr::null(), 0, token)).is_none());
            assert_eq!(se_last_error_cancelled(), 1);
            assert!(take(se_oxv_verify(p.as_ptr(), p.len(), ptr::null(), 0, token)).is_none());
            se_cancel_token_free(token);
            assert!(!std::path::Path::new(&set).join("CURRENT").exists());
            let model = r#"{"family_id":"f","chunking_identity":7}"#;
            for (i, (p, m)) in plain.iter().enumerate() {
                let r = take(se_vectors_install(set.as_ptr(), set.len(), p.as_ptr(), p.len(), m.as_ptr(), m.len(), ptr::null(), 0, model.as_ptr(), model.len(), ptr::null())).unwrap();
                assert_eq!(r["generation"], i as u64 + 1, "{r}");
                assert_eq!(r["library_version"], i as u64 + 1);
            }
            // a corrupted block
            let mut bytes = std::fs::read(&plain[0].0).unwrap();
            let sections = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
            let at = (0..sections).map(|i| 224 + i * 40).find(|&e| u16::from_le_bytes([bytes[e], bytes[e + 1]]) == 0x0012).unwrap();
            let off = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
            bytes[off] ^= 0x40;
            let bad = tmp.join("bad.oxv").to_string_lossy().into_owned();
            std::fs::write(&bad, bytes).unwrap();
            assert!(take(se_oxv_verify(bad.as_ptr(), bad.len(), ptr::null(), 0, ptr::null())).is_none());
        }
        let info = seforim_vectors::set::SegmentSet::open(std::path::Path::new(&set)).unwrap().info(None);
        assert_eq!((info.library_version, info.segments.len()), (3, 3));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

// ── User documents (issue #44) ──────────────────────────────────────────

/// A user index (host documents that are not in the library: text files,
/// PDFs, attached libraries) in its own directory. Its documents are
/// written with `se_writer_put_document` / `se_writer_delete_documents` on a
/// writer of `se_user_index_handle(user)`; every other index call (writer,
/// info, FM sidecar, background merge, search alone) takes that handle too.
/// Ids: book `2^51 + n`, line and position `2^51 + (n << 24) + line`; the
/// library's ids stay below 2^51 (`se_user_id_base`).
pub struct UserIndex(std::sync::Arc<LineIndex>);

/// The first id of the user range (2^51): a hit with a line id from here to
/// 2^52 is a user document's line.
#[no_mangle]
pub extern "C" fn se_user_id_base() -> u64 {
    seforim_engine::USER_ID_BASE
}

/// Creates a fresh user index in `dir` (wiping it). `options_json` as in
/// `se_index_create`: it must equal the library's options to be attached.
///
/// # Safety
/// Pointers must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_user_index_create(
    dir: *const u8,
    dir_len: usize,
    options_json: *const u8,
    options_len: usize,
) -> *mut UserIndex {
    guard(ptr::null_mut(), || {
        // the options are parsed (and checked) exactly as se_index_create does
        let probe = se_index_options(str_arg(options_json, options_len)?)?;
        let dir = PathBuf::from(str_arg(dir, dir_len)?);
        let index = LineIndex::create_user(&dir, probe).map_err(err)?;
        Ok(Box::into_raw(Box::new(UserIndex(std::sync::Arc::new(index)))))
    })
}

/// Opens an existing user index (an error for a library index).
///
/// # Safety
/// Pointers must be valid for their lengths.
#[no_mangle]
pub unsafe extern "C" fn se_user_index_open(dir: *const u8, dir_len: usize) -> *mut UserIndex {
    guard(ptr::null_mut(), || {
        let dir = PathBuf::from(str_arg(dir, dir_len)?);
        let index = LineIndex::open(&dir).map_err(err)?;
        if !index.is_user_index() {
            return Err(format!("{} is a library index, not a user index", dir.display()));
        }
        Ok(Box::into_raw(Box::new(UserIndex(std::sync::Arc::new(index)))))
    })
}

/// The user index as an index handle for every other index call. Valid
/// while `user` is open; never pass it to `se_index_close`.
///
/// # Safety
/// `user` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_user_index_handle(user: *const UserIndex) -> *const LineIndex {
    match user.as_ref() {
        Some(u) => std::sync::Arc::as_ptr(&u.0),
        None => ptr::null(),
    }
}

/// Closes the host's handle (an index it is attached to keeps it open until
/// detached or closed).
///
/// # Safety
/// `user` must come from `se_user_index_create/open`, closed once.
#[no_mangle]
pub unsafe extern "C" fn se_user_index_close(user: *mut UserIndex) {
    if !user.is_null() {
        drop(Box::from_raw(user));
    }
}

/// Attaches a user index to a library index (null `user` detaches): a search
/// with `"sources":"all"` (the default) reads both, library hits first, as
/// one search (one expansion cap, exact counts); `"library"` and `"user"`
/// read one. Same options and fields required. 0 = ok, -1 on error.
///
/// # Safety
/// `index` and `user` (or null) must be live.
#[no_mangle]
pub unsafe extern "C" fn se_index_attach_user(index: *const LineIndex, user: *const UserIndex) -> i32 {
    guard(-1, || {
        let index = index.as_ref().ok_or("null index")?;
        index.attach_user_index(user.as_ref().map(|u| u.0.clone())).map_err(err)?;
        Ok(0)
    })
}

/// Adds or replaces a user document (visible after `se_writer_commit`, which
/// writes the documents list atomically with the lines). JSON:
/// `{"key":"c:/a.pdf","stamp":"<host hash>","lines":[{"text":"...","locator":3,
/// "section_id":null,"no_cross":false}]}` (`locator`, e.g. a page, defaults to 0).
/// Returns the document's book id (kept on a replace), -1 on error (a library
/// index's writer included).
///
/// # Safety
/// `writer` must be live; the JSON valid for its length.
#[no_mangle]
pub unsafe extern "C" fn se_writer_put_document(writer: *mut LineWriter, json: *const u8, json_len: usize) -> i64 {
    guard(-1, || {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Doc {
            key: String,
            #[serde(default)]
            stamp: String,
            lines: Vec<seforim_engine::DocLine>,
        }
        let d: Doc = serde_json::from_str(str_arg(json, json_len)?).map_err(|e| format!("invalid document: {e}"))?;
        let w = writer.as_mut().ok_or("null writer")?;
        Ok(w.put_document(&d.key, &d.stamp, &d.lines).map_err(err)? as i64)
    })
}

/// Deletes the user documents with these keys (JSON array of strings;
/// unknown keys skipped). Returns how many were indexed, -1 on error.
///
/// # Safety
/// `writer` must be live; the JSON valid for its length.
#[no_mangle]
pub unsafe extern "C" fn se_writer_delete_documents(writer: *mut LineWriter, keys_json: *const u8, keys_len: usize) -> i64 {
    guard(-1, || {
        let keys: Vec<String> = serde_json::from_str(str_arg(keys_json, keys_len)?).map_err(|e| format!("invalid keys: {e}"))?;
        let w = writer.as_mut().ok_or("null writer")?;
        Ok(w.delete_documents(&keys).map_err(err)? as i64)
    })
}

/// The user documents of a user index, or of the user index attached to a
/// library index (none: `[]`), by book id: JSON
/// `[{"key":"...","stamp":"...","book_id":N,"lines":N}]`.
///
/// # Safety
/// `index` must be live.
#[no_mangle]
pub unsafe extern "C" fn se_index_documents(index: *const LineIndex) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        Ok(out_string(serde_json::to_string(&index.documents().map_err(err)?).map_err(err)?))
    })
}

/// Where each line comes from: JSON array, one entry per id,
/// `{"key":"...","locator":N,"line":N}` for a user line, null for a library
/// line or an unknown id.
///
/// # Safety
/// `index` must be live; `ids` valid for `count` values.
#[no_mangle]
pub unsafe extern "C" fn se_index_line_sources(index: *const LineIndex, ids: *const u64, count: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let ids = if count == 0 { &[][..] } else { std::slice::from_raw_parts(ids, count) };
        Ok(out_string(serde_json::to_string(&index.line_sources(ids).map_err(err)?).map_err(err)?))
    })
}

/// The stored texts of user lines (the user index keeps them; library
/// texts come from the host's database): JSON array of strings, null for a
/// line the user index does not hold.
///
/// # Safety
/// `index` must be live; `ids` valid for `count` values.
#[no_mangle]
pub unsafe extern "C" fn se_index_line_texts(index: *const LineIndex, ids: *const u64, count: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let index = index.as_ref().ok_or("null index")?;
        let ids = if count == 0 { &[][..] } else { std::slice::from_raw_parts(ids, count) };
        Ok(out_string(serde_json::to_string(&index.line_texts(ids).map_err(err)?).map_err(err)?))
    })
}
