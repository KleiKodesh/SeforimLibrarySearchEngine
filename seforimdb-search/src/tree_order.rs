//! Port of `SeforimDbBookTreeOrder.cs`: each book's PRECEDENCE in the
//! library, one number per book. The full-text index stores lines in it, so
//! results come back in it.
//!
//! The walk numbers, per folder: (1) the folder's own BASE texts, (2) its
//! subfolders recursively, (3) its remaining own books. Categories in load
//! order (negative orderIndex last among siblings), custom (negative-id)
//! entries last per level, and books whose category row is missing under a
//! synthetic last root.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::catalog_title::tokenize;
use crate::error::Result;
use crate::schema::column_exists;

/// A category row, in load order.
#[derive(Debug, Clone)]
pub struct CategoryInput {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub title: String,
}

/// A book row, in load order. `is_dependent`: the database's `dependenceType`
/// is set (commentary, targum, …); such a book is never a base text whatever
/// its title.
#[derive(Debug, Clone)]
pub struct BookInput {
    pub id: i64,
    pub category_id: i64,
    pub title: String,
    pub is_base_book: bool,
    pub is_dependent: bool,
}

/// bookId → tree order (0-based), read from the database.
pub fn load(conn: &Connection) -> Result<HashMap<i64, usize>> {
    let has_order_index = column_exists(conn, "category", "orderIndex")?;
    let has_is_base_book = column_exists(conn, "book", "isBaseBook")?;
    let has_dependence_type = column_exists(conn, "book", "dependenceType")?;

    let categories_sql = if has_order_index {
        "SELECT id, parentId, title FROM category ORDER BY level, CASE WHEN orderIndex < 0 THEN 1 \
         ELSE 0 END, orderIndex"
    } else {
        "SELECT id, parentId, title FROM category ORDER BY level"
    };
    let mut categories = Vec::new();
    {
        let mut stmt = conn.prepare(categories_sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            categories.push(CategoryInput {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            });
        }
    }

    let books_sql = format!(
        "SELECT b.id, b.categoryId, b.title, group_concat(a.name, ', ') AS authors, {} AS \
         isBaseBook, {} AS dependenceType FROM book b LEFT JOIN book_author ba ON ba.bookId = \
         b.id LEFT JOIN author a ON a.id = ba.authorId GROUP BY b.id ORDER BY b.orderIndex",
        if has_is_base_book { "b.isBaseBook" } else { "0" },
        if has_dependence_type { "b.dependenceType" } else { "NULL" },
    );
    let mut books = Vec::new();
    {
        let mut stmt = conn.prepare(&books_sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let is_dependent = match row.get_ref(5)? {
                rusqlite::types::ValueRef::Text(t) => {
                    !std::str::from_utf8(t).unwrap_or("").trim().is_empty()
                }
                rusqlite::types::ValueRef::Integer(i) => i != 0,
                _ => false,
            };
            books.push(BookInput {
                id: row.get(0)?,
                category_id: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                is_base_book: row.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0,
                is_dependent,
            });
        }
    }

    Ok(compute(&categories, &books))
}

/// bookId → tree order (0-based). Both lists in the frontend's load order.
pub fn compute(categories: &[CategoryInput], books: &[BookInput]) -> HashMap<i64, usize> {
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut cat_books: HashMap<i64, Vec<&BookInput>> = HashMap::new();
    let titles: HashMap<i64, &str> = categories.iter().map(|c| (c.id, c.title.as_str())).collect();

    let mut roots = Vec::new();
    for c in categories {
        match c.parent_id {
            Some(pid) if titles.contains_key(&pid) => children.entry(pid).or_default().push(c.id),
            _ => roots.push(c.id),
        }
    }

    let mut orphaned = Vec::new();
    for b in books {
        if titles.contains_key(&b.category_id) {
            cat_books.entry(b.category_id).or_default().push(b);
        } else {
            orphaned.push(b);
        }
    }

    for l in children.values_mut() {
        stable_sort_custom_last(l, |&id| id);
    }
    for l in cat_books.values_mut() {
        stable_sort_custom_last(l, |b| b.id);
    }
    stable_sort_custom_last(&mut roots, |&id| id);

    let mut orders: HashMap<i64, usize> = HashMap::with_capacity(books.len());
    let mut counter = 0usize;

    fn walk(
        category_id: i64,
        children: &HashMap<i64, Vec<i64>>,
        cat_books: &HashMap<i64, Vec<&BookInput>>,
        titles: &HashMap<i64, &str>,
        orders: &mut HashMap<i64, usize>,
        counter: &mut usize,
    ) {
        let own = cat_books.get(&category_id);
        let folder_title = titles.get(&category_id).copied().unwrap_or("");

        // 1. The folder's own base texts, ahead of the subfolders that comment on them.
        if let Some(own) = own {
            for b in own {
                if is_base_text(b, folder_title) {
                    orders.insert(b.id, *counter);
                    *counter += 1;
                }
            }
        }
        // 2. Subfolders, in load order.
        if let Some(cs) = children.get(&category_id) {
            for &child in cs {
                walk(child, children, cat_books, titles, orders, counter);
            }
        }
        // 3. The folder's remaining own books.
        if let Some(own) = own {
            for b in own {
                if !orders.contains_key(&b.id) {
                    orders.insert(b.id, *counter);
                    *counter += 1;
                }
            }
        }
    }

    for &root in &roots {
        walk(root, &children, &cat_books, &titles, &mut orders, &mut counter);
    }
    for b in orphaned {
        orders.insert(b.id, counter); // synthetic last root
        counter += 1;
    }
    orders
}

