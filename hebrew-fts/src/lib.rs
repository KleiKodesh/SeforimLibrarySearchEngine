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
pub mod marks;
mod scanner_reference;
pub mod prepared;
pub mod query_parser;
pub mod quote_free;
pub mod scanner;
pub mod set_automaton;
pub mod snippet;
pub mod tokenizer;
pub mod user_regex;
pub mod variants;
pub mod walk;
pub mod wildcard;
pub mod word_rule;

pub use wildcard::{HebrewWildcardQuery, WildcardPattern, WildcardRejected};

pub use exact_terms::ExactTermsQuery;
pub use fuzzy::HebrewFuzzyQuery;
pub use grammar_query::HebrewGrammarQuery;
pub use prepared::{PreparedQueryGroups, PreparedTerm};
pub use query_parser::{tokenize_query, QueryTerm, QueryToken, MAX_FUZZY_DISTANCE};
pub use scanner::{scan, scan_with, TextToken, TextTokens, WordSpan};
pub use snippet::{Proximity, SnippetDescriptor, SnippetWindowLocator, DEFAULT_CONTEXT_WORDS, MARK_STRIDE};
pub use set_automaton::SetAutomaton;
pub use user_regex::{HebrewRegexQuery, UserRegex, UserRegexError};
pub use tokenizer::{register_hebrew_tokenizer, HebrewTokenizer, HEBREW_TOKENIZER_NAME};
pub use marks::MarkFree;
pub use walk::{HebrewWalkAutomaton, HebrewWalkQuery, TypoMode, WalkOptions};
