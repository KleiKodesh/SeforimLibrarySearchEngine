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
//! - Calls on one object are not thread-safe; use one object per thread or
//!   lock. Different objects are independent.

use std::cell::RefCell;
use std::ffi::{c_char, CString};
use std::path::PathBuf;
use std::ptr;

use seforim_engine::{
    CancelToken, HitLines, IndexOptions, LineDoc, LineIndex, LineWriter, SearchOptions, SearchRequest, SearchResult, Snippet,
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

/// Otzaria's query sanitizer step `keep_second_of_paired_readings`: every
/// paired reading `(X) [Y]` (ketiv and qere) in `query` becomes ` Y `, as the
/// engine's query parser does. A host that numbers the query's words itself
/// (per-word options, per-pair spacing) runs it first, so its words are the
/// engine's. Free the result with `se_string_free`; null on error.
///
/// # Safety
/// `query` must be valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_reduce_paired_readings(query: *const u8, len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let q = str_arg(query, len)?;
        Ok(out_string(seforim_engine::reduce_paired_readings(q).into_owned()))
    })
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

// ── Index ───────────────────────────────────────────────────────────────

/// Creates a fresh index in `dir` (wiping it). `options_json`:
/// `{"positions":true,"keep_marks":false}` (both optional).
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
        let json = str_arg(options_json, options_len)?;
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Opts {
            positions: Option<bool>,
            keep_marks: Option<bool>,
        }
        let o: Opts = if json.trim().is_empty() {
            Opts::default()
        } else {
            serde_json::from_str(json).map_err(err)?
        };
        let defaults = IndexOptions::default();
        let options = IndexOptions {
            positions: o.positions.unwrap_or(defaults.positions),
            keep_marks: o.keep_marks.unwrap_or(defaults.keep_marks),
        };
        let index = LineIndex::create(&dir, options).map_err(err)?;
        Ok(Box::into_raw(Box::new(index)))
    })
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

/// JSON: `{"num_lines":N,"positions":bool,"keep_marks":bool,"line_data":bool,"payload":string|null}`
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
            "payload": index.committed_payload().map_err(err)?,
        });
        Ok(out_string(info.to_string()))
    })
}

/// Builds the FM-index sidecar over the term dictionary (FtsLib's
/// `BuildFmSidecar`), which serves leading-`*` wildcards and fuzzy words
/// without walking the whole dictionary. Needs a single segment: call after
/// `se_writer_merge_and_close`. Idempotent. Returns 1 when a sidecar is in
/// place, 0 when the index has several segments, no terms or more than 64
/// distinct characters (searches then use the walk), -1 on error.
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
            line_id,
            position,
            book_id,
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
            line_id,
            position,
            book_id,
            text: str_arg(text, text_len)?.to_string(),
            section_id: (flags & SE_LINE_HAS_SECTION != 0).then_some(section_id),
            no_cross: flags & SE_LINE_NO_CROSS != 0,
        };
        writer.add(&doc).map_err(err)?;
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
        writer.as_mut().ok_or("null writer")?.delete(line_id);
        Ok(0)
    })
}

/// Commits, storing `payload` (may be null) atomically with the documents.
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

/// Merges all segments into one, waits, and CLOSES the writer (it must not
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

/// Number of matching lines.
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

/// How the search was served, as JSON: `{"notices":["..."],"truncated":false}`.
/// `notices` are not errors: `"relevance_needs_positions"` = relevance order
/// was asked for a phrase / proximity / multi-word fuzzy query on an index
/// without positions, so the hits are in catalog order. `truncated`: a walk
/// hit its expansion cap (2000 per segment), so terms beyond it were not
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

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
            se_index_close(index);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
