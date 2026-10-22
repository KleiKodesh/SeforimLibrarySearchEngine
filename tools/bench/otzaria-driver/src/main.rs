//! Headless driver for Otzaria's search engine (crate `search_engine`, v5.0.0), for
//! benchmarking it against SeforimdbSerachEngine on the same seforim.db.
//! See README.md for the commands and every port/assumption.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use rusqlite::Connection;
use search_engine::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource, LibraryBookStatus,
};
use search_engine::api::search_engine::{
    configure_line_source, split_query_words, BookOrderEntry, ResultsOrder, SearchEngine,
    SearchPageResult, SearchScope, TextStorage, WordMatchMode,
};

type Res<T> = Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The engine panics on some open failures (SearchEngine::new): report those as errors.
    let outcome = std::panic::catch_unwind(|| run(&args));
    match outcome {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            eprintln!("error: panic: {msg}");
            ExitCode::from(2)
        }
    }
}

fn usage() -> String {
    "usage:\n  otzaria-driver build <seforim.db> <index dir>\n  otzaria-driver search <seforim.db> <index dir> --mode exact|advanced|fuzzy [--distance N] [--options k1,k2] [--exclude \"words\"] [--match all|any|most|atleast] [--match-count N] [--limit N] <query...>".into()
}

fn run(args: &[String]) -> Res<()> {
    match args.first().map(String::as_str) {
        Some("build") if args.len() == 3 => build(&args[1], &args[2]),
        Some("search") if args.len() >= 3 => search(&args[1], &args[2], &args[3..]),
        _ => Err(usage()),
    }
}

fn e<E: std::fmt::Display>(ctx: &str) -> impl FnOnce(E) -> String + '_ {
    move |err| format!("{ctx}: {err}")
}

// ── Catalogue ────────────────────────────────────────────────────────────────

struct Cat {
    id: i64,
    parent: Option<i64>,
    title: String,
}

struct BookRow {
    id: i64,
    category: Option<i64>,
    title: String,
    is_base: bool,
    is_dependent: bool,
}

