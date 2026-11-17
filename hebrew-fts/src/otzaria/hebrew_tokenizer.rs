// Ported verbatim from otzaria_search_engine 7.2.0 (MIT, Copyright (c) 2026 otzaria),
// rust/src/hebrew_tokenizer.rs; see THIRD-PARTY-NOTICES.md. Changes: module paths, the test
// module dropped (golden tests live in hebrew-fts/tests/otzaria_helpers.rs), `is_alphanumeric`
// read from the 7.2.0 binary's pinned table (unicode_alnum.rs).
use std::borrow::Cow;

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::otzaria::hebrew_query::{
    canonical_mark_order, fold_presentation_form, is_attached_mark, is_invisible_char,
    is_word_mark, CGJ,
};

/// טוקנייזר עברי שמתנהג כמו SimpleTokenizer עם שלוש תוספות:
/// גרש (' או ׳) בין אותיות או בסוף טוקן נשמר בו; גרשיים (" או ״) בין
/// אותיות נשמרות בטוקן (ראשי-תיבות הם טרם אחד); ותווים "שקופים"
/// ([`is_transparent`]) אינם שוברים טוקן אך מושמטים מטקסטו — כך שטקסט
/// מאונדקס שמשמר פיסוק לתצוגה ממופה לאותם טרמים כמו שאילתה שעברה
/// `sanitize_query`. ׳/״ מקופלים בטקסט הטוקן ל-'/" ASCII, וזוג גרשים
/// רצוף בין אותיות (מוסכמת `רמב''ם` בקבצים ישנים) מאוחד ל-`"`.
///
/// דוגמאות:
///   "תוס'"    → ["תוס'"]     (גרש בסוף — נשמר)
///   "ז\"ל"    → ["ז\"ל"]     (גרשיים באמצע — חלק מהטוקן)
///   "רמב''ם"  → ["רמב\"ם"]   (זוג גרשים → גרשיים)
///   "רמב\""   → ["רמב"]      (גרשיים בקצה — מפריד)
///   "א.ב"     → ["אב"]       (נקודה שקופה — לא שוברת ולא נכללת)
///   "עא:"     → ["עא"]       (פיסוק בסוף — מחוץ לטוקן)
///
/// עם `emit_quote_free` (אנליזטורי התצוגה ו-`lineEdge`): טוקן שמכיל גרש/גרשיים
/// פולט מיד אחריו טוקן-תאום נטול-גרשיים באותה עמדה ובאותם offsets, כך שהדגשה
/// של `רמבם` צובעת `רמב"ם`. השדות עצמם מאונדקסים בלי התאום (סכמה 6) — שאילתה
/// מגיעה לצורות עם הגרשיים דרך `quote_expansion`.
///
/// עם `keep_marks` (השדה המנוקד, `textVocalized`): ניקוד/טעמים/combining
/// *נשמרים* בטקסט הטוקן, בסדר הקנוני ובלי CGJ ([`canonical_mark_order`]). גבולות
/// הטוקנים ומיקומיהם זהים אחד-לאחד בשני המצבים (סימן צמוד הוא תו-מילה בשניהם), כך ששני השדות של אותה שורה
/// מטוקננים לאותן עמדות; רק טקסט הטרם שונה.
#[derive(Clone, Default)]
pub struct HebrewTokenizer {
    pub emit_quote_free: bool,
    pub keep_marks: bool,
}

pub struct HebrewTokenStream<'a> {
    text: &'a str,
    token: Token,
    byte_pos: usize,
    token_count: usize,
    keep_marks: bool,
    emit_quote_free: bool,
    /// טוקן-תאום נטול-גרשיים שממתין להיפלט (ראו `emit_quote_free`).
    pending_quote_free: Option<Token>,
    /// הטוקן הקודם הוא X של זוג `(X) [Y]` — הבא (Y) נפלט באותה עמדה.
    share_next_position: bool,
}

