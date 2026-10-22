"""Line-level diff of the exact single-word rows of the Otzaria hit-count comparison.

usage: python3 -I -X utf8 tools/bench/hitdiff.py <results dir> <seforim.db> <demo exe> <ours pos index> <otzaria driver> <otzaria index> <native lib>

For every `exact` row with a non-zero difference in <results dir>/otz-hitcount.json: dumps our
hit lines (`SeforimSearch.Demo dump`), Otzaria's hit lines (`otzaria-driver search`, segment =
line index of the book), and splits the difference into ours-only and Otzaria-only lines.
Each ours-only / Otzaria-only line is then tested against Otzaria's own indexing text: its
`strip_html_for_indexing` rule is replayed (tags deleted with no separator unless a block tag,
`&...;` entities deleted), pieces are split on whitespace and punctuation inside a piece is
dropped, so `<sup>5</sup>word` / `word.<sup>5</sup>` become the single token `5word` / `word5`
and the word is not a token any more. Lines where the word is still a token
are kept in `unexplained` (sample ids) for a manual look. For queries with a quote / geresh a
line with no piece spelled exactly like the query counts as `quote_free` (ours matches quote-free). Dumps already in
<results dir>/hc/hitdiff/ours are reused. Writes <results dir>/hc/hitdiff.json; prints counts
only (ASCII, no Hebrew).
"""
import array, json, re, sqlite3, subprocess, sys
from pathlib import Path

out, db, demo, idx, otz_exe, otz_idx, native = sys.argv[1:8]
out = Path(out)
rows = []
for st, rs in json.loads((out / "otz-hitcount.json").read_text(encoding="utf-8"))["sets"].items():
    rows += [(st, r) for r in rs if r["mode"] == "exact" and r["ours"] != r["otz"]]
adv = [(st, r) for st, rs in json.loads((out / "otz-hitcount.json").read_text(encoding="utf-8"))["sets"].items()
       for r in rs if r["mode"] == "advanced" and r["ours"] != r["otz"]]

con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
n_ids = con.execute("select max(id) from line").fetchone()[0] + 1
book_of = array.array("q", [0]) * n_ids
idx_of = array.array("q", [0]) * n_ids
for i, b, k in con.execute("select id, bookId, lineIndex from line"):
    book_of[i] = b; idx_of[i] = k

