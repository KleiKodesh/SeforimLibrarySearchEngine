// Ported verbatim from otzaria_search_engine 7.2.0 (MIT, Copyright (c) 2026 otzaria),
// rust/src/gap_phrase.rs; see THIRD-PARTY-NOTICES.md. Changes: module paths, the test
// module dropped (golden tests live in hebrew-fts/tests/otzaria_helpers.rs), only ChainSweep / slack / Proximity / WordWindows kept (no tantivy queries).

pub(crate) struct ChainSweep {
    feasible: Vec<u32>,
    next: Vec<u32>,
}

impl ChainSweep {
    pub(crate) fn start(&mut self, first_word: &[u32]) {
        self.feasible.clear();
        self.feasible.extend_from_slice(first_word);
    }

    /// Keeps the positions of the next word reachable from a chain end with
    /// at most `gap` intermediate words; false when none is.
    pub(crate) fn extend(&mut self, positions: &[u32], gap: u32) -> bool {
        // q extends a chain iff some feasible p satisfies
        // q - gap - 1 <= p <= q - 1 (strictly after p, within the gap).
        let window = gap as u64 + 1;
        self.next.clear();
        let mut j = 0usize;
        for &q in positions {
            let lo = (q as u64).saturating_sub(window);
            while j < self.feasible.len() && (self.feasible[j] as u64) < lo {
                j += 1;
            }
            if j < self.feasible.len() && self.feasible[j] < q {
                self.next.push(q);
            }
        }
        std::mem::swap(&mut self.feasible, &mut self.next);
        !self.feasible.is_empty()
    }

    pub(crate) fn ends(&self) -> &[u32] {
        &self.feasible
    }

    /// Runs the whole chain; false as soon as a word cannot extend it.
    fn chain(&mut self, words: &[Vec<u32>], gaps: &[u32]) -> bool {
        let Some((first, rest)) = words.split_first() else {
            return false;
        };
        self.start(first);
        !self.feasible.is_empty()
            && rest
                .iter()
                .zip(gaps)
                .all(|(positions, &gap)| self.extend(positions, gap))
    }
}

/// The anchoring check of a phrase that ends a line: the fewest words between
/// a chain's last word and position `last` (the line's last content word),
/// when that is at most `max_slack`. `words` holds each word's sorted positions.
pub(crate) fn end_anchored_slack(
    sweep: &mut ChainSweep,
    words: &[Vec<u32>],
    gaps: &[u32],
    last: u32,
    max_slack: u32,
) -> Option<u32> {
    if !sweep.chain(words, gaps) {
        return None;
    }
    let end = sweep.ends().iter().rev().find(|&&end| end <= last)?;
    let slack = last - end;
    (slack <= max_slack).then_some(slack)
}

/// The anchoring check of a phrase that starts a line: the fewest words
/// between position `first` (the line's first content word) and a chain's first
/// word, when that is at most `max_slack`.
pub(crate) fn start_anchored_slack(
    sweep: &mut ChainSweep,
    words: &[Vec<u32>],
    gaps: &[u32],
    first: u32,
    max_slack: u32,
) -> Option<u32> {
    let (head, rest) = words.split_first()?;
    let window_end = first.saturating_add(max_slack);
    head.iter()
        .filter(|&&start| start >= first && start <= window_end)
        .find(|&&start| {
            sweep.start(&[start]);
            rest.iter()
                .zip(gaps)
                .all(|(positions, &gap)| sweep.extend(positions, gap))
        })
        .map(|&start| start - first)
}

/// How the words of a multi-word query must lie relative to each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Proximity {
    /// In query order; `gaps[i]` = allowed intermediate words between words
    /// `i` and `i+1`.
    Ordered(Vec<u32>),
    /// In any order, each word at its own position, the last position at most
    /// `max_span` after the first.
    AnyOrder { max_span: u32 },
}

impl Proximity {
    /// The span an ordered match of `word_count` words, with at most `distance`
    /// words between each pair, can take — so any order never loses an ordered hit.
    pub(crate) fn any_order(word_count: usize, distance: u32) -> Self {
        let pairs = u32::try_from(word_count.saturating_sub(1)).unwrap_or(u32::MAX);
        Self::AnyOrder {
            max_span: pairs.saturating_mul(distance.saturating_add(1)),
        }
    }

    /// Strictly adjacent, in order.
    pub(crate) fn adjacent(word_count: usize) -> Self {
        Self::Ordered(vec![0; word_count.saturating_sub(1)])
    }

    pub(crate) fn is_any_order(&self) -> bool {
        matches!(self, Self::AnyOrder { .. })
    }
}

/// Windows of word occurrences for [`Proximity::AnyOrder`]. Fed in increasing
/// position order with the query words each position can fill; finds windows
/// where every word gets a distinct position. Word sets are lists, not bit
/// masks, so any word count works.
#[derive(Default)]
pub(crate) struct WordWindows {
    word_count: usize,
    positions: Vec<u32>,
    /// Entry `e` holds `words[starts[e]..starts[e + 1]]`.
    starts: Vec<usize>,
    words: Vec<usize>,
    coverage: Vec<u32>,
    adjacency: Vec<Vec<usize>>,
    owner: Vec<usize>,
    visited: Vec<bool>,
    stack: Vec<(usize, usize)>,
}