impl Tokenizer for HebrewTokenizer {
    type TokenStream<'a> = HebrewTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        HebrewTokenStream {
            text,
            token: Token::default(),
            byte_pos: 0,
            token_count: 0,
            keep_marks: self.keep_marks,
            emit_quote_free: self.emit_quote_free,
            pending_quote_free: None,
            share_next_position: false,
        }
    }
}

/// תו "שקוף": הסט ש-`sanitize_query` מוחק בצד השאילתה. אינו שובר טוקן
/// ואינו נכלל בטקסטו, אך נשאר בטקסט השמור (שהוא טקסט התצוגה).
/// נקודה נשארת שקופה (ראשי-תיבות `פ.ב.י`, מספרים `3.14`) וכן `[]`
/// (השלמות עריכתיות תוך-מילה: `יב[ע]ר`), ואיתם התווים הבלתי-נראים
/// (bidi/zero-width — עקבי עם מסלול ה-PDF שמוחק אותם לפני אינדוקס).
/// פיסוק ההפסקה — `,;:!?(){}` — הוא *מפריד* (שובר טוקן), כמו מקף, `|`
/// וסוף-פסוק: בקורפוס הוא מדביק בעיקר מספרי הערות-שוליים (`רעהו,10`)
/// ופיסוק חסר-רווח (`שלום,עולם`).
#[inline]
pub(crate) fn is_transparent(c: char) -> bool {
    matches!(
        c,
        '*' | '[' | ']' | '^' | '$' | '\\' | '+' | '.' | '~' | '`'
    ) || is_invisible_char(c)
}

/// גרש: ASCII, עברי (U+05F3), או ציטוט-יחיד טיפוגרפי (U+2018/U+2019 —
/// ברינדור RTL שתי הצורות משמשות באותו תפקיד, ראו סריקת הקורפוס).
#[inline]
pub(crate) fn is_geresh(c: char) -> bool {
    matches!(c, '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}')
}

/// גרשיים: ASCII, עברי (U+05F4), או ציטוט-כפול טיפוגרפי (U+201C/U+201D).
#[inline]
pub(crate) fn is_gershayim(c: char) -> bool {
    matches!(c, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}')
}

/// תו שממשיך טוקן: אות/ספרה, או סימן צמוד ([`is_word_mark`] — ניקוד/טעם
/// עברי, combining כללי, varika). הטקסט המנורמל לאינדוקס נקי מניקוד ממילא;
/// קבלת הסימנים כאן היא הגנת-עומק כדי שקלט שלא נורמל (למשל דרך
/// `add_document` ישיר) לא יתפרק באמצע מילה, ותמיכה אמיתית ב-combining
/// הכלליים שנשמרים בטקסט (תעתיק ערבית-יהודית: `כלת̇ום`).
#[inline]
pub(crate) fn is_word_char(c: char) -> bool {
    // מסלול מהיר לאותיות עבריות ול-ASCII: חיפוש טבלאות Unicode הוא רוב זמן הטוקניזציה.
    if is_hebrew_letter(c) || c.is_ascii_alphanumeric() {
        return true;
    }
    !c.is_ascii() && (super::unicode_alnum::is_alphanumeric(c) || is_word_mark(c))
}

/// תו שפותח טוקן: אות/ספרה של ממש, לא סימן צמוד (ראו [`next_token_boundaries`]).
#[inline]
pub(crate) fn starts_token(c: char) -> bool {
    if is_hebrew_letter(c) || c.is_ascii_alphanumeric() {
        return true;
    }
    !c.is_ascii() && super::unicode_alnum::is_alphanumeric(c) && !is_word_mark(c)
}

#[inline]
fn is_hebrew_letter(c: char) -> bool {
    matches!(c, '\u{05D0}'..='\u{05EA}')
}

