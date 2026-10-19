//! Smoke test against a real seforim.db.
//!
//! ```text
//! cargo run --release -p seforimdb-search --example smoke -- \
//!     <seforim.db> <index-dir> [max-lines] [word ...]
//! ```
//! Builds (or resumes) the index from the DB, capped at `max-lines` (0 = all),
//! then searches the words (AND, grammatical prefixes on) and prints the first
//! hits with their snippets rendered as plain text with [brackets] around the
//! marks.

use std::ops::ControlFlow;
use std::path::Path;
use std::time::Instant;

use hebrew_fts::WalkOptions;
use seforimdb_search::{build, search, BuildOptions, LineIndex, SearchRequest, SeforimDbCorpus, WordGroup};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: smoke <seforim.db> <index-dir> [max-lines] [word ...]");
        std::process::exit(2);
    }
    let db = Path::new(&args[0]);
    let index_dir = Path::new(&args[1]);
    let max_lines: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let words: Vec<String> = args.iter().skip(3).cloned().collect();

    let t = Instant::now();
    let mut last = 0u64;
    let outcome = build(
        db,
        index_dir,
        BuildOptions {
            max_lines: if max_lines > 0 { Some(max_lines) } else { None },
            on_progress: Some(Box::new(move |done, total| {
                if done - last >= 100_000 || done == total {
                    eprintln!("  indexed {done} / {total}");
                    last = done;
                }
            })),
            ..Default::default()
        },
    )?;
    eprintln!("build: {outcome:?} in {:.1?}", t.elapsed());

    if words.is_empty() {
        return Ok(());
    }
    let index = LineIndex::open(index_dir)?;
    let mut corpus = SeforimDbCorpus::open(db)?;
    let request = SearchRequest {
        groups: words
            .iter()
            .map(|w| WordGroup {
                word: w.clone(),
                options: WalkOptions {
                    grammar_prefixes: true,
                    ..Default::default()
                },
            })
            .collect(),
        ..Default::default()
    };

    let t = Instant::now();
    let mut shown = 0usize;
    let mut total = 0usize;
    let mut first_batch_at = None;
    let terms = search(&index, &mut corpus, &request, |batch| {
        first_batch_at.get_or_insert_with(|| t.elapsed());
        total += batch.hits.len();
        for hit in &batch.hits {
            if shown >= 10 {
                break;
            }
            shown += 1;
            let joined: Vec<&str> = hit
                .snippet_line_ids
                .iter()
                .filter_map(|id| batch.line_texts.get(id).map(|s| s.as_str()))
                .collect();
            let joined = joined.join(" ");
            println!(
                "line {} book {} toc {} dist {}: {}",
                hit.line_id,
                hit.book_id,
                hit.toc_entry_id,
                hit.word_distance,
                render(&joined, hit.snippet_start, hit.snippet_end, &hit.snippet_marks, hit.snippet_flags)
            );
        }
        ControlFlow::Continue(())
    })?;
    eprintln!(
        "search: {total} hits in {:.1?} (first batch after {:.1?}); matched words: {:?}",
        t.elapsed(),
        first_batch_at.unwrap_or_default(),
        terms.all()
    );
    Ok(())
}

/// Plain-text rendering of a descriptor over UTF-16 offsets: tags stripped,
/// marks in [brackets], ellipses from the flags.
fn render(text: &str, start: usize, end: usize, marks: &[u32], flags: u32) -> String {
    let u16s: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(u16s.len());
    let mut out = String::new();
    if flags & 1 != 0 {
        out.push('…');
    }
    let mut pos = start;
    for m in marks.chunks_exact(3) {
        let (s, l) = (m[0] as usize, m[1] as usize);
        if s < pos {
            continue;
        }
        out.push_str(&strip_tags(&String::from_utf16_lossy(&u16s[pos..s])));
        out.push('[');
        out.push_str(&strip_tags(&String::from_utf16_lossy(&u16s[s..(s + l).min(end)])));
        out.push(']');
        pos = (s + l).min(end);
    }
    out.push_str(&strip_tags(&String::from_utf16_lossy(&u16s[pos..end])));
    if flags & 2 != 0 {
        out.push('…');
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}
