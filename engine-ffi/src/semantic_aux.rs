//! C ABI of the semantic helpers around the vector set (otzaria_search_engine 7.2.0):
//! passage highlights (`semantic_passage_highlights`, the model in the host), the set's
//! directory info (`semantic_vectors_info`) and its coverage of the host's library
//! (`semantic_coverage`, the host feeding book by book).

use std::ffi::c_char;
use std::path::Path;
use std::ptr;

use hebrew_fts::otzaria::semantic_highlight::{self, PASSAGE_SNIPPET_BYTES};
use seforim_vectors::chunker::{Chunker, ChunkerConfig, LineRef};
use seforim_vectors::coverage::CoverageCounter;
use serde_json::json;

use crate::{err, guard, install_err, out_string, str_arg, CancelToken};

/// A short line's context: its book's lines around it, with their sections.
#[derive(serde::Deserialize)]
struct PassageContext {
    lines: Vec<String>,
    #[serde(default)]
    sections: Vec<u64>,
    index: usize,
}

#[derive(serde::Deserialize)]
struct PassageTarget {
    text: String,
    #[serde(default)]
    window: u8,
    #[serde(default)]
    context: Option<PassageContext>,
}

#[derive(serde::Deserialize)]
struct PlanRequest {
    query: String,
    targets: Vec<PassageTarget>,
    #[serde(default)]
    forms: Vec<String>,
    /// The set's chunking identity (0 / absent: the production recipe).
    #[serde(default)]
    chunking_identity: u64,
}

/// The text a target's highlight is computed on: its window text, else its line.
fn display_text(chunker: &Chunker, target: &PassageTarget) -> String {
    let window = match &target.context {
        // A section not given is unknown: as Otzaria's `context_window`, a window that needs
        // it is not formed and the line's own text is shown.
        Some(c) if c.index < c.lines.len() => {
            let texts: Vec<&str> = c.lines.iter().map(String::as_str).collect();
            chunker.display_window_text_with(&texts, |i| c.sections.get(i).copied(), c.index, target.window)
        }
        _ => chunker.display_window_text_with(&[target.text.as_str()], |_| None, 0, target.window),
    };
    window.unwrap_or_else(|| target.text.clone())
}

/// Semantic passage highlights, step 1 (otzaria_search_engine 7.2.0
/// `semantic_passage_highlights` up to its embeddings). `request` JSON:
/// `{"query", "targets": [{"text", "window", "context": {"lines", "sections", "index"}}],
/// "forms": [..], "chunking_identity"}`: each target's line text, its matched window
/// (`SemanticHit` chunk) and, for a short line, its book's lines around it with their
/// sections (`index` = the line's place in them); `forms` = the host dictionary's highlight
/// forms of the query words. Returns `{"query_text", "needs_query_vector", "lines": [{"text",
/// "chosen", "clauses", "out_of_budget"}]}`: the host embeds `query_text` as a query and each
/// line's `clauses` as passages, then calls [`se_passage_mark`]. Null on error.
///
/// # Safety
/// `request` valid UTF-8 for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_passage_plan(request: *const u8, len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let r: PlanRequest = serde_json::from_str(str_arg(request, len)?).map_err(err)?;
        let config = if r.chunking_identity == 0 {
            ChunkerConfig::production()
        } else {
            ChunkerConfig::by_identity(r.chunking_identity).ok_or_else(|| format!("chunking identity {} is not a recipe this build implements", r.chunking_identity))?
        };
        let chunker = Chunker::new(config).map_err(|e| e.0)?;
        let texts: Vec<String> = r.targets.iter().map(|t| display_text(&chunker, t)).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let plan = semantic_highlight::passage_plan(&r.query, &refs, &r.forms);
        let lines: Vec<_> = plan
            .lines
            .iter()
            .zip(&texts)
            .map(|(l, text)| json!({"text": text, "chosen": l.chosen, "clauses": l.clauses, "out_of_budget": l.out_of_budget}))
            .collect();
        Ok(out_string(json!({"query_text": plan.query_text, "needs_query_vector": plan.needs_query_vector, "lines": lines}).to_string()))
    })
}

#[derive(serde::Deserialize)]
struct MarkLine {
    text: String,
    #[serde(default)]
    chosen: Vec<usize>,
    #[serde(default)]
    vectors: Vec<Vec<f32>>,
}

#[derive(serde::Deserialize)]
struct MarkRequest {
    query: String,
    #[serde(default)]
    query_vector: Vec<f32>,
    lines: Vec<MarkLine>,
    #[serde(default)]
    max_chars: Option<usize>,
}

