//! A [`tantivy_fst::Automaton`] that matches exactly a finite set of byte
//! strings, as a byte trie.
//!
//! Driving tantivy's term-dictionary walk with it visits only the FST branches
//! that spell one of the set's strings — the walk IS the existence check, and
//! with a `MatchedTermsSink` attached the surviving forms come back with the
//! search.

use tantivy_fst::Automaton;

#[derive(Debug, Clone, Copy)]
struct TrieNode {
    /// Start of this node's edges in `edges`.
    edges_start: u32,
    /// Number of edges.
    edges_len: u32,
    /// This node completes one of the set's strings.
    is_match: bool,
}

/// Byte-trie automaton over a finite string set. Cheap to clone is not needed
/// (tantivy wraps automatons in `Arc`), but construction is O(total bytes).
#[derive(Debug)]
pub struct SetAutomaton {
    nodes: Vec<TrieNode>,
    /// Flat, per-node-sorted edge list: (byte, target node index).
    edges: Vec<(u8, u32)>,
    /// When true, the canonical quote byte `"` is transparent: from any
    /// state it is skipped (self-loop), so indexed terms match with or
    /// without geresh/gershayim (התעלם מגרשיים).
    transparent_quotes: bool,
}

impl SetAutomaton {
    /// Builds the trie from the given strings.
    pub fn from_strings<S: AsRef<str>>(strings: impl IntoIterator<Item = S>) -> SetAutomaton {
        // Build with per-node edge vecs first, then flatten.
        struct Building {
            edges: Vec<(u8, u32)>,
            is_match: bool,
        }
        let mut building: Vec<Building> = vec![Building {
            edges: Vec::new(),
            is_match: false,
        }];
        for s in strings {
            let mut node = 0usize;
            for &b in s.as_ref().as_bytes() {
                node = match building[node].edges.iter().find(|(eb, _)| *eb == b) {
                    Some(&(_, target)) => target as usize,
                    None => {
                        let target = building.len();
                        building[node].edges.push((b, target as u32));
                        building.push(Building {
                            edges: Vec::new(),
                            is_match: false,
                        });
                        target
                    }
                };
            }
            building[node].is_match = true;
        }
        let mut nodes = Vec::with_capacity(building.len());
        let mut edges = Vec::new();
        for b in &mut building {
            b.edges.sort_unstable_by_key(|&(byte, _)| byte);
            nodes.push(TrieNode {
                edges_start: edges.len() as u32,
                edges_len: b.edges.len() as u32,
                is_match: b.is_match,
            });
            edges.extend_from_slice(&b.edges);
        }
        SetAutomaton {
            nodes,
            edges,
            transparent_quotes: false,
        }
    }

    /// Makes the canonical quote byte `"` transparent (skipped from any
    /// state): רמבם matches the indexed רמב"ם and vice versa. The index's
    /// tokenizer normalizes all quote forms (" ' ׳ ״) to '"', so only this
    /// one byte needs skipping. Candidate strings should be quote-stripped
    /// when this is on.
    pub fn with_transparent_quotes(mut self) -> SetAutomaton {
        self.transparent_quotes = true;
        self
    }

    /// Root node index (for composite walks).
    pub(crate) const ROOT: u32 = 0;

    /// Whether `node` completes one of the set's strings.
    #[inline]
    pub(crate) fn node_is_match(&self, node: u32) -> bool {
        self.nodes[node as usize].is_match
    }

    #[inline]
    pub(crate) fn step(&self, node: u32, byte: u8) -> Option<u32> {
        let n = self.nodes[node as usize];
        let edges =
            &self.edges[n.edges_start as usize..(n.edges_start + n.edges_len) as usize];
        edges
            .binary_search_by_key(&byte, |&(b, _)| b)
            .ok()
            .map(|i| edges[i].1)
    }
}

impl Automaton for SetAutomaton {
    /// Current trie node; `None` is the dead state.
    type State = Option<u32>;

    fn start(&self) -> Option<u32> {
        Some(0)
    }

    fn is_match(&self, state: &Option<u32>) -> bool {
        state.is_some_and(|node| self.nodes[node as usize].is_match)
    }

    fn accept(&self, state: &Option<u32>, byte: u8) -> Option<u32> {
        state.and_then(|node| {
            self.step(node, byte).or({
                if self.transparent_quotes && byte == b'"' {
                    Some(node)
                } else {
                    None
                }
            })
        })
    }

    fn can_match(&self, state: &Option<u32>) -> bool {
        state.is_some()
    }

    fn will_always_match(&self, _state: &Option<u32>) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(a: &SetAutomaton, s: &str) -> bool {
        let mut state = a.start();
        for &b in s.as_bytes() {
            if !a.can_match(&state) {
                return false;
            }
            state = a.accept(&state, b);
        }
        a.is_match(&state)
    }

    #[test]
    fn exact_set_matching() {
        let a = SetAutomaton::from_strings(["תורה", "התורה", "תורות"]);
        assert!(matches(&a, "תורה"));
        assert!(matches(&a, "התורה"));
        assert!(matches(&a, "תורות"));
        assert!(!matches(&a, "תור")); // prefix of a member is not a match
        assert!(!matches(&a, "תורהה"));
        assert!(!matches(&a, "בתורה"));
        assert!(!matches(&a, ""));
    }

    #[test]
    fn dead_state_stays_dead() {
        let a = SetAutomaton::from_strings(["אב"]);
        let s = a.start();
        let s = a.accept(&s, b'x'); // no such edge
        assert!(!a.can_match(&s));
        assert_eq!(a.accept(&s, b'x'), None);
    }

    #[test]
    fn empty_string_member() {
        let a = SetAutomaton::from_strings([""]);
        assert!(matches(&a, ""));
        assert!(!matches(&a, "א"));
    }
}