/// האם `c` יכול להופיע *בתוך* טוקן בלי לשבור אותו: תו-מילה, תו שקוף
/// (`פ.ב.י`, `יב[ע]ר`) או גרש/גרשיים (`רמב״ם`, `תוס'`).
///
/// מכאן נגזרות מחלקות התווים של [`crate::otzaria::display_highlight`], כדי שספירת
/// המילים שבין מילות השאילתה בהדגשה תהיה זהה לספירת הטוקנים באינדקס.
/// שכפול ידני של קבוצת התווים סטה מכאן בעבר: `כי־גר` נספרה כמילה אחת
/// (מקף) בעוד האינדקס מפצל אותה לשתיים.
#[inline]
pub(crate) fn continues_token(c: char) -> bool {
    is_word_char(c) || is_transparent(c) || is_geresh(c) || is_gershayim(c)
}

/// Puts `text`'s marks in canonical order ([`canonical_mark_order`]).
fn sort_marks(text: &mut String) {
    if let Cow::Owned(sorted) = canonical_mark_order(text) {
        *text = sorted;
    }
}

/// דוחף `"` לטקסט הטוקן, אלא אם הוא כבר מסתיים ב-`"`. שני רצפי-גרשיים
/// שמופרדים רק בסימנים צמודים (למשל `רמב''ְ"ם` — קלט פתולוגי אך אפשרי)
/// היו מייצרים `""` צמודות אחרי הסרת הסימנים — בניגוד לערובה שטוקן לעולם
/// אינו מכיל רצף גרשיים כפול; הצורה הקנונית במילון היא `"` יחידה.
#[inline]
fn push_gershayim_deduped(text: &mut String) {
    if !text.ends_with('"') {
        text.push('"');
    }
}

/// תו שדורש בנייה איטית של טקסט הטוקן (סינון/קיפול) במקום העתקה ישירה.
/// `'` ASCII נכלל בגלל איחוד זוג-הגרשים (`''`→`"`); `"` ASCII מועתק
/// כמות-שהוא ולכן אינו כאן.
#[inline]
fn needs_normalising(c: char) -> bool {
    is_word_mark(c)
        || is_transparent(c)
        || is_geresh(c)
        || (is_gershayim(c) && c != '"')
        || fold_presentation_form(c).is_some()
}