/// True when `book` is the text its folder exists for: flagged isBaseBook, or
/// not a dependent work and titled like the folder itself, with a leading
/// "ספר" / "מדרש" / "מסכת" ignored on either side.
pub fn is_base_text(book: &BookInput, folder_title: &str) -> bool {
    if book.is_base_book {
        return true;
    }
    if book.is_dependent {
        return false;
    }
    title_tokens_equal(&book.title, folder_title)
}

const GENERIC_TITLE_HEADS: [&str; 3] = ["ספר", "מדרש", "מסכת"];

fn title_tokens_equal(book_title: &str, folder_title: &str) -> bool {
    let a = strip_generic_head(tokenize(book_title));
    let b = strip_generic_head(tokenize(folder_title));
    !a.is_empty() && a == b
}

fn strip_generic_head(mut tokens: Vec<String>) -> Vec<String> {
    if tokens.len() > 1 && GENERIC_TITLE_HEADS.contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    tokens
}

/// Stable: items with a negative id move after the others, order kept.
fn stable_sort_custom_last<T>(items: &mut [T], id: impl Fn(&T) -> i64) {
    items.sort_by_key(|x| if id(x) < 0 { 1 } else { 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(id: i64, parent: Option<i64>, title: &str) -> CategoryInput {
        CategoryInput {
            id,
            parent_id: parent,
            title: title.into(),
        }
    }
    fn book(id: i64, cat: i64, title: &str, base: bool, dep: bool) -> BookInput {
        BookInput {
            id,
            category_id: cat,
            title: title.into(),
            is_base_book: base,
            is_dependent: dep,
        }
    }

    #[test]
    fn base_text_first_then_subfolders_then_rest() {
        // folder "טור": Tur (titled like folder) + a mefarshim subfolder + another book
        let cats = vec![cat(1, None, "טור"), cat(2, Some(1), "מפרשים")];
        let books = vec![
            book(10, 1, "בית יוסף", false, true), // dependent: not base even if...
            book(11, 1, "טור", false, false),      // titled like folder → base
            book(20, 2, "דרכי משה", false, true),
        ];
        let o = compute(&cats, &books);
        assert_eq!(o[&11], 0); // base text first
        assert_eq!(o[&20], 1); // subfolder
        assert_eq!(o[&10], 2); // remaining own book
    }

    #[test]
    fn generic_head_ignored_and_orphans_last() {
        let cats = vec![cat(1, None, "ספר המצוות")];
        let books = vec![
            book(1, 1, "המצוות", false, false), // equals folder minus "ספר"
            book(2, 1, "פירוש", false, false),
            book(3, 99, "orphan", false, false), // category missing
        ];
        let o = compute(&cats, &books);
        assert_eq!(o[&1], 0);
        assert_eq!(o[&2], 1);
        assert_eq!(o[&3], 2);
    }

    #[test]
    fn custom_negative_ids_sort_last() {
        let cats = vec![cat(-5, None, "custom"), cat(1, None, "a")];
        let books = vec![book(1, -5, "x", false, false), book(2, 1, "y", false, false)];
        let o = compute(&cats, &books);
        assert_eq!(o[&2], 0);
        assert_eq!(o[&1], 1);
    }
}
