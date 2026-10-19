//! Tantivy [`Tokenizer`] built on the FtsLib-compatible scanner.
//!
//! Registering this tokenizer on a text field makes tantivy index exactly the
//! terms that KitveiHakodesh's FTS5 index holds: nikud stripped, ASCII
//! lowercased, intra-word quotes dropped (רש"י → רשי), inline HTML tags
//! transparent, block tags / entities / maqaf as separators, words of 2..=29
//! normalized letters only.

use crate::scanner;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager};

/// Name under which [`register_hebrew_tokenizer`] registers the tokenizer.
pub const HEBREW_TOKENIZER_NAME: &str = "hebrew";

/// Tokenizer matching FtsLib's `HtmlWordScanner` (see module docs).
///
/// Token `text` is the normalized form; `offset_from`/`offset_to` are byte
/// offsets of the word's raw span in the source (including nikud and any
/// inline tags the word runs across).
#[derive(Clone, Default)]
pub struct HebrewTokenizer;

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
        scanner::scan(text, |word, span| {
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

/// Registers [`HebrewTokenizer`] as `"hebrew"` on a tokenizer manager
/// (typically `index.tokenizers()`).
pub fn register_hebrew_tokenizer(manager: &TokenizerManager) {
    manager.register(
        HEBREW_TOKENIZER_NAME,
        TextAnalyzer::from(HebrewTokenizer::default()),
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
        assert_eq!(texts, ["ויאמר", "רשי", "על", "פני"]);
        assert_eq!(toks[0].position, 0);
        assert_eq!(toks[3].position, 3);
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