/// גבולות הטוקן הבא החל מ-`start_byte`: `(tok_start, tok_end)` בבייטים,
/// על גבולות char. **ליבת-הגבולות המשותפת** — הטוקנייזר (צד האינדקס) סורק
/// איתה טקסט משמר-פיסוק, ו-`split_query_words` (צד השאילתה) סורק איתה
/// שאילתה שעברה `sanitize_query` — מקור אמת אחד לכללי "היכן מילה נשברת",
/// כך ששני הצדדים אינם יכולים לסטות זה מזה.
pub(crate) fn next_token_boundaries(text: &str, start_byte: usize) -> Option<(usize, usize)> {
    let slice = &text[start_byte..];

    // מצא את תחילת הטוקן — חייב להתחיל באות/ספרה של ממש. שימו לב:
    // `is_alphanumeric` לבדו לא מספיק, כי סימני נִיקוד (להבדיל מטעמים)
    // וגם כמה combining כלליים (כמו U+0345) מסווגים ב-Unicode
    // כ-Other_Alphabetic; בלי הסינון סימן תועה בין רווחים היה פותח
    // טוקן שמתרוקן אחרי הסרת הסימנים.
    let tok_start_rel = slice
        .char_indices()
        .find(|(_, c)| starts_token(*c))
        .map(|(i, _)| i)?;

    let tok_start = start_byte + tok_start_rel;
    let s = &text[tok_start..];

    // `pos` — סוף רצף תווי-המילה האחרון. הלולאה מדמה את הטקסט כאילו
    // התווים השקופים נמחקו (כמו בצד השאילתה), אך בגבולות המקוריים.
    let mut pos = 0;
    let tok_end_rel = loop {
        pos += s[pos..]
            .char_indices()
            .find(|(_, c)| !is_word_char(*c))
            .map(|(i, _)| i)
            .unwrap_or(s.len() - pos);

        // מבט קדימה דרך תווים שקופים: ספור את רצף הגרשים/הגרשיים עד התו
        // הבא שאינו שקוף ואינו גרש — הוא שמכריע אם הרצף פנימי (הטוקן
        // נמשך דרכו) או סוגר. מונים בלבד — בלי הקצאת מחרוזת: הרצף מורכב
        // מגרשים בלבד, ולכן ספירת (גרש, גרשיים) קובעת את הצורה חד-ערכית.
        let mut geresh_n = 0usize;
        let mut gershayim_n = 0usize;
        let mut first_is_geresh = false;
        let mut first_quote_end = 0usize;
        let mut next: Option<(usize, char)> = None;
        for (i, c) in s[pos..].char_indices() {
            if is_geresh(c) || is_gershayim(c) {
                if geresh_n + gershayim_n == 0 {
                    first_quote_end = pos + i + c.len_utf8();
                    first_is_geresh = is_geresh(c);
                }
                if is_geresh(c) {
                    geresh_n += 1;
                } else {
                    gershayim_n += 1;
                }
            } else if !is_transparent(c) {
                next = Some((pos + i, c));
                break;
            }
        }

        match next {
            Some((i, c)) if is_word_char(c) => match (geresh_n, gershayim_n) {
                // בלי גרשים — פיסוק שקוף בין אותיות, הטוקן נמשך.
                // גרש / זוג-גרשים (D2) / גרשיים פנימיים — נכללים
                // בטוקן והוא נמשך.
                (0, 0) | (1, 0) | (2, 0) | (0, 1) => pos = i,
                // כל צירוף אחר (גרשיים כפולות, שלשות, ערבוב...) — מפריד.
                _ => break pos,
            },
            // סוף מילה אמיתי: גרש סוגר (הראשון בלבד) נכלל בטוקן;
            // גרשיים סוגרות הן מפריד ונשארות בחוץ.
            _ => {
                break if first_is_geresh {
                    first_quote_end
                } else {
                    pos
                }
            }
        }
    };

    Some((tok_start, tok_start + tok_end_rel))
}

/// זוג קריאות של מילה אחת: `(X) [Y]` — קרי וכתיב במקרא, ותיקון נוסח
/// "(מחק) [גרוס]" בשאר הספרים. כש-`[tok_start, tok_end)` הוא X, מחזיר את
/// גבולות Y. שתיהן מאונדקסות באותה עמדה, כך שביטוי נמצא עם כל אחת מהן.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PairedReading {
    pub second_start: usize,
    pub second_end: usize,
    /// Byte immediately after the closing `]`, including trailing punctuation
    /// inside the brackets (e.g. the maqaf in `[לכה־]נא`).
    pub after_pair: usize,
}

/// A trailing separator inside a reading does not add another token. Brackets
/// and markup delimiters are excluded so nested or incomplete readings cannot
/// be mistaken for a complete pair.
fn is_reading_suffix(c: char) -> bool {
    !is_word_char(c) && !matches!(c, '(' | ')' | '[' | ']' | '<' | '>')
}

pub(crate) fn paired_reading_after(
    text: &str,
    tok_start: usize,
    tok_end: usize,
) -> Option<PairedReading> {
    if !text[..tok_start].ends_with('(') {
        return None;
    }
    let after_close = text[tok_end..]
        .trim_start_matches(is_reading_suffix)
        .strip_prefix(')')?;
    let after_open = after_close.trim_start().strip_prefix('[')?;
    let y_start = text.len() - after_open.len();
    let (start, end) = next_token_boundaries(text, y_start)?;
    if start != y_start {
        return None;
    }
    let after_pair = text[end..]
        .trim_start_matches(is_reading_suffix)
        .strip_prefix(']')?;
    Some(PairedReading {
        second_start: start,
        second_end: end,
        after_pair: text.len() - after_pair.len(),
    })
}

