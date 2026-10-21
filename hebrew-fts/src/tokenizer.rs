//! Tantivy [`Tokenizer`] built on the scanner ([`crate::scanner`]: FtsLib's
//! HTML layer, Otzaria's word rule).
//!
//! Registering this tokenizer on a text field indexes every word with its
//! position: marks stripped (kept by `hebrew_marked`), lowercased, quotes
//! normalized (`'` for a geresh, `"` for a gershayim or two gereshes; the
//! walks can treat them as transparent), presentation forms folded, inline
//! HTML tags transparent, block tags / entities / maqaf as separators. The
//! second reading of `(X) [Y]` shares X's position. A word whose term has
//! [`MAX_TERM_BYTES`](crate::chars::MAX_TERM_BYTES) bytes or more is not a
//! term but keeps its position (Otzaria's `RemoveLongFilter`, FtsLib's
//! `Tokenizer.OnWord`).

use crate::chars::is_indexable_len;
use crate::scanner;
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
        scanner::scan_with(text, self.keep_marks, |word, span| {
            // A word whose term is too long is not a term but keeps its
            // position (Otzaria's RemoveLongFilter after its tokenizer).
            if !is_indexable_len(span.term_bytes) {
                return;
            }
            tokens.push(Token {
                offset_from: span.byte_start,
                offset_to: span.byte_end,
                position: span.position,
                text: word.to_string(),
                position_length: 1,
            });
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
        let long = "\u{05D0}".repeat(63); // 126 bytes: indexed
        let too_long = "\u{05D1}".repeat(64); // 128 bytes: not indexed
        let toks = tokens(&format!("\u{05DE} \u{05D5} \u{05D4}' {long} {too_long} 12 \u{05D1}5 (\u{05D0}) [\u{05D1}] \u{05EA}"));
        let got: Vec<(&str, usize)> = toks.iter().map(|t| (t.text.as_str(), t.position)).collect();
        assert_eq!(
            got,
            [
                ("\u{05DE}", 0),
                ("\u{05D5}", 1),
                ("\u{05D4}'", 2),
                (long.as_str(), 3),
                ("12", 5),
                ("\u{05D1}5", 6),
                ("\u{05D0}", 7),
                ("\u{05D1}", 7),
                ("\u{05EA}", 8)
            ]
        );
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
