"""Explain the positions-flavour parity differences line by line. ASCII output only.

usage: python -I -X utf8 tools/parity/explain_pos_diffs.py <native lib> <pos index> <nopos index>
       <seforim.db> <parity dir> [per query, default 5]

For every query of <parity dir>/<set>-pos-diffs.json (compare.py --diff-out) that has a word
distance and lines only one flavour returned, takes up to N such lines, reads their text from
seforim.db and locates them (se_search_locate_batch) in three sessions:
  pos+d    the positions index with the distance (the word-tightest window, issue #21)
  nopos+d  the no-positions index with the distance (FtsLib's path step for step)
  nopos    the no-positions index WITHOUT the distance: the window FtsLib's rule picks (fewest
           UTF-16 units), with its word distance
Printed per line: line id, each session's pass / word distance / window words / mark word
indexes (word = run of letters, digits and quotes; approximate, for reading only).
"A line only ours returned" is explained when pos+d passes with distance <= d, nopos+d drops it
and the nopos window's distance is > d: a closer pair exists, FtsLib measured only its
char-tightest window. Lines FtsLib alone returned are printed for reading; that they sit at the
tail of the capped dump (pushed out by the added lines) is checked by compare.py --strict positions."""
import ctypes as C, json, re, sqlite3, sys
from pathlib import Path

R = 1 << 52
HDR = 7


class Buf(C.Structure):
    _fields_ = [("data", C.POINTER(C.c_uint32)), ("len", C.c_size_t), ("cap", C.c_size_t)]


def load(lib):
    d = C.CDLL(lib)
    d.se_index_open.restype = C.c_void_p; d.se_index_open.argtypes = [C.c_char_p, C.c_size_t]
    d.se_search.restype = C.c_void_p; d.se_search.argtypes = [C.c_void_p, C.c_char_p, C.c_size_t]
    d.se_search_hit_count.restype = C.c_uint64; d.se_search_hit_count.argtypes = [C.c_void_p]
    d.se_search_locate_batch.restype = Buf
    d.se_search_locate_batch.argtypes = [C.c_void_p, C.POINTER(C.c_char_p), C.POINTER(C.c_size_t), C.c_size_t]
    d.se_buffer_free.argtypes = [Buf]
    d.se_last_error.restype = C.c_char_p
    d.se_search_close.argtypes = [C.c_void_p]
    return d


class Texts:
    def __init__(self, path):
        self.db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        cols = [r[1] for r in self.db.execute("pragma table_info(line)")]
        self.inline = "content" in cols
        self.dicts = {}
        if not self.inline:
            from compression import zstd
            self.z = zstd
            for _, blob in self.db.execute("select id, dict from zstd_dict"):
                zd = zstd.ZstdDict(blob)
                self.dicts[zd.dict_id] = zd

    def get(self, lid):
        if lid >= R:
            return None  # virtual notes line: skipped
        if self.inline:
            r = self.db.execute("select content from line where id=?", (lid,)).fetchone()
            return r and r[0]
        r = self.db.execute("select content from line_content where id=?", (lid,)).fetchone()
        if not r or r[0] is None:
            return None
        v = r[0]
        if isinstance(v, str):
            if not v.startswith("KLUv"):
                return v
            import base64
            v = base64.b64decode(v)
        # frame header: dictionary id (if any) after the frame header descriptor
        for zd in [None] + list(self.dicts.values()):
            try:
                return self.z.decompress(v, zstd_dict=zd).decode("utf-8") if zd else self.z.decompress(v).decode("utf-8")
            except Exception:
                continue
        return None


WORD = re.compile(r"[\w'\"׳״]+")


def session(d, ix, req):
    js = json.dumps(req, ensure_ascii=False).encode()
    s = d.se_search(ix, js, len(js))
    if not s:
        raise RuntimeError((d.se_last_error() or b"").decode("utf-8", "replace").encode("ascii", "replace").decode())
    return s


def locate(d, s, text):
    b = text.encode("utf-8")
    arr = (C.c_char_p * 1)(b); lens = (C.c_size_t * 1)(len(b))
    buf = d.se_search_locate_batch(s, arr, lens, 1)
    v = [buf.data[i] for i in range(buf.len)]
    d.se_buffer_free(buf)
    if not v or v[0] == 0:
        return None
    score, dist, wc, start, end, flags, nm = v[1:1 + HDR]
    marks = [v[1 + HDR + 3 * i] for i in range(nm)]
    return dict(dist=dist, words=wc, start=start, end=end,
                mark_words=sorted({len(WORD.findall(text.encode("utf-16-le")[:2 * m].decode("utf-16-le", "ignore"))) for m in marks}))


def fmt(r):
    return "drop" if r is None else f"pass dist {r['dist']} window {r['words']} words {r['start']}-{r['end']} marks at words {r['mark_words']}"


def main(lib, pos, nopos, dbp, pdir, per="5"):
    per = int(per)
    d = load(lib)
    ixp = d.se_index_open(pos.encode(), len(pos.encode()))
    ixn = d.se_index_open(nopos.encode(), len(nopos.encode()))
    if not ixp or not ixn:
        raise SystemExit("index open failed")
    texts = Texts(dbp)
    explained = unexplained = 0
    for f in sorted(Path(pdir).glob("*-pos-diffs.json")):
        st = f.name[:-len("-pos-diffs.json")]
        extra = {"ignore_quotes": True, "ktiv_male_haser": True, "ktiv_rules": "ftslib"} if st == "ketiv" else {}
        for q in json.loads(f.read_text(encoding="utf-8"))["diffs"]:
            only_t, only_f = q.get("only_tantivy", []), q.get("only_ftslib", [])
            if not only_t and not only_f:
                continue
            dist = q.get("distance")
            tag = f"{st} #{q['n']} dist={dist} ftslib={q.get('ftslib_hits')} ours={q.get('tantivy_hits')} only_ours={len(only_t)} only_ftslib={len(only_f)}"
            print("==", tag, flush=True)
            if dist is None or dist >= 2147483647:
                print("   no distance: not explained by the window rule (a real difference)"); unexplained += 1
                continue
            base = dict(query=q["query"], **extra)
            sp = session(d, ixp, dict(base, max_word_distance=dist))
            sn = session(d, ixn, dict(base, max_word_distance=dist))
            s0 = session(d, ixn, base)
            for lid in only_t[:per] + only_f[:per]:
                side = "ours" if lid in only_t else "ftslib"
                t = texts.get(lid)
                if t is None:
                    print(f"   line {lid} ({side} only): no text (notes line or unreadable)"); continue
                a, b, c = locate(d, sp, t), locate(d, sn, t), locate(d, s0, t)
                ok = side == "ours" and a is not None and a["dist"] <= dist and b is None and (c is None or c["dist"] > dist)
                explained += ok; unexplained += (side == "ours" and not ok)
                print(f"   line {lid} ({side} only){' EXPLAINED' if ok else ''}\n"
                      f"      pos+d:   {fmt(a)}\n      nopos+d: {fmt(b)}\n      nopos:   {fmt(c)}", flush=True)
            for s in (sp, sn, s0):
                d.se_search_close(s)
    print(f"lines only ours, explained by the word-tightest window: {explained}; not explained: {unexplained}")
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