impl<'a> TokenStream for HebrewTokenStream<'a> {
    fn advance(&mut self) -> bool {
        // טוקן-תאום ממתין (נטול-גרשיים, אותה עמדה) — נפלט לפני המילה הבאה.
        if let Some(pending) = self.pending_quote_free.take() {
            self.token = pending;
            return true;
        }
        match next_token_boundaries(self.text, self.byte_pos) {
            None => false,
            Some((tok_start, tok_end)) => {
                let tok_text = &self.text[tok_start..tok_end];
                self.token.text.clear();
                // נרמול טקסט הטוקן לצורת מילון הטרמים: הסרת ניקוד/טעמים
                // ופיסוק שקוף, קיפול ׳→' ו-״→", איחוד זוג גרשים רצוף
                // ל-`"` (D2) ופירוק Presentation Forms. במסלול המהיר —
                // הרוב המכריע — העתקה ישירה.
                if tok_text.chars().any(needs_normalising) {
                    for c in tok_text.chars() {
                        // סימן צמוד (ניקוד/טעם/combining כללי): מוסר במצב
                        // הרגיל, נשמר במצב המנוקד — למעט varika (U+FB1E),
                        // שהוא Presentation Form ומקופל ל-"" בהמשך.
                        if is_word_mark(c) && c != '\u{FB1E}' {
                            // Dropping CGJ here lets the quotes around it merge as in `text`.
                            if self.keep_marks && c != CGJ {
                                self.token.text.push(c);
                            }
                            continue;
                        }
                        if is_transparent(c) {
                            continue;
                        }
                        if is_geresh(c) {
                            // זוג גרשים (גם דרך שקופים, שכבר סוננו) הוא
                            // פנימי בהכרח — גרש סוגר נבלע יחיד — ולכן
                            // בטוח לאחדו לגרשיים.
                            if self.token.text.ends_with('\'') {
                                self.token.text.pop();
                                push_gershayim_deduped(&mut self.token.text);
                            } else {
                                self.token.text.push('\'');
                            }
                        } else if is_gershayim(c) {
                            push_gershayim_deduped(&mut self.token.text);
                        } else if let Some(folded) = fold_presentation_form(c) {
                            let keep_marks = self.keep_marks;
                            self.token.text.extend(
                                folded
                                    .chars()
                                    .filter(|f| keep_marks || !is_attached_mark(*f)),
                            );
                        } else {
                            self.token.text.push(c);
                        }
                    }
                    if self.keep_marks {
                        sort_marks(&mut self.token.text);
                    }
                } else {
                    self.token.text.push_str(tok_text);
                }
                self.token.offset_from = tok_start;
                self.token.offset_to = tok_end;
                if self.share_next_position {
                    self.share_next_position = false;
                    self.token.position = self.token_count - 1;
                } else {
                    self.token.position = self.token_count;
                    self.token_count += 1;
                }
                self.share_next_position =
                    paired_reading_after(self.text, tok_start, tok_end).is_some();
                self.byte_pos = tok_end;
                // מצב אינדוקס: מילה עם גרש/גרשיים מטמיעה גם את צורתה
                // הנקייה באותה עמדה ובאותם offsets — שאילתות phrase רואות
                // עמדה אחת, וההדגשה יורשת את טווח המילה המקורית.
                if self.emit_quote_free && self.token.text.contains(['\'', '"']) {
                    let mut stripped: String = self
                        .token
                        .text
                        .chars()
                        .filter(|c| *c != '\'' && *c != '"')
                        .collect();
                    // A removed quote can join two mark runs.
                    if self.keep_marks {
                        sort_marks(&mut stripped);
                    }
                    if !stripped.is_empty() && stripped != self.token.text {
                        let mut dup = self.token.clone();
                        dup.text = stripped;
                        self.pending_quote_free = Some(dup);
                    }
                }
                true
            }
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