fn has_column(db: &Connection, table: &str, col: &str) -> Res<bool> {
    let mut st = db
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(e("pragma"))?;
    let names = st
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(e("pragma"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(e("pragma"))?;
    Ok(names.iter().any(|n| n == col))
}

/// Categories in load order: `ORDER BY level, (orderIndex<0), orderIndex`
/// (SeforimDbSqlStrings.CatalogIndexCategories).
fn load_categories(db: &Connection) -> Res<Vec<Cat>> {
    let sql = if has_column(db, "category", "orderIndex")? {
        "SELECT id, parentId, title FROM category ORDER BY level, CASE WHEN orderIndex < 0 THEN 1 ELSE 0 END, orderIndex"
    } else {
        "SELECT id, parentId, title FROM category ORDER BY level"
    };
    let mut st = db.prepare(sql).map_err(e("categories"))?;
    let rows = st
        .query_map([], |r| {
            Ok(Cat {
                id: r.get(0)?,
                parent: r.get(1)?,
                title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .map_err(e("categories"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(e("categories"))?;
    Ok(rows)
}

/// Books in load order: `ORDER BY orderIndex, id` (C# orders by orderIndex only, Dart adds id).
fn load_books(db: &Connection) -> Res<Vec<BookRow>> {
    let base = if has_column(db, "book", "isBaseBook")? { "isBaseBook" } else { "0" };
    let dep = if has_column(db, "book", "dependenceType")? { "dependenceType" } else { "NULL" };
    let sql = format!(
        "SELECT id, categoryId, title, {base}, {dep} FROM book ORDER BY orderIndex, id"
    );
    let mut st = db.prepare(&sql).map_err(e("books"))?;
    let rows = st
        .query_map([], |r| {
            let dep: Option<String> = r.get::<_, Option<rusqlite::types::Value>>(4)?.and_then(|v| match v {
                rusqlite::types::Value::Text(s) => Some(s),
                rusqlite::types::Value::Integer(i) => Some(i.to_string()),
                _ => None,
            });
            Ok(BookRow {
                id: r.get(0)?,
                category: r.get(1)?,
                title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                is_base: r.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
                is_dependent: dep.map(|s| !s.trim().is_empty()).unwrap_or(false),
            })
        })
        .map_err(e("books"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(e("books"))?;
    Ok(rows)
}

/// Simplified port of SeforimDbCatalogTextNormalizer.Tokenize for TITLE comparison only:
/// whitespace split, keep letters/digits (lowercased), fold שלחן→שולחן. The abbreviation map
/// (quoted acronyms only on the index side) and the daf/amud step are NOT ported.
fn title_tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = s
        .split_whitespace()
        .filter_map(|w| {
            let t: String = w
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            if t.is_empty() {
                None
            } else if t == "שלחן" {
                Some("שולחן".to_string())
            } else {
                Some(t)
            }
        })
        .collect();
    if out.len() > 1 && ["ספר", "מדרש", "מסכת"].contains(&out[0].as_str()) {
        out.remove(0);
    }
    out
}

/// SeforimDbBookTreeOrder.IsBaseText.
fn is_base_text(b: &BookRow, folder_title: &str) -> bool {
    if b.is_base {
        return true;
    }
    if b.is_dependent {
        return false;
    }
    let a = title_tokens(&b.title);
    let f = title_tokens(folder_title);
    !a.is_empty() && a == f
}

/// Port of SeforimDbBookTreeOrder.Compute: per folder (1) own base texts, (2) subfolders
/// recursively, (3) remaining own books; negative ids last per level (stable); books whose
/// category is missing go last. Returns book ids in catalogue order.
fn tree_order(cats: &[Cat], books: &[BookRow]) -> Vec<i64> {
    let titles: HashMap<i64, &str> = cats.iter().map(|c| (c.id, c.title.as_str())).collect();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut roots = Vec::new();
    for c in cats {
        match c.parent {
            Some(p) if titles.contains_key(&p) => children.entry(p).or_default().push(c.id),
            _ => roots.push(c.id),
        }
    }
    let mut own: HashMap<i64, Vec<&BookRow>> = HashMap::new();
    let mut orphans = Vec::new();
    for b in books {
        match b.category {
            Some(c) if titles.contains_key(&c) => own.entry(c).or_default().push(b),
            _ => orphans.push(b.id),
        }
    }
    for l in children.values_mut() {
        l.sort_by_key(|id| (*id < 0) as u8);
    }
    for l in own.values_mut() {
        l.sort_by_key(|b| (b.id < 0) as u8);
    }
    roots.sort_by_key(|id| (*id < 0) as u8);

    let mut order = Vec::with_capacity(books.len());
    let mut seen = std::collections::HashSet::new();
    fn walk(
        c: i64,
        titles: &HashMap<i64, &str>,
        children: &HashMap<i64, Vec<i64>>,
        own: &HashMap<i64, Vec<&BookRow>>,
        order: &mut Vec<i64>,
        seen: &mut std::collections::HashSet<i64>,
    ) {
        let folder = titles[&c];
        if let Some(bs) = own.get(&c) {
            for b in bs {
                if is_base_text(b, folder) && seen.insert(b.id) {
                    order.push(b.id);
                }
            }
        }
        if let Some(cs) = children.get(&c) {
            for &ch in cs {
                walk(ch, titles, children, own, order, seen);
            }
        }
        if let Some(bs) = own.get(&c) {
            for b in bs {
                if seen.insert(b.id) {
                    order.push(b.id);
                }
            }
        }
    }
    for r in roots {
        walk(r, &titles, &children, &own, &mut order, &mut seen);
    }
    for o in orphans {
        if seen.insert(o) {
            order.push(o);
        }
    }
    order
}

/// Category.path of the app (lib/library/models/library.dart:33): "/A/B/C" from the root,
/// stopping at a category titled 'ספריית אוצריא'; then BookFacet._normalizeCategoryPath.
fn category_path(cat: i64, by_id: &HashMap<i64, &Cat>) -> String {
    let mut parts = Vec::new();
    let mut cur = by_id.get(&cat).copied();
    let mut guard = 0;
    while let Some(c) = cur {
        if c.title == "ספריית אוצריא" || guard > 64 {
            break;
        }
        parts.push(c.title.trim().to_string());
        cur = c.parent.and_then(|p| by_id.get(&p).copied());
        guard += 1;
    }
    parts.reverse();
    let parts: Vec<String> = parts
        .iter()
        .flat_map(|p| p.split('/').map(str::to_string).collect::<Vec<_>>())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("/{}", parts.join("/"))
    }
}

// ── build ────────────────────────────────────────────────────────────────────

fn stage(name: &str, t: Instant) {
    println!("  stage: {name}: {:.3}s", t.elapsed().as_secs_f64());
}

fn build(db_path: &str, index_dir: &str) -> Res<()> {
    let total = Instant::now();
    configure_line_source(db_path.to_string()).map_err(e("configure_line_source"))?;

    let t = Instant::now();
    let db = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(e("open db"))?;
    let cats = load_categories(&db)?;
    let books = load_books(&db)?;
    let order = tree_order(&cats, &books);
    let by_id: HashMap<i64, &Cat> = cats.iter().map(|c| (c.id, c)).collect();
    let book_by_id: HashMap<i64, &BookRow> = books.iter().map(|b| (b.id, b)).collect();
    stage("catalogue", t);

    std::fs::create_dir_all(index_dir).map_err(e("create index dir"))?;
    let t = Instant::now();
    let mut engine = SearchEngine::new(index_dir);
    engine.open_index_writer().map_err(e("open_index_writer"))?;
    stage("open", t);

    // Mirrors IndexingRepository._libraryBookInput (indexing_repository.dart:1099-1115).
    let inputs: Vec<LibraryBookInput> = order
        .iter()
        .map(|id| {
            let b = book_by_id[id];
            let cat_path = b.category.map(|c| category_path(c, &by_id)).unwrap_or_default();
            let key = format!("id:{}", b.id);
            LibraryBookInput {
                source: LibraryBookSource::Library,
                attached_slug: None,
                book_id: b.id,
                db_title: Some(b.title.clone()),
                title: b.title.clone(),
                topics: if cat_path.is_empty() { format!("/{key}") } else { format!("{cat_path}/{key}") },
                file_path: key,
                extra_facets: None,
                source_hash: None,
                text_storage: TextStorage::LibraryDb,
            }
        })
        .collect();

    let m = inputs.len();
    let cancel = IndexingCancellationToken::new();
    let t = Instant::now();
    let (mut done, mut docs, mut indexed, mut missing, mut failed) = (0usize, 0u64, 0usize, 0usize, 0usize);
    for chunk in inputs.chunks(200) {
        let res = engine
            .add_library_books(chunk.to_vec(), &cancel)
            .map_err(e("add_library_books"))?;
        for r in &res {
            match r.status {
                LibraryBookStatus::Indexed => {
                    indexed += 1;
                    docs += r.docs as u64;
                }
                LibraryBookStatus::Missing => missing += 1,
                _ => {
                    failed += 1;
                    eprintln!(
                        "book {} {:?}: {}",
                        r.book_id,
                        r.status,
                        r.error.clone().unwrap_or_default()
                    );
                }
            }
        }
        done += chunk.len();
        println!("indexed books {done} / {m} ({:.1} s)", t.elapsed().as_secs_f64());
    }
    stage("add books", t);

    let t = Instant::now();
    engine.commit().map_err(e("commit"))?;
    stage("commit", t);

    let t = Instant::now();
    engine.optimize().map_err(e("optimize"))?;
    stage("optimize", t);

    let t = Instant::now();
    let entries: Vec<BookOrderEntry> = order
        .iter()
        .enumerate()
        .map(|(i, id)| BookOrderEntry {
            file_path: format!("id:{id}"),
            catalogue_order: i as u32,
            generation_order: 0,
        })
        .collect();
    engine.set_book_order(entries).map_err(e("set_book_order"))?;
    stage("book order", t);

    println!(
        "build: {docs} lines, {indexed} books indexed ({missing} missing, {failed} failed) of {m} in {:.3}s",
        total.elapsed().as_secs_f64()
    );
    print_index_size(Path::new(index_dir))?;
    if failed > 0 {
        return Err(format!("{failed} books failed"));
    }
    Ok(())
}

fn print_index_size(dir: &Path) -> Res<()> {
    let mut files = Vec::new();
    fn walk(p: &Path, out: &mut Vec<(String, u64)>) -> std::io::Result<()> {
        for ent in std::fs::read_dir(p)? {
            let ent = ent?;
            let md = ent.metadata()?;
            if md.is_dir() {
                walk(&ent.path(), out)?;
            } else {
                let ext = ent
                    .path()
                    .extension()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.push((ext, md.len()));
            }
        }
        Ok(())
    }
    walk(dir, &mut files).map_err(e("index size"))?;
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let total: u64 = files.iter().map(|f| f.1).sum();
    println!("index size: {:.1} MB in {} files", mb(total), files.len());
    let mut by_ext: std::collections::BTreeMap<String, u64> = Default::default();
    for (x, n) in files {
        *by_ext.entry(x).or_default() += n;
    }
    let mut v: Vec<_> = by_ext.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (x, n) in v {
        println!("  .{x} {:.1} MB", mb(n));
    }
    Ok(())
}

// ── search ───────────────────────────────────────────────────────────────────

fn search(db_path: &str, index_dir: &str, rest: &[String]) -> Res<()> {
    let mut mode = None;
    let mut distance: u32 = 0;
    let mut distance_set = false;
    let mut options: Vec<String> = Vec::new();
    let mut exclude = String::new();
    let mut match_mode_s: Option<String> = None;
    let mut match_count = None;
    let mut limit: u32 = 2000;
    let mut repeat: u32 = 1;
    let mut words = Vec::new();
    let mut i = 0;
    let val = |i: usize| rest.get(i + 1).cloned().ok_or_else(|| format!("missing value for {}", rest[i]));
    while i < rest.len() {
        match rest[i].as_str() {
            "--mode" => { mode = Some(val(i)?); i += 2; }
            "--distance" => { distance = val(i)?.parse().map_err(e("--distance"))?; distance_set = true; i += 2; }
            "--options" => {
                options = val(i)?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                i += 2;
            }
            "--exclude" => { exclude = val(i)?; i += 2; }
            "--match" => {
                match_mode_s = Some(val(i)?);
                let _: Option<WordMatchMode> = Some(match val(i)?.as_str() {
                    "all" => WordMatchMode::All,
                    "any" => WordMatchMode::AnyWord,
                    "most" => WordMatchMode::MostWords,
                    "atleast" => WordMatchMode::AtLeast,
                    o => return Err(format!("bad --match {o}")),
                });
                i += 2;
            }
            "--match-count" => { match_count = Some(val(i)?.parse::<u32>().map_err(e("--match-count"))?); i += 2; }
            "--repeat" => { repeat = val(i)?.parse().map_err(e("--repeat"))?; i += 2; }
            "--limit" => { limit = val(i)?.parse().map_err(e("--limit"))?; i += 2; }
            _ => { words.push(rest[i].clone()); i += 1; }
        }
    }
    let query = words.join(" ");
    if query.trim().is_empty() {
        return Err(usage());
    }
    let mode = mode.ok_or_else(usage)?;

    configure_line_source(db_path.to_string()).map_err(e("configure_line_source"))?;
    let t = Instant::now();
    let engine = SearchEngine::new(index_dir);
    eprintln!("open: {} ms", t.elapsed().as_millis());

    // SearchQueryBuilder.expandGlobalOptionsToWords: the same options for every word,
    // keyed "{word}_{index}", over the engine's own split_query_words.
    let expand = |q: &str| -> HashMap<String, HashMap<String, bool>> {
        if options.is_empty() {
            return HashMap::new();
        }
        let global: HashMap<String, bool> = options.iter().map(|k| (k.clone(), true)).collect();
        split_query_words(q.to_string())
            .into_iter()
            .enumerate()
            .map(|(i, w)| (format!("{w}_{i}"), global.clone()))
            .collect()
    };

    let mut last: Option<SearchPageResult> = None;
    for _run in 0..repeat.max(1) {
    let t = Instant::now();
    let page: SearchPageResult = match mode.as_str() {
        "exact" => engine.search_and_count_exact(
            query.clone(), vec![], limit, 0, ResultsOrder::Catalogue, false, false, None,
        ),
        "fuzzy" => engine.search_and_count_fuzzy(
            query.clone(), vec![], limit, 0,
            if distance_set { distance as u8 } else { 1 },
            ResultsOrder::Catalogue, false, false, None,
        ),
        "advanced" => engine.search_and_count_advanced(
            query.clone(),
            exclude.clone(),
            vec![],
            limit,
            0,
            distance,
            distance,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            expand(&query),
            expand(&exclude),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            match match_mode_s.as_deref() {
                Some("all") => Some(WordMatchMode::All),
                Some("any") => Some(WordMatchMode::AnyWord),
                Some("most") => Some(WordMatchMode::MostWords),
                Some("atleast") => Some(WordMatchMode::AtLeast),
                _ => None,
            },
            match_count,
            None,
        ),
        o => return Err(format!("bad --mode {o}")),
    }
    .map_err(e("search"))?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "search: {} hits in {:.1} ms (first batch {:.1} ms)",
        page.total_count, ms, ms
    );
    if page.truncated {
        eprintln!("note: truncated=true (engine served a partial expansion)");
    }
    last = Some(page);
    }
    let page = last.unwrap();

    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for r in &page.results {
        let book: Option<i64> = r.file_path.strip_prefix("id:").and_then(|s| s.parse().ok());
        let line = serde_json::json!({
            "book": book,
            "segment": r.segment,
            "title": r.title,
            "reference": r.reference,
            "text": r.text,
        });
        writeln!(out, "{line}").map_err(e("stdout"))?;
    }
    Ok(())
}
