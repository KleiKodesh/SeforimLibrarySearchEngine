//! Otzaria's text and query helpers (otzaria_search_engine 7.2.0), ported verbatim so
//! their results are byte-identical to the old engine's: query sanitizing / word split /
//! word spans, display-highlight patterns with their prepared matcher, the literal
//! in-book pattern, the smart display pattern and the user-regex display matcher.
#![allow(dead_code, clippy::all)]

mod api;
pub use api::*;

pub(crate) mod display_highlight;
mod fingerprint;
pub use fingerprint::{book_content_fingerprint, BookNoteRow};
pub(crate) mod inline_notes;
pub(crate) mod gap_phrase;
pub(crate) mod hebrew_query;
pub(crate) mod hebrew_tokenizer;
pub mod semantic_highlight;
pub(crate) mod highlight_matcher;
pub(crate) mod smart_query;
mod unicode_alnum;

/// otzaria_search_engine `api::search_engine::MAX_TOKEN_BYTES`.
pub(crate) const MAX_TOKEN_BYTES: usize = 128;
