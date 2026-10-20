//! Port of FtsLib `FmIndex.cs`: an FM-index over the term dictionary for
//! infix (`*x*`), suffix (`*x`) and fuzzy (Levenshtein) lookups. Returns
//! EXACT matching term rowids. Read through a memory map.
//!
//! Text: `SEP t0 SEP t1 ... SEP t(k-1) SEP END`, one code per char
//! (END=0, SEP=1, real chars 2.. in code-point order).
//!
//! Suffix order: compare codes up to and including the first SEP/END
//! ("stop"); if equal, by text position. The position tie-break keeps
//! backward search valid when the pattern ends with SEP (suffix and fuzzy
//! queries), and backward search never steps through any other SEP. The
//! one place LF order and this order differ is the SEP range itself (rows
//! "SEP t ..." sorted by position, not by t), so nothing ever maps INTO it:
//! a fuzzy match reads its term off the term-start row (see `FuzzySearch`).
//!
//! Parts:
//! * BWT as a Huffman-shaped binary wavelet tree: one bitvector per internal
//!   node, each split into 64-byte rank blocks = u32 ones-before-block + 480
//!   data bits.
//! * Row → rowid section: for every suffix-array row, the rowid of the term
//!   its suffix starts in, bit-packed. A match is a contiguous row range →
//!   one sequential pass.
//!
//! Differences from FtsLib, both deliberate:
//! * Here the terms handed in are the dictionary's QUOTE-STRIPPED forms (FtsLib's
//!   terms carried no quotes), so two dictionary terms (רשי, רש"י) may share
//!   one text form. Duplicates are therefore allowed, and a fuzzy match reads
//!   its rowids off the row → rowid section (FtsLib used a rank → rowid
//!   table, which assumes the text forms sort like the rowids; stripped
//!   forms do not).
//! * Rowid = dictionary ordinal + 1; the rank → rowid table is never written
//!   (flag bit 0 always set).
//!
//! File layout (little-endian), identical to FtsLib's 'FMI3':
//!   header  : magic u32, textLength u32, sigma u32, termCount u32,
//!             internalNodes u32, rowIdBits u32, bitvectorBytes u64,
//!             flags u32 (bit 0: rowid == rank + 1), reserved u32      [40 B]
//!   chars   : sigma x u16 (code -> char; 0 for END/SEP)
//!   C       : (sigma + 1) x u32
//!   nodes   : internalNodes x { left i32, right i32, bitLength u32, pad u32, offset u64 }
//!             child >= 0 = internal node index, child < 0 = leaf symbol -(code+1)
//!   bitvectors : bitvectorBytes
//!   rowIds  : textLength x rowIdBits, packed, + 8 zero bytes of padding

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// 'FMI3'
pub const MAGIC: u32 = 0x3349_4D46;
const END_CODE: u8 = 0;
const SEPARATOR_CODE: u8 = 1;
const HEADER_BYTES: usize = 40;
const NODE_BYTES: usize = 24;
const RANK_BLOCK_BYTES: usize = 64;
const RANK_BLOCK_DATA_BITS: usize = (RANK_BLOCK_BYTES - 4) * 8; // 480
const CODES_PER_SORT_KEY: usize = 10; // 6 bits each → 60 bits
const FLAG_ROWID_IS_RANK_PLUS_ONE: u32 = 1;

/// Why an FM-index could not be built for a dictionary.
#[derive(Debug, Clone)]
pub struct FmBuildError(pub String);

impl std::fmt::Display for FmBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FmIndex: {}", self.0)
    }
}

// ── Build ─────────────────────────────────────────────────────────────