BREAKING = {"address", "article", "aside", "blockquote", "br", "caption", "center", "dd", "div", "dl", "dt",
            "figcaption", "figure", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li", "main",
            "nav", "ol", "p", "pre", "section", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul"}  # approximates BREAKING_TAG_NAMES
def otz_text(c):
    c = re.sub(r"&(?:nbsp|thinsp|ensp|emsp);", " ", c)
    def tag(m):
        name = re.match(r"/?([A-Za-z0-9]{1,10})", m.group(1))
        return " " if name and name.group(1).lower() in BREAKING else ""
    c = re.sub(r"<([^>]*)>", tag, c)
    c = re.sub(r"&[^;]+;", "", c)
    return re.sub("[\u0591-\u05c7]", "", c)
def otz_has_token(word, content):
    w = "".join(ch for ch in word if "\u05d0" <= ch <= "\u05ea")
    # Otzaria splits on whitespace only and drops punctuation inside a piece: `word.<sup>3</sup>` -> `word3`
    return w in (re.sub(r"\W", "", t) for t in otz_text(content).split())

QUOTES = "\"'\u05f4\u05f3\u201c\u201d\u2019\u2018"
def canon(x):
    return x.translate(str.maketrans({"\u05f4": '"', "\u05f3": "'", "\u201c": '"', "\u201d": '"', "\u2019": "'", "\u2018": "'"}))
def quote_free_only(word, content):
    """The line has no piece spelled exactly like the quoted query (quote missing, moved, or another kind of
    quote): ours matches quote-free, Otzaria needs the quote where the query has it."""
    if not any(ch in QUOTES for ch in word):
        return False
    pieces = [canon(re.sub("[^\u05d0-\u05ea" + QUOTES + "]", "", t)) for t in otz_text(content).split()]
    w = canon(word)
    return w not in pieces

res = {}
work = out / "hc" / "hitdiff"
work.mkdir(parents=True, exist_ok=True)
qfile = work / "queries.txt"
qfile.write_text("\n".join(r["query"] for _, r in rows) + "\n", encoding="utf-8")
if not all((work / "ours" / f"{n}.jsonl").exists() for n in range(len(rows))):
    subprocess.run([demo, "dump", "--db", db, "--index", idx, "--queries", str(qfile), "--out", str(work / "ours"),
                "--max-hits", "100000000"], check=True, stderr=subprocess.DEVNULL)
for n, (st, r) in enumerate(rows):
    ours = set()
    for l in (work / "ours" / f"{n}.jsonl").open(encoding="utf-8"):
        if l.startswith('{"line"'):
            ours.add(int(l[8:l.index(",", 8)]))
    p = subprocess.run([otz_exe, "search", db, otz_idx, "--mode", "exact", "--limit", "100000000", r["query"]],
                       capture_output=True, check=True)
    otz = set()
    for l in p.stdout.decode("utf-8").splitlines():
        d = json.loads(l)
        otz.add((d["book"], d["segment"]))
    ours_keys = {(book_of[i], idx_of[i]): i for i in ours}
    only_ours = [ours_keys[k] for k in ours_keys.keys() - otz]
    only_otz = otz - ours_keys.keys()
    word = r["query"]
    unexplained = [i for i in only_ours
                   if otz_has_token(word, con.execute("select content from line where id=?", (i,)).fetchone()[0])]
    qfree = [i for i in unexplained
             if quote_free_only(word, con.execute("select content from line where id=?", (i,)).fetchone()[0])]
    unexplained = [i for i in unexplained if i not in set(qfree)]
    g_ours = len(only_ours) - len(unexplained) - len(qfree)
    g_otz = 0
    for b, k in only_otz:
        row = con.execute("select content from line where bookId=? and lineIndex=?", (b, k)).fetchone()
        g_otz += bool(row and not otz_has_token(word, row[0]))
    res[f"{st}:{r['n']}"] = dict(ours=len(ours), otz=len(otz), only_ours=len(only_ours), only_otz=len(only_otz),
                                 only_ours_explained=g_ours, only_ours_quote_free=len(qfree), unexplained_n=len(unexplained), only_otz_explained=g_otz,
                                 unexplained=sorted(unexplained)[:5])
    print(f"{st} q{r['n']}: ours {len(ours)} otz {len(otz)} only-ours {len(only_ours)} (Otzaria strip {g_ours}, quote-free {len(qfree)}, unexplained {len(unexplained)}) "
          f"only-otz {len(only_otz)} (otz-only, word not a token {g_otz})", flush=True)

# several-word rows: ours through the native library with the same request hitcount.py sends (ordered
# proximity, cross_line), Otzaria through the driver; a line is explained when one of the query words is
# not a token of Otzaria's indexing text of the line (or of the line plus the next one, cross-line).
import ctypes as C
sys.path.insert(0, str(Path(__file__).resolve().parent))
import hitcount
lib = hitcount.load(native)
ix = lib.se_index_open(idx.encode(), len(idx.encode()))
qlines = {st: [l for l in (Path(__file__).resolve().parent.parent / "parity" / f"queries-{st}.txt").read_text(encoding="utf-8").splitlines()
               if l.strip() and not l.startswith("#")] for st in hitcount.SETS}
lib.se_search_copy_hits.restype = C.c_uint64
lib.se_search_copy_hits.argtypes = [C.c_void_p, C.c_uint64, C.c_uint64, C.POINTER(C.c_uint64)]
def content_pair(i):
    row = con.execute("select content, bookId, lineIndex from line where id=?", (i,)).fetchone()
    nxt = con.execute("select content from line where bookId=? and lineIndex=?", (row[1], row[2] + 1)).fetchone()
    return row[0], (row[0] + " " + nxt[0]) if nxt else row[0]
def words_ok(words, text):
    toks = set(re.sub(r"\W", "", t) for t in otz_text(text).split())
    return all(w in toks for w in words)
for st, r in adv:
    q, _, d = qlines[st][r["n"]].partition("\t")
    req = json.dumps(hitcount.translate(q, d.strip()), ensure_ascii=False).encode()
    sr = lib.se_search(ix, req, len(req))
    n_hits = lib.se_search_hit_count(sr)
    buf = (C.c_uint64 * (3 * n_hits))()
    lib.se_search_copy_hits(sr, 0, n_hits, buf)
    lib.se_search_close(sr)
    ours_ids = {buf[i * 3 + 1] for i in range(n_hits)}
    cmd = [otz_exe, "search", db, otz_idx, "--mode", "advanced", "--distance", d.strip() or "1000", "--limit", "100000000", q]
    p = subprocess.run(cmd, capture_output=True, check=True)
    otz = {(json.loads(l)["book"], json.loads(l)["segment"]) for l in p.stdout.decode("utf-8").splitlines()}
    ours_keys = {(book_of[i], idx_of[i]): i for i in ours_ids}
    only_ours = [ours_keys[k] for k in ours_keys.keys() - otz]
    only_otz = otz - ours_keys.keys()
    words = ["".join(ch for ch in w if "\u05d0" <= ch <= "\u05ea") for w in q.split()]
    unexplained = [i for i in only_ours if words_ok(words, content_pair(i)[1])]
    otz_unexplained = 0
    for b, k in only_otz:
        row = con.execute("select id from line where bookId=? and lineIndex=?", (b, k)).fetchone()
        otz_unexplained += bool(row and not words_ok(words, content_pair(row[0])[1]))
    res[f"{st}:{r['n']}"] = dict(ours=len(ours_ids), otz=len(otz), only_ours=len(only_ours), only_otz=len(only_otz),
                                 only_ours_explained=len(only_ours) - len(unexplained), only_ours_quote_free=0,
                                 unexplained_n=len(unexplained), only_otz_unexplained=otz_unexplained,
                                 unexplained=sorted(unexplained)[:5])
    print(f"{st} q{r['n']} (advanced): ours {len(ours_ids)} otz {len(otz)} only-ours {len(only_ours)} "
          f"(Otzaria strip {len(only_ours) - len(unexplained)}, unexplained {len(unexplained)}) only-otz {len(only_otz)} "
          f"(a query word is a token anyway: {otz_unexplained})", flush=True)

(out / "hc" / "hitdiff.json").write_text(json.dumps(res, indent=1), encoding="utf-8")
