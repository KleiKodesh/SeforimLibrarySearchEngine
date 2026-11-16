"""Issue G-K1 parity fixture: a small Zayit-shaped seforim.db and substring cases.

python -I -X utf8 tools/parity/substring_fixture.py <out dir>
Writes <out dir>/seforim.db and <out dir>/substring-cases.txt. Then compare
  FtsLibDump substring <db> <ftslib index> <cases> ftslib.txt
  SeforimSearch.Demo substring --db <db> --index <ours> --cases <cases> --out ours.txt
(see tools/parity/README or the main README "Parity with FtsLib"). Hebrew is built from
code points only (no Hebrew literal in this file).
"""
import os
import random
import sqlite3
import sys

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
db = os.path.join(out, "seforim.db")
if os.path.exists(db):
    os.remove(db)

LETTERS = [chr(c) for c in range(0x05D0, 0x05EB) if c not in (0x05DA, 0x05DD, 0x05DF, 0x05E3, 0x05E5)]
FINALS = {chr(0x05DB): chr(0x05DA), chr(0x05DE): chr(0x05DD), chr(0x05E0): chr(0x05DF),
          chr(0x05E4): chr(0x05E3), chr(0x05E6): chr(0x05E5)}
GERSHAYIM, MAQAF, QAMATS, PATAH = "״", "־", "ָ", "ַ"
rng = random.Random(18)


def word():
    w = "".join(rng.choice(LETTERS) for _ in range(rng.randint(2, 7)))
    if w[-1] in FINALS and rng.random() < 0.7:
        w = w[:-1] + FINALS[w[-1]]
    r = rng.random()
    if r < 0.05 and len(w) > 2:
        w = w[:-1] + GERSHAYIM + w[-1]          # an acronym
    elif r < 0.10:
        w = w[0] + QAMATS + w[1:]                # nikud
    elif r < 0.13:
        w = w[0] + PATAH + w[1:]
    return w


def line_text():
    words = [word() for _ in range(rng.randint(3, 12))]
    if rng.random() < 0.1:
        i = rng.randrange(len(words) - 1)
        words[i] = words[i] + MAQAF + words.pop(i + 1)
    if rng.random() < 0.1:
        words.append("abc%d" % rng.randint(0, 50))
    if rng.random() < 0.05:
        words.insert(0, "<b>" + words.pop(0) + "</b>")
    return " ".join(words)


c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE category (id INTEGER PRIMARY KEY NOT NULL, parentId INTEGER, title TEXT NOT NULL,
  level INTEGER NOT NULL DEFAULT 0, orderIndex INTEGER NOT NULL DEFAULT 999);
CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, categoryId INTEGER NOT NULL, sourceId INTEGER NOT NULL DEFAULT 0,
  title TEXT NOT NULL, heRef TEXT, orderIndex INTEGER NOT NULL DEFAULT 999, totalLines INTEGER NOT NULL DEFAULT 0,
  isBaseBook INTEGER NOT NULL DEFAULT 0);
CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL,
  content TEXT NOT NULL, heRef TEXT, tocEntryId INTEGER);
CREATE TABLE author (id INTEGER PRIMARY KEY NOT NULL, name TEXT NOT NULL);
CREATE TABLE book_author (bookId INTEGER NOT NULL, authorId INTEGER NOT NULL);
INSERT INTO category (id, parentId, title, level, orderIndex) VALUES (1, NULL, 'cat', 0, 1);
""")
BOOKS = [(1, 3, 900), (2, 1, 1200), (3, 2, 700), (4, 4, 400)]   # id, orderIndex, lines
texts = {}
lid = 0
for book, order, n in BOOKS:
    c.execute("INSERT INTO book (id, categoryId, title, orderIndex, totalLines) VALUES (?, 1, ?, ?, ?)",
              (book, "book %d" % book, order, n))
    for i in range(n):
        lid += 1
        t = line_text()
        texts[(book, i)] = t
        c.execute("INSERT INTO line (id, bookId, lineIndex, content, heRef) VALUES (?, ?, ?, ?, '')", (lid, book, i, t))
c.commit()
c.close()

# cases: fragments cut from real words (any place, any length), multi-word, quoted, marked,
# absent, too common (declined), punctuation only (declined); one range, a few, many
cases = []
words = sorted({w for t in texts.values() for w in t.replace(MAQAF, " ").split() if not w.startswith("<")})
for _ in range(60):
    w = rng.choice(words)
    a = rng.randrange(len(w))
    b = rng.randint(a + 1, len(w))
    frag = w[a:b]
    if rng.random() < 0.3:
        frag += " " + rng.choice(words)[:3]
    cases.append(frag)
cases += [words[5], words[9] + MAQAF + words[11], "abc1", "abc", LETTERS[3], LETTERS[3] + LETTERS[7],
          "zzz", "!!!", "  ", "x" + GERSHAYIM + "y", LETTERS[0] + GERSHAYIM + LETTERS[1]]


def ranges():
    k = rng.choice([1, 1, 3, 20, 40])
    out = []
    for _ in range(k):
        book, _, n = rng.choice(BOOKS + [(9, 0, 50)])
        first = rng.randint(-5, n)
        last = first + rng.choice([0, 0, 3, 50, 2000])
        out.append("%d:%d..%d" % (book, first, last))
    return ",".join(out)


with open(os.path.join(out, "substring-cases.txt"), "w", encoding="utf-8", newline="\n") as f:
    for q in cases:
        f.write(q + "\t" + ranges() + "\n")
print("wrote", db, "and", len(cases), "cases")
