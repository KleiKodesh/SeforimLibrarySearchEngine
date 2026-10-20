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
    IndexOptions, LineDoc, LineIndex, LineWriter, SearchRequest, SearchResult, Snippet,
};

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(message: impl std::fmt::Display) {
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

/// JSON: `{"num_lines":N,"positions":bool,"keep_marks":bool,"payload":string|null}`.
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

/// Runs a search. `request_json` (all fields optional except groups):
/// ```json
/// {"groups":[{"kind":"word","word":"תורה","options":{"grammar_prefixes":true}},
///            {"kind":"wildcard","pattern":"שלו*"}],
///  "max_word_distance":5,"require_ordered":false,"context_words":8}
/// ```
/// Returns a search object holding every matching line (catalog order) and
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
        let request: SearchRequest =
            serde_json::from_str(str_arg(request_json, request_len)?).map_err(err)?;
        let result = index.search(&request).map_err(err)?;
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

/// Locates the snippets of a batch of matched lines. `texts` is `count`
/// UTF-8 strings given as parallel pointer and length arrays. Output, per
/// text: `[0]` when the line does not pass (filters), else `[1]` followed by
/// the snippet (SNIPPET_HEADER_U32 values, then mark_count × (start, length,
/// term_id)). Offsets are UTF-16 units into each text.
///
/// # Safety
/// Arrays must hold `count` entries; each text valid for its length.
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
        let texts = std::slice::from_raw_parts(texts, count);
        let lens = std::slice::from_raw_parts(lens, count);
        let mut strs = Vec::with_capacity(count);
        for i in 0..count {
            strs.push(str_arg(texts[i], lens[i])?);
        }
        let mut out = Vec::with_capacity(count * 16);
        for snippet in search.session.locate_many(&strs) {
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
            se_search_close(search);
            se_index_close(index);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
