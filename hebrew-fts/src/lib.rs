//! Hebrew FTS compatibility layer — a faithful Rust port of KitveiHakodesh's
//! FtsLib (C#), so a tantivy index produces the same terms, matches, snippets
//! and highlights as the SQLite-FTS5-based engine.
//!
//! Ported so far (from FtsLib git HEAD):
//! - `HebrewChars.cs` → [`chars`]: the per-character rules every path shares.
//! - `HtmlBlockTags.cs` → `html_tags`: block-tag / entity handling.
//! - `HtmlWordScanner.cs` + `TokenStream.cs` → [`scanner`]: the single-pass
//!   HTML-aware word scanner with raw-span and visible-count tracking.
//! - A tantivy [`Tokenizer`](tantivy::tokenizer::Tokenizer) over the scanner →
//!   [`tokenizer`].
//!
//! Still to port: query parser + expanders, snippet window locator, searcher.

pub mod chars;
pub mod exact_terms;
pub mod fuzzy;
pub mod grammar;
pub mod grammar_query;
mod html_tags;
mod scanner_reference;
pub mod prepared;
pub mod quote_free;
pub mod scanner;
pub mod set_automaton;
pub mod snippet;
pub mod tokenizer;
pub mod variants;
pub mod walk;
pub mod wildcard;

pub use wildcard::{HebrewWildcardQuery, WildcardPattern, WildcardRejected};

pub use exact_terms::ExactTermsQuery;
pub use fuzzy::HebrewFuzzyQuery;
pub use grammar_query::HebrewGrammarQuery;
pub use prepared::{PreparedQueryGroups, PreparedTerm};
pub use scanner::{scan, scan_with, TextToken, TextTokens, WordSpan};
pub use snippet::{Proximity, SnippetDescriptor, SnippetWindowLocator, DEFAULT_CONTEXT_WORDS, MARK_STRIDE};
pub use set_automaton::SetAutomaton;
pub use tokenizer::{register_hebrew_tokenizer, HebrewTokenizer, HEBREW_TOKENIZER_NAME};
pub use walk::{HebrewWalkAutomaton, HebrewWalkQuery, WalkOptions};