/// Builds the index bytes over `terms` (the text forms, in dictionary order;
/// term i has rowid i + 1). Every term must be non-empty BMP text; at most
/// 64 distinct chars (6-bit sort keys), as in FtsLib.
pub fn build<'a>(terms: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>, FmBuildError> {
    let terms: Vec<&str> = terms.into_iter().collect();

    // Alphabet in code-point order (code order == ordinal for BMP text).
    let mut present = vec![false; 65536];
    for t in &terms {
        if t.is_empty() {
            return Err(FmBuildError("empty term".into()));
        }
        for ch in t.chars() {
            let u = ch as u32;
            if u >= 65536 {
                return Err(FmBuildError("non-BMP char".into()));
            }
            present[u as usize] = true;
        }
    }
    let mut code_of = vec![0u8; 65536];
    let mut char_of: Vec<u16> = vec![0, 0];
    for ch in 0..65536usize {
        if present[ch] {
            if char_of.len() >= 256 {
                return Err(FmBuildError("sigma > 255".into()));
            }
            code_of[ch] = char_of.len() as u8;
            char_of.push(ch as u16);
        }
    }
    let sigma = char_of.len();
    if sigma > 64 {
        return Err(FmBuildError(format!("sigma {sigma} > 64 (6-bit sort keys)")));
    }

    // Text, plus the rowid of the term each position belongs to. A SEP
    // carries the FOLLOWING term's rowid (never read: every match range
    // starts at a term character; FtsLib stored 0 there), END carries 0.
    let mut text_len = 2usize;
    for t in &terms {
        text_len += t.chars().count() + 1;
    }
    if text_len > i32::MAX as usize {
        return Err(FmBuildError("text too long".into()));
    }
    let n = text_len;
    let mut text = vec![0u8; n];
    let mut rowid_at = vec![0u32; n];
    let mut max_rowid = 0u32;
    let mut w = 0usize;
    for (i, t) in terms.iter().enumerate() {
        let rowid = (i + 1) as u32;
        max_rowid = max_rowid.max(rowid);
        text[w] = SEPARATOR_CODE;
        rowid_at[w] = rowid;
        w += 1;
        for ch in t.chars() {
            rowid_at[w] = rowid;
            text[w] = code_of[ch as usize];
            w += 1;
        }
    }
    text[w] = SEPARATOR_CODE;
    w += 1;
    text[w] = END_CODE;

    let suffix_array = build_suffix_array(&text);

    let mut bwt = vec![0u8; n];
    for (row, &p) in suffix_array.iter().enumerate() {
        bwt[row] = text[if p == 0 { n - 1 } else { p - 1 }];
    }

    let mut symbol_count = vec![0i64; sigma];
    for &b in &text {
        symbol_count[b as usize] += 1;
    }
    let mut c = vec![0u32; sigma + 1];
    for s in 0..sigma {
        c[s + 1] = c[s] + symbol_count[s] as u32;
    }

    // Huffman-shaped wavelet tree.
    let tree = HuffmanTree::build(&symbol_count);
    let mut node_bits: Vec<BitWriter> = (0..tree.left.len()).map(|_| BitWriter::default()).collect();
    for &sym in &bwt {
        let nodes = &tree.path_nodes[sym as usize];
        let bits = &tree.path_bits[sym as usize];
        for d in 0..nodes.len() {
            node_bits[nodes[d]].append(bits[d]);
        }
    }

    let rowid_bits = 32 - max_rowid.leading_zeros() as usize;

    // Serialize.
    let mut node_offsets = vec![0u64; node_bits.len()];
    let mut bitvector_bytes = 0u64;
    for (i, bits) in node_bits.iter().enumerate() {
        node_offsets[i] = bitvector_bytes;
        bitvector_bytes += (bits.count / RANK_BLOCK_DATA_BITS as u64 + 1) * RANK_BLOCK_BYTES as u64;
    }

    let mut out = Vec::with_capacity(HEADER_BYTES + sigma * 2 + (sigma + 1) * 4 + node_bits.len() * NODE_BYTES
        + bitvector_bytes as usize + (n * rowid_bits + 7) / 8 + 8);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(sigma as u32).to_le_bytes());
    out.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    out.extend_from_slice(&(node_bits.len() as u32).to_le_bytes());
    out.extend_from_slice(&(rowid_bits as u32).to_le_bytes());
    out.extend_from_slice(&bitvector_bytes.to_le_bytes());
    out.extend_from_slice(&FLAG_ROWID_IS_RANK_PLUS_ONE.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for ch in &char_of {
        out.extend_from_slice(&ch.to_le_bytes());
    }
    for v in &c {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for i in 0..node_bits.len() {
        out.extend_from_slice(&tree.left[i].to_le_bytes());
        out.extend_from_slice(&tree.right[i].to_le_bytes());
        out.extend_from_slice(&(node_bits[i].count as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&node_offsets[i].to_le_bytes());
    }
    for bits in &node_bits {
        write_rank_blocks(&mut out, bits);
    }

    let mut packed = BitWriter::default();
    for &p in &suffix_array {
        packed.append_value(rowid_at[p], rowid_bits);
    }
    out.extend_from_slice(&packed.to_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // padding: the reader decodes with 8-byte loads
    Ok(out)
}

fn write_rank_blocks(out: &mut Vec<u8>, bits: &BitWriter) {
    let blocks = bits.count / RANK_BLOCK_DATA_BITS as u64 + 1;
    let mut ones_before = 0u32;
    for b in 0..blocks {
        let mut data = [0u8; RANK_BLOCK_BYTES - 4];
        let start = b * RANK_BLOCK_DATA_BITS as u64;
        let end = bits.count.min(start + RANK_BLOCK_DATA_BITS as u64);
        let mut ones = 0u32;
        // 64 bits at a time: the block's 480 data bits are 7 full words and a half.
        let mut k = 0usize;
        while start + (k as u64) * 64 < end {
            let width = (end - (start + k as u64 * 64)).min(64) as usize;
            let word = bits.get_bits(start + k as u64 * 64, width);
            ones += word.count_ones();
            for byte_index in 0..8 {
                if k * 8 + byte_index < data.len() {
                    data[k * 8 + byte_index] = (word >> (byte_index * 8)) as u8;
                }
            }
            k += 1;
        }
        out.extend_from_slice(&ones_before.to_le_bytes());
        out.extend_from_slice(&data);
        ones_before += ones;
    }
}

#[derive(Default)]
struct BitWriter {
    words: Vec<u64>,
    count: u64,
}

impl BitWriter {
    fn ensure(&mut self, bit_index: u64) {
        let need = (bit_index >> 6) as usize + 2;
        if self.words.len() < need {
            self.words.resize(need.max(self.words.len() * 2).max(1024), 0);
        }
    }

    fn append(&mut self, bit: bool) {
        self.ensure(self.count);
        if bit {
            self.words[(self.count >> 6) as usize] |= 1u64 << (self.count & 63);
        }
        self.count += 1;
    }

    /// Appends the low `bits` bits of `value` (bits ≤ 32).
    fn append_value(&mut self, value: u32, bits: usize) {
        self.ensure(self.count + bits as u64);
        let v = (value as u64) & ((1u64 << bits) - 1);
        let offset = (self.count & 63) as usize;
        let w = (self.count >> 6) as usize;
        self.words[w] |= v << offset;
        if offset + bits > 64 {
            self.words[w + 1] |= v >> (64 - offset);
        }
        self.count += bits as u64;
    }

    /// `width` bits (1..=64) starting at bit `start`, LSB first.
    fn get_bits(&self, start: u64, width: usize) -> u64 {
        let offset = (start & 63) as usize;
        let w = (start >> 6) as usize;
        let mut value = self.words.get(w).copied().unwrap_or(0) >> offset;
        if offset != 0 {
            if let Some(&next) = self.words.get(w + 1) {
                value |= next << (64 - offset);
            }
        }
        if width == 64 {
            value
        } else {
            value & ((1u64 << width) - 1)
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let len = ((self.count + 7) / 8) as usize;
        let mut bytes = vec![0u8; len];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (self.words.get(i >> 3).copied().unwrap_or(0) >> ((i & 7) * 8)) as u8;
        }
        bytes
    }
}

/// Huffman tree over the codes; internal nodes indexed 0..sigma-2, root last.
struct HuffmanTree {
    left: Vec<i32>,
    right: Vec<i32>,
    root: usize,
    path_nodes: Vec<Vec<usize>>,
    path_bits: Vec<Vec<bool>>,
}

impl HuffmanTree {
    fn build(frequency: &[i64]) -> HuffmanTree {
        let mut t = HuffmanTree {
            left: Vec::new(),
            right: Vec::new(),
            root: 0,
            path_nodes: Vec::new(),
            path_bits: Vec::new(),
        };
        // (weight, tie-break, child encoding): smallest first.
        let mut queue: BinaryHeap<Reverse<(i64, u32, i32)>> = BinaryHeap::new();
        let mut tie = 0u32;
        for (s, &f) in frequency.iter().enumerate() {
            queue.push(Reverse((f.max(1), tie, -(s as i32 + 1))));
            tie += 1;
        }
        while queue.len() > 1 {
            let Reverse((wa, _, a)) = queue.pop().unwrap();
            let Reverse((wb, _, b)) = queue.pop().unwrap();
            t.left.push(a);
            t.right.push(b);
            queue.push(Reverse((wa + wb, tie, (t.left.len() - 1) as i32)));
            tie += 1;
        }
        t.root = t.left.len() - 1;
        t.compute_paths(frequency.len());
        t
    }

    fn from_parts(left: Vec<i32>, right: Vec<i32>, sigma: usize) -> HuffmanTree {
        let root = left.len() - 1;
        let mut t = HuffmanTree {
            left,
            right,
            root,
            path_nodes: Vec::new(),
            path_bits: Vec::new(),
        };
        t.compute_paths(sigma);
        t
    }

    fn compute_paths(&mut self, sigma: usize) {
        self.path_nodes = vec![Vec::new(); sigma];
        self.path_bits = vec![Vec::new(); sigma];
        let mut nodes = Vec::new();
        let mut bits = Vec::new();
        self.walk(self.root, &mut nodes, &mut bits);
    }

    fn walk(&mut self, node: usize, nodes: &mut Vec<usize>, bits: &mut Vec<bool>) {
        for side in 0..2 {
            let child = if side == 0 { self.left[node] } else { self.right[node] };
            nodes.push(node);
            bits.push(side == 1);
            if child < 0 {
                let sym = (-child - 1) as usize;
                self.path_nodes[sym] = nodes.clone();
                self.path_bits[sym] = bits.clone();
            } else {
                self.walk(child as usize, nodes, bits);
            }
            nodes.pop();
            bits.pop();
        }
    }
}

/// Sorts all suffixes under the "compare up to the first stop, then by
/// position" order. Terms are short, so a packed 10-code key settles almost
/// every comparison; equal-key runs are finished with a direct comparison.
fn build_suffix_array(text: &[u8]) -> Vec<usize> {
    let n = text.len();
    let mut distance_to_stop = vec![0usize; n]; // codes from i up to and including the first SEP/END
    for i in (0..n).rev() {
        distance_to_stop[i] = if text[i] <= SEPARATOR_CODE {
            1
        } else {
            distance_to_stop[i + 1] + 1
        };
    }

    let mut keyed: Vec<(u64, usize)> = Vec::with_capacity(n);
    for i in 0..n {
        let len = distance_to_stop[i].min(CODES_PER_SORT_KEY);
        let mut key = 0u64;
        for k in 0..len {
            key |= (text[i + k] as u64) << (6 * (CODES_PER_SORT_KEY - 1 - k));
        }
        keyed.push((key, i));
    }
    keyed.sort_unstable_by_key(|&(k, _)| k);

    let mut suffix_array: Vec<usize> = keyed.iter().map(|&(_, i)| i).collect();
    let keys: Vec<u64> = keyed.iter().map(|&(k, _)| k).collect();

    let tail = |a: &usize, b: &usize| -> std::cmp::Ordering {
        let (la, lb) = (distance_to_stop[*a], distance_to_stop[*b]);
        let common = la.min(lb);
        for k in CODES_PER_SORT_KEY..common {
            match text[a + k].cmp(&text[b + k]) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
        }
        if la != lb {
            la.cmp(&lb)
        } else {
            a.cmp(b)
        }
    };

    let mut run_start = 0usize;
    while run_start < n {
        let mut run_end = run_start + 1;
        while run_end < n && keys[run_end] == keys[run_start] {
            run_end += 1;
        }
        if run_end - run_start > 1 {
            let stop_inside_key = distance_to_stop[suffix_array[run_start]] <= CODES_PER_SORT_KEY;
            let run = &mut suffix_array[run_start..run_end];
            if stop_inside_key {
                run.sort_unstable();
            } else {
                run.sort_unstable_by(tail);
            }
        }
        run_start = run_end;
    }
    suffix_array
}

// ── Read ───────────────────────────────────────────────────────────────

/// An opened FM-index (over a memory map or an owned buffer).
pub struct FmIndex {
    data: Box<dyn AsRef<[u8]> + Send + Sync>,
    text_length: usize,
    term_count: u32,
    rowid_bits: usize,
    c: Vec<u32>,
    code_of: Vec<u8>, // 65536 entries; 0 = char not in alphabet
    left: Vec<i32>,
    right: Vec<i32>,
    root: usize,
    node_offset: Vec<u64>, // absolute file offset of each node's rank blocks
    path_nodes: Vec<Vec<usize>>,
    path_bits: Vec<Vec<bool>>,
    rowids_offset: u64,
}

impl FmIndex {
    /// Opens from any byte container (an `Mmap`, a `Vec<u8>`).
    pub fn open(data: Box<dyn AsRef<[u8]> + Send + Sync>) -> Result<FmIndex, FmBuildError> {
        let bytes: &[u8] = (*data).as_ref();
        if bytes.len() < HEADER_BYTES {
            return Err(FmBuildError("file too short".into()));
        }
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        if u32_at(0) != MAGIC {
            return Err(FmBuildError("bad magic".into()));
        }
        let text_length = u32_at(4) as usize;
        let sigma = u32_at(8) as usize;
        let term_count = u32_at(12);
        let internal_nodes = u32_at(16) as usize;
        let rowid_bits = u32_at(20) as usize;
        let bitvector_bytes = u64_at(24);
        let _flags = u32_at(32);
        // A corrupt or truncated file must be an Err, never a panic.
        let tables = HEADER_BYTES as u64
            + 2 * sigma as u64
            + 4 * (sigma as u64 + 1)
            + NODE_BYTES as u64 * internal_nodes as u64;
        if !(2..=256).contains(&sigma)
            || internal_nodes + 1 != sigma
            || !(1..=32).contains(&rowid_bits)
            || tables + bitvector_bytes > bytes.len() as u64
        {
            return Err(FmBuildError("corrupt header".into()));
        }

        let mut pos = HEADER_BYTES;
        let mut code_of = vec![0u8; 65536];
        for code in 0..sigma {
            let ch = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
            if code as u8 > SEPARATOR_CODE {
                code_of[ch as usize] = code as u8;
            }
            pos += 2;
        }
        let mut c = vec![0u32; sigma + 1];
        for v in c.iter_mut() {
            *v = u32_at(pos);
            pos += 4;
        }
        let bitvectors_start = pos as u64 + (internal_nodes * NODE_BYTES) as u64;
        let mut left = Vec::with_capacity(internal_nodes);
        let mut right = Vec::with_capacity(internal_nodes);
        let mut node_offset = Vec::with_capacity(internal_nodes);
        for _ in 0..internal_nodes {
            left.push(u32_at(pos) as i32);
            right.push(u32_at(pos + 4) as i32);
            let offset = u64_at(pos + 16);
            if offset >= bitvector_bytes {
                return Err(FmBuildError("corrupt node table".into()));
            }
            node_offset.push(bitvectors_start + offset);
            pos += NODE_BYTES;
        }
        if left
            .iter()
            .chain(right.iter())
            .any(|&c| c >= internal_nodes as i32 || c < -(sigma as i32))
        {
            return Err(FmBuildError("corrupt node table".into()));
        }
        let tree = HuffmanTree::from_parts(left, right, sigma);
        let rowids_offset = bitvectors_start + bitvector_bytes;
        let packed_bytes = (text_length as u64 * rowid_bits as u64 + 7) / 8;
        if (rowids_offset + packed_bytes + 8) as usize > bytes.len() {
            return Err(FmBuildError("file truncated".into()));
        }
        Ok(FmIndex {
            text_length,
            term_count,
            rowid_bits,
            c,
            code_of,
            root: tree.root,
            left: tree.left,
            right: tree.right,
            node_offset,
            path_nodes: tree.path_nodes,
            path_bits: tree.path_bits,
            rowids_offset,
            data,
        })
    }

    /// Number of terms the index was built over.
    pub fn term_count(&self) -> u32 {
        self.term_count
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        (*self.data).as_ref()
    }

    #[inline]
    fn u32_at(&self, o: u64) -> u32 {
        let b = self.bytes();
        u32::from_le_bytes(b[o as usize..o as usize + 4].try_into().unwrap())
    }

    /// 8-byte little-endian load, zero-padded past the end.
    #[inline]
    fn u64_at(&self, o: u64) -> u64 {
        let b = self.bytes();
        let o = o as usize;
        if o + 8 <= b.len() {
            u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
        } else {
            let mut buf = [0u8; 8];
            let avail = b.len().saturating_sub(o);
            buf[..avail].copy_from_slice(&b[o..o + avail]);
            u64::from_le_bytes(buf)
        }
    }

    /// Ones in the node's bitvector before position i: one 64-byte block.
    fn rank1(&self, node: usize, i: usize) -> usize {
        let block = i / RANK_BLOCK_DATA_BITS;
        let within = i - block * RANK_BLOCK_DATA_BITS;
        let b = self.node_offset[node] + (block * RANK_BLOCK_BYTES) as u64;
        let mut ones = self.u32_at(b) as usize;
        let words = b + 4;
        let full_words = within >> 6;
        for k in 0..full_words {
            ones += self.u64_at(words + (k * 8) as u64).count_ones() as usize;
        }
        let rest = within & 63;
        if rest != 0 {
            ones += (self.u64_at(words + (full_words * 8) as u64) & ((1u64 << rest) - 1)).count_ones() as usize;
        }
        ones
    }

    /// Occurrences of `code` in BWT[0, row): one rank per code bit.
    fn occ(&self, code: u8, mut row: usize) -> usize {
        let nodes = &self.path_nodes[code as usize];
        let bits = &self.path_bits[code as usize];
        for d in 0..nodes.len() {
            let ones = self.rank1(nodes[d], row);
            row = if bits[d] { ones } else { row - ones };
        }
        row
    }

    #[inline]
    fn rowid_at_row(&self, row: usize) -> u32 {
        let mask = (1u64 << self.rowid_bits) - 1;
        let bit = row as u64 * self.rowid_bits as u64;
        ((self.u64_at(self.rowids_offset + (bit >> 3)) >> (bit & 7)) & mask) as u32
    }

    /// Backward search. None when the pattern does not occur.
    fn find_row_range(&self, pattern: &[char], must_end_term: bool) -> Option<(usize, usize)> {
        let (mut first, mut end) = (0usize, self.text_length);
        let start_k = pattern.len() as isize - if must_end_term { 0 } else { 1 };
        let mut k = start_k;
        while k >= 0 {
            let code = if k as usize == pattern.len() {
                SEPARATOR_CODE // only when must_end_term
            } else {
                let ch = pattern[k as usize] as u32;
                let code = if ch < 65536 { self.code_of[ch as usize] } else { 0 };
                if code == 0 {
                    return None;
                }
                code
            };
            first = self.c[code as usize] as usize + self.occ(code, first);
            end = self.c[code as usize] as usize + self.occ(code, end);
            if first >= end {
                return None;
            }
            k -= 1;
        }
        Some((first, end))
    }

    /// Sorted distinct rowids of terms containing `pattern` (`must_end_term`:
    /// terms ending with it). Backward search, then one pass over the packed
    /// row → rowid entries of the whole match range.
    pub fn find_row_ids(&self, pattern: &str, must_end_term: bool) -> Vec<u32> {
        let chars: Vec<char> = pattern.chars().collect();
        if chars.is_empty() {
            return Vec::new();
        }
        let Some((first, end)) = self.find_row_range(&chars, must_end_term) else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = (first..end).map(|row| self.rowid_at_row(row)).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    // ── Fuzzy ─────────────────────────────────────────────────────────

    /// Every term within Levenshtein distance `max_distance` of `query`, as
    /// (rowid, distance), sorted by rowid.
    ///
    /// Backward search from SEP walks a trie of the REVERSED terms: the range
    /// for the string "x SEP" holds every term ending with x, and its children
    /// are the distinct BWT symbols in that range (found by descending the
    /// wavelet tree once). One Levenshtein DP row per trie level, against the
    /// reversed query; a branch dies when its row minimum exceeds the limit.
    /// A SEP child means x itself is a term.
    pub fn find_fuzzy(&self, query: &str, max_distance: usize) -> Vec<(u32, u32)> {
        let qchars: Vec<char> = query.chars().collect();
        let m = qchars.len();
        let reversed: Vec<u8> = (0..m)
            .map(|j| {
                let ch = qchars[m - 1 - j] as u32;
                let code = if ch < 65536 { self.code_of[ch as usize] } else { 0 };
                if code == 0 {
                    255 // never equals a BWT symbol
                } else {
                    code
                }
            })
            .collect();
        let mut search = FuzzySearch {
            fm: self,
            query: reversed,
            max_distance,
            m,
            results: Vec::new(),
            children: vec![Vec::new(); m + max_distance + 2],
            rows: vec![vec![0usize; m + 1]; m + max_distance + 3],
        };
        let first_row: Vec<usize> = (0..=m).collect();
        let sep = SEPARATOR_CODE as usize;
        search.visit(self.c[sep] as usize, self.c[sep + 1] as usize, &first_row, 0);
        let mut results = search.results;
        results.sort_unstable();
        results
    }
}

struct FuzzySearch<'a> {
    fm: &'a FmIndex,
    query: Vec<u8>,
    max_distance: usize,
    m: usize,
    results: Vec<(u32, u32)>,
    // Per-depth scratch, reused across siblings: child symbols with their Occ
    // ranges, and the DP rows handed down to each child. Depth never exceeds
    // m + max_distance: row[0] == depth, so deeper rows are pruned.
    children: Vec<Vec<(u8, usize, usize)>>,
    rows: Vec<Vec<usize>>,
}

impl FuzzySearch<'_> {
    fn visit(&mut self, lo: usize, hi: usize, row: &[usize], depth: usize) {
        let mut children = std::mem::take(&mut self.children[depth]);
        children.clear();
        self.enumerate_symbols(self.fm.root, lo, hi, &mut children);
        let mut next = std::mem::take(&mut self.rows[depth + 1]);

        for &(code, occ_lo, occ_hi) in &children {
            if code == SEPARATOR_CODE {
                // x is a complete term. The rows of [lo, hi) are the suffixes
                // "x SEP ...": those preceded by SEP start a term equal to x
                // (several dictionary terms may share one text form). The SEP
                // rows themselves are in position order, not LF order, so the
                // term is read off its own row, never through the SEP range.
                if depth > 0 && row[self.m] <= self.max_distance {
                    let distance = row[self.m] as u32;
                    if occ_hi - occ_lo == hi - lo {
                        for r in lo..hi {
                            self.results.push((self.fm.rowid_at_row(r), distance));
                        }
                    } else {
                        for r in lo..hi {
                            if self.fm.occ(SEPARATOR_CODE, r + 1) != self.fm.occ(SEPARATOR_CODE, r) {
                                self.results.push((self.fm.rowid_at_row(r), distance));
                            }
                        }
                    }
                }
                continue;
            }
            if code == END_CODE {
                continue;
            }

            next[0] = row[0] + 1;
            let mut min = next[0];
            for j in 1..=self.m {
                let mut v = row[j - 1] + usize::from(self.query[j - 1] != code);
                let delete = row[j] + 1;
                let insert = next[j - 1] + 1;
                if delete < v {
                    v = delete;
                }
                if insert < v {
                    v = insert;
                }
                next[j] = v;
                if v < min {
                    min = v;
                }
            }
            if min > self.max_distance {
                continue;
            }
            let c = self.fm.c[code as usize] as usize;
            // The child's own children overwrite rows[depth + 2], never this
            // row, and siblings are processed after the child returns.
            let next_copy = next.clone();
            self.visit(c + occ_lo, c + occ_hi, &next_copy, depth + 1);
        }

        self.rows[depth + 1] = next;
        self.children[depth] = children;
    }

    /// Distinct symbols in BWT[lo, hi) with their Occ at lo and hi.
    fn enumerate_symbols(&self, node: usize, lo: usize, hi: usize, into: &mut Vec<(u8, usize, usize)>) {
        let ones_lo = self.fm.rank1(node, lo);
        let ones_hi = self.fm.rank1(node, hi);
        let zeros_lo = lo - ones_lo;
        let zeros_hi = hi - ones_hi;
        if zeros_hi > zeros_lo {
            self.descend(self.fm.left[node], zeros_lo, zeros_hi, into);
        }
        if ones_hi > ones_lo {
            self.descend(self.fm.right[node], ones_lo, ones_hi, into);
        }
    }

    fn descend(&self, child: i32, lo: usize, hi: usize, into: &mut Vec<(u8, usize, usize)>) {
        if child < 0 {
            into.push(((-child - 1) as u8, lo, hi));
        } else {
            self.enumerate_symbols(child as usize, lo, hi, into);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(terms: &[&str]) -> FmIndex {
        let bytes = build(terms.iter().copied()).unwrap();
        FmIndex::open(Box::new(bytes)).unwrap()
    }

    #[test]
    fn infix_and_suffix_lookups() {
        // rowid = index + 1
        let fm = open(&["אבגד", "בגד", "גדול", "שלום", "שלומכם", "לום"]);
        assert_eq!(fm.find_row_ids("גד", false), vec![1, 2, 3]); // infix
        assert_eq!(fm.find_row_ids("גד", true), vec![1, 2]); // suffix: ends with גד
        assert_eq!(fm.find_row_ids("לום", true), vec![4, 6]);
        assert_eq!(fm.find_row_ids("לום", false), vec![4, 6]); // שלומכם has a medial מ
        assert_eq!(fm.find_row_ids("לומ", false), vec![5]);
        assert_eq!(fm.find_row_ids("שלומכם", true), vec![5]);
        assert!(fm.find_row_ids("זזז", false).is_empty());
        assert!(fm.find_row_ids("", false).is_empty());
        assert_eq!(fm.term_count(), 6);
    }

    #[test]
    fn fuzzy_distances() {
        let fm = open(&["שלום", "שלם", "שלומות", "אבג", "שלומ"]);
        let hits = fm.find_fuzzy("שלום", 1);
        // שלום (0), שלם (1: delete ו), שלומ (1: substitute ם→מ); שלומות is 2 away
        assert_eq!(hits, vec![(1, 0), (2, 1), (5, 1)]);
        // שלומות: ם→מ, +ו, +ת = 3 edits
        let hits2 = fm.find_fuzzy("שלום", 2);
        assert!(!hits2.iter().any(|&(r, _)| r == 3 || r == 4));
        assert!(fm.find_fuzzy("שלום", 3).contains(&(3, 3)));
    }

    #[test]
    fn duplicate_text_forms_return_every_rowid() {
        // רשי and רש"י both strip to רשי: both rowids must come back
        let fm = open(&["אבג", "רשי", "רשי", "רשת"]);
        assert_eq!(fm.find_row_ids("שי", true), vec![2, 3]);
        let fuzzy = fm.find_fuzzy("רשי", 0);
        assert_eq!(fuzzy, vec![(2, 0), (3, 0)]);
    }

    #[test]
    fn long_terms_and_many_terms() {
        // beyond the 10-code sort key, and enough terms for multi-block rank
        let terms: Vec<String> = (0..3000)
            .map(|i| format!("{}{}", "אבגדהוזחטיכלמנסעפצקרשת".chars().cycle().skip(i % 22).take(12).collect::<String>(), i % 7))
            .collect();
        let refs: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
        let bytes = build(refs.iter().copied()).unwrap();
        let fm = FmIndex::open(Box::new(bytes)).unwrap();
        // every term is found by its own full text as a suffix match of itself
        for (i, t) in refs.iter().enumerate().step_by(97) {
            let ids = fm.find_row_ids(t, true);
            assert!(ids.contains(&(i as u32 + 1)), "term {i} {t}");
        }
        let ids = fm.find_row_ids("אבגדהוזחטיכל", false);
        assert!(!ids.is_empty());
    }
}

