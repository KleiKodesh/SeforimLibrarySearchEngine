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

/// The C ABI version. Bump it on EVERY change to the C ABI (a function
/// added, removed or renamed, a signature, a struct layout, a JSON shape or
/// an enum / flag value the hosts read). The C# and Dart packages carry the
/// value they were built for (`NativeAbi.Expected` / `seforimAbiVersion`) and
/// refuse to load a library with another value. `tools/release/check_sync.py`
/// fails CI when the three disagree.
pub const SE_ABI_VERSION: u32 = 6;

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
/// engine's. Free the result with `se_string_free`; null on error.
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
        let json = str_arg(options_json, options_len)?;
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

/// Why the index's FM sidecar file cannot be used (the reason behind the
/// search notice `fm_sidecar_unusable`), or "" when it is usable or absent.
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

/// Commits, storing `payload` (may be null) atomically with the documents.
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

    fn se_last_error_message() -> String {
        unsafe { CStr::from_ptr(se_last_error()).to_str().unwrap().to_string() }
    }
}