const UNMATCHED: usize = usize::MAX;

impl WordWindows {
    pub(crate) fn reset(&mut self, word_count: usize) {
        self.word_count = word_count;
        self.positions.clear();
        self.starts.clear();
        self.starts.push(0);
        self.words.clear();
    }

    /// Records that `word` can fill `position`, which must not precede the
    /// previous one. Returns the new slot (see [`Self::slot`]), `None` for a repeat.
    pub(crate) fn push(&mut self, position: u32, word: usize) -> Option<usize> {
        debug_assert!(word < self.word_count);
        if self.positions.last() != Some(&position) {
            debug_assert!(self.positions.last().is_none_or(|&last| last < position));
            self.positions.push(position);
            self.starts.push(self.words.len());
        }
        let begin = self.starts[self.starts.len() - 2];
        if self.words[begin..].contains(&word) {
            return None;
        }
        self.words.push(word);
        *self.starts.last_mut().unwrap() = self.words.len();
        Some(self.words.len() - 1)
    }

    /// The slot [`Self::push`] returned for `word` at entry `entry`.
    pub(crate) fn slot(&self, entry: usize, word: usize) -> usize {
        let begin = self.starts[entry];
        begin
            + self.words[begin..self.starts[entry + 1]]
                .iter()
                .position(|&w| w == word)
                .expect("an assigned word fills its entry")
    }

    /// Last minus first position of an assignment from [`Self::next_window`].
    pub(crate) fn assigned_span(&self, assignment: &[usize]) -> u32 {
        let first = assignment.iter().min().copied().unwrap_or(0);
        let last = assignment.iter().max().copied().unwrap_or(0);
        self.positions[last] - self.positions[first]
    }

    /// The window ending earliest, among entries `from..`, whose positions span
    /// at most `max_span` and give every word a distinct entry. On success
    /// `assignment[w]` is word `w`'s entry (the latest ones are preferred, for a
    /// tight occurrence) and the window's last entry is returned.
    pub(crate) fn next_window(
        &mut self,
        from: usize,
        max_span: u32,
        assignment: &mut Vec<usize>,
    ) -> Option<usize> {
        let k = self.word_count;
        if k == 0 {
            return None;
        }
        self.coverage.clear();
        self.coverage.resize(k, 0);
        let mut covered = 0;
        // Entries several words can fill; without any, coverage implies a matching.
        let mut shared = 0;
        let mut left = from;
        for right in from..self.positions.len() {
            let entry = &self.words[self.starts[right]..self.starts[right + 1]];
            shared += usize::from(entry.len() > 1);
            for &w in entry {
                covered += usize::from(self.coverage[w] == 0);
                self.coverage[w] += 1;
            }
            while self.positions[right] - self.positions[left] > max_span {
                let entry = &self.words[self.starts[left]..self.starts[left + 1]];
                shared -= usize::from(entry.len() > 1);
                for &w in entry {
                    self.coverage[w] -= 1;
                    covered -= usize::from(self.coverage[w] == 0);
                }
                left += 1;
            }
            if covered < k || right + 1 - left < k {
                continue;
            }
            if shared == 0 {
                assignment.clear();
                assignment.resize(k, UNMATCHED);
                for e in (left..=right).rev() {
                    let w = self.words[self.starts[e]];
                    if assignment[w] == UNMATCHED {
                        assignment[w] = e;
                    }
                }
                return Some(right);
            }
            if self.match_window(left, right, assignment) {
                return Some(right);
            }
        }
        None
    }

    /// Kuhn's bipartite matching of the words to the entries `left..=right`.
    fn match_window(&mut self, left: usize, right: usize, assignment: &mut Vec<usize>) -> bool {
        let k = self.word_count;
        let n = right + 1 - left;
        self.adjacency.resize_with(k, Vec::new);
        self.adjacency.iter_mut().for_each(Vec::clear);
        for e in (left..=right).rev() {
            for &w in &self.words[self.starts[e]..self.starts[e + 1]] {
                self.adjacency[w].push(e - left);
            }
        }
        self.owner.clear();
        self.owner.resize(n, UNMATCHED);
        for w in 0..k {
            self.visited.clear();
            self.visited.resize(n, false);
            if !self.augment(w) {
                return false;
            }
        }
        assignment.clear();
        assignment.resize(k, UNMATCHED);
        for (i, &w) in self.owner.iter().enumerate() {
            if w != UNMATCHED {
                assignment[w] = left + i;
            }
        }
        true
    }

    /// An augmenting path from `start`; iterative, so a long query cannot
    /// exhaust the stack. Each frame takes the entry it tried last.
    fn augment(&mut self, start: usize) -> bool {
        self.stack.clear();
        self.stack.push((start, 0));
        while let Some(&(w, i)) = self.stack.last() {
            let Some(&e) = self.adjacency[w].get(i) else {
                self.stack.pop();
                continue;
            };
            self.stack.last_mut().unwrap().1 += 1;
            if self.visited[e] {
                continue;
            }
            self.visited[e] = true;
            let holder = self.owner[e];
            if holder == UNMATCHED {
                for &(word, next) in &self.stack {
                    self.owner[self.adjacency[word][next - 1]] = word;
                }
                return true;
            }
            self.stack.push((holder, 0));
        }
        false
    }
}