/// Semantic passage highlights, step 2: `request` JSON `{"query", "query_vector", "lines":
/// [{"text", "chosen", "vectors"}], "max_chars"}` (each line's `text` / `chosen` as
/// [`se_passage_plan`] returned them, `vectors` the host's passage vectors of its clauses in
/// order; `max_chars` default 800). Returns `[{"snippet_html", "is_highlighted",
/// "span_score"}]`, one per line. Null on error.
///
/// # Safety
/// `request` valid UTF-8 for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_passage_mark(request: *const u8, len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let r: MarkRequest = serde_json::from_str(str_arg(request, len)?).map_err(err)?;
        let budget = r.max_chars.unwrap_or(PASSAGE_SNIPPET_BYTES);
        let out: Vec<_> = r
            .lines
            .iter()
            .map(|l| {
                let h = if l.chosen.is_empty() || l.vectors.len() != l.chosen.len() {
                    Default::default()
                } else {
                    semantic_highlight::passage_mark(&r.query, &l.text, &l.chosen, &l.vectors, &r.query_vector, budget)
                };
                json!({"snippet_html": h.snippet_html, "is_highlighted": !h.snippet_html.is_empty(), "span_score": h.span_score})
            })
            .collect();
        Ok(out_string(serde_json::Value::Array(out).to_string()))
    })
}

/// What a vector set's directory holds, from its small files alone (Otzaria
/// `semantic_vectors_info`): JSON `{"present", "identity_digest", "model_family_id",
/// "library_version", "library_release_tag", "generation", "segments": [{"id", "kind",
/// "from_library_version", "to_library_version", "slots", "slots_dead",
/// "foreign_unresolved", "size"}], "slots_live", "slots_dead", "bytes_on_disk",
/// "needs_compaction", "recovered_from_previous", "chunking_identity"}`; `present: false`
/// when nothing is installed. Null on error (`vector install [corrupt|incompatible]`).
///
/// # Safety
/// `dir` valid UTF-8 for `dir_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_dir_info(dir: *const u8, dir_len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let dir = str_arg(dir, dir_len)?;
        let info = seforim_vectors::coverage::dir_info(Path::new(dir)).map_err(install_err)?;
        Ok(out_string(serde_json::to_string(&info).map_err(err)?))
    })
}

/// Opens a coverage count of the set at `dir` (Otzaria `semantic_coverage`): every key the
/// set holds is read. Feed every live book with [`se_vectors_coverage_add_book`], read the
/// result with [`se_vectors_coverage_result`], free with [`se_vectors_coverage_free`]. Null on
/// error (`vector install [missing]` when nothing is installed; cancelled by `token`).
///
/// # Safety
/// `dir` valid UTF-8 for `dir_len` bytes; `token` null or a live cancel token.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_coverage_open(dir: *const u8, dir_len: usize, token: *const CancelToken) -> *mut CoverageCounter {
    guard(ptr::null_mut(), || {
        let dir = str_arg(dir, dir_len)?;
        let flag = token.as_ref().map(|t| t.flag());
        let counter = CoverageCounter::open(Path::new(dir), flag.as_deref()).map_err(install_err)?;
        Ok(Box::into_raw(Box::new(counter)))
    })
}

#[derive(serde::Deserialize)]
struct CoverageBook {
    lines: Vec<String>,
    sections: Vec<u64>,
}

/// One book's lines in order, `{"lines": [..], "sections": [..]}` (one section per line,
/// required: Otzaria's heading-count sections). 0, -1 on error.
///
/// # Safety
/// `counter` from [`se_vectors_coverage_open`]; `book_json` valid UTF-8 for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_coverage_add_book(counter: *mut CoverageCounter, book_json: *const u8, len: usize) -> i32 {
    guard(-1, || {
        let counter = counter.as_mut().ok_or_else(|| "null coverage counter".to_string())?;
        let book: CoverageBook = serde_json::from_str(str_arg(book_json, len)?).map_err(err)?;
        // Every line has its section, as every Otzaria document has its sectionId (its key
        // recomputation fails on one without).
        if book.sections.len() != book.lines.len() {
            return Err(format!("coverage book: {} section(s) for {} line(s); every line needs its section", book.sections.len(), book.lines.len()));
        }
        let lines: Vec<LineRef<'_>> = book.lines.iter().zip(&book.sections).map(|(text, &section)| LineRef { text, section }).collect();
        counter.add_book(&lines);
        Ok(0)
    })
}

/// JSON `{"live_keyed_lines", "covered_lines", "books_live", "books_covered",
/// "vectors_library_version", "ratio"}`. Null on error.
///
/// # Safety
/// `counter` from [`se_vectors_coverage_open`].
#[no_mangle]
pub unsafe extern "C" fn se_vectors_coverage_result(counter: *const CoverageCounter) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let counter = counter.as_ref().ok_or_else(|| "null coverage counter".to_string())?;
        Ok(out_string(serde_json::to_string(&counter.result()).map_err(err)?))
    })
}

/// Frees a coverage counter (null is ignored).
///
/// # Safety
/// `counter` from [`se_vectors_coverage_open`], not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn se_vectors_coverage_free(counter: *mut CoverageCounter) {
    if !counter.is_null() {
        drop(Box::from_raw(counter));
    }
}
