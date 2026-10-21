//! Tantivy [`Tokenizer`] built on the FtsLib-compatible scanner.
//!
//! Registering this tokenizer on a text field indexes: nikud stripped, ASCII
//! lowercased, intra-word quotes kept and normalized to '"' (רש״י → רש"י; the
//! walks can optionally treat them as transparent), inline HTML tags
//! transparent, block tags / entities / maqaf as separators. Every word of
//! one letter or more is indexed with its position (Otzaria's word model, see
//! [`crate::word_model`]); a word whose normalized form is over
//! [`MAX_TERM_BYTES`] bytes is not a term but keeps its position (Otzaria's
//! `RemoveLongFilter`). FtsLib's 2..=29-letter words are a subset the
//! FtsLib-model searches restrict themselves to.

use crate::scanner;
use crate::word_model::{WordModel, MAX_TERM_BYTES};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager};

/// Name under which [`register_hebrew_tokenizer`] registers the
/// mark-stripping tokenizer.
pub const HEBREW_TOKENIZER_NAME: &str = "hebrew";

/// Name of the mark-keeping variant (nikud/te'amim preserved in terms); use
/// it for fields holding vocalized text. The walks can then opt in/out of
/// ignoring the marks per search.
pub const HEBREW_MARKED_TOKENIZER_NAME: &str = "hebrew_marked";

/// Tokenizer matching FtsLib's `HtmlWordScanner` (see module docs).
///
/// Token `text` is the normalized form; `offset_from`/`offset_to` are byte
/// offsets of the word's raw span in the source (including nikud and any
/// inline tags the word runs across).
#[derive(Clone, Default)]
pub struct HebrewTokenizer {
    keep_marks: bool,
}

impl HebrewTokenizer {
    /// Variant that keeps nikud/te'amim in the indexed terms.
    pub fn keeping_marks() -> HebrewTokenizer {
        HebrewTokenizer { keep_marks: true }
    }
}

/// Token stream over pre-scanned tokens.
pub struct HebrewTokenStream {
    tokens: std::vec::IntoIter<Token>,
    current: Token,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> HebrewTokenStream {
        let mut tokens = Vec::new();
        let mut position = 0;
        scanner::scan_words(text, self.keep_marks, WordModel::Otzaria, |word, span| {
            if word.len() > MAX_TERM_BYTES {
                position += 1; // not a term, but it holds its place
                return;
            }
            tokens.push(Token {
                offset_from: span.byte_start,
                offset_to: span.byte_end,
                position,
                text: word.to_string(),
                position_length: 1,
            });
            position += 1;
        });
        HebrewTokenStream {
            tokens: tokens.into_iter(),
            current: Token::default(),
        }
    }
}

impl TokenStream for HebrewTokenStream {
    fn advance(&mut self) -> bool {
        match self.tokens.next() {
            Some(token) => {
                self.current = token;
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

/// Registers [`HebrewTokenizer`] as `"hebrew"` (marks stripped) and
/// `"hebrew_marked"` (marks kept) on a tokenizer manager (typically
/// `index.tokenizers()`).
pub fn register_hebrew_tokenizer(manager: &TokenizerManager) {
    manager.register(
        HEBREW_TOKENIZER_NAME,
        TextAnalyzer::from(HebrewTokenizer::default()),
    );
    manager.register(
        HEBREW_MARKED_TOKENIZER_NAME,
        TextAnalyzer::from(HebrewTokenizer::keeping_marks()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<Token> {
        let mut tokenizer = HebrewTokenizer::default();
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().clone());
        }
        out
    }

    #[test]
    fn tokenizes_like_ftslib() {
        let toks = tokens("וַיֹּאמֶר רש\"י על־פני");
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["ויאמר", "רש\"י", "על", "פני"]);
        assert_eq!(toks[0].position, 0);
        assert_eq!(toks[3].position, 3);
    }

    #[test]
    fn one_letter_and_long_words_take_positions() {
        let long = "א".repeat(64); // 128 bytes: still a term
        let too_long = "ב".repeat(65); // 130 bytes: no term, keeps its position
        let toks = tokens(&format!("משה ו ה' {long} {too_long} תורה"));
        let got: Vec<(&str, usize)> = toks.iter().map(|t| (t.text.as_str(), t.position)).collect();
        assert_eq!(got, [("משה", 0), ("ו", 1), ("ה", 2), (long.as_str(), 3), ("תורה", 5)]);
    }

    #[test]
    fn offsets_are_bytes_into_source() {
        let text = "אב גד";
        let toks = tokens(text);
        assert_eq!(&text[toks[0].offset_from..toks[0].offset_to], "אב");
        assert_eq!(&text[toks[1].offset_from..toks[1].offset_to], "גד");
    }

    #[test]
    fn works_through_analyzer_and_manager() {
        let manager = TokenizerManager::default();
        register_hebrew_tokenizer(&manager);
        let mut analyzer = manager.get(HEBREW_TOKENIZER_NAME).unwrap();
        let mut stream = analyzer.token_stream("בְּרֵאשִׁית בָּרָא");
        let mut texts = Vec::new();
        while stream.advance() {
            texts.push(stream.token().text.clone());
        }
        assert_eq!(texts, ["בראשית", "ברא"]);
    }
}
