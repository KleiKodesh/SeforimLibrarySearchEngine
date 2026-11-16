"""Per-row content hashes of some books of a seforim.db, to compare two copies of the DB
(e.g. the CI release download against a local Otzaria install) without printing text.

usage: python -I row_hashes.py <seforim.db> <book id>[,<book id>...]
prints: book <id> rows <n> notes <n> sha <whole-book sha256[:16]>, then one line per row:
  <book> <lineIndex> <line id> <len> <sha256[:12]> <starts-with-BOM 0/1>
Content as Otzaria reads it: TEXT rows as their UTF-8 bytes, BLOB rows zstd-decoded with
the dictionary named in the frame (Python 3.14 compression.zstd, or the zstandard module)."""
import hashlib
import sqlite3
import sys

try:
    from compression import zstd as _z

    def make_dict(blob):
        return _z.ZstdDict(blob)

    def dict_id(d):
        return d.dict_id

    def frame_dict_id(frame):
        return _z.get_frame_info(frame).dictionary_id

    def decompress(frame, d):
        return _z.decompress(frame, zstd_dict=d)
except ImportError:
    import zstandard as _z

    def make_dict(blob):
        return _z.ZstdCompressionDict(blob)

    def dict_id(d):
        return d.dict_id()

    def frame_dict_id(frame):
        return _z.get_frame_parameters(frame).dict_id

    def decompress(frame, d):
        return _z.ZstdDecompressor(dict_data=d).decompress(frame)


def main():
    db, books = sys.argv[1], [int(b) for b in sys.argv[2].split(',')]
    c = sqlite3.connect('file:' + db + '?mode=ro', uri=True)
    dicts = {}
    tables = {r[0] for r in c.execute("select name from sqlite_master where type='table'")}
    if 'zstd_dict' in tables:
        for _id, blob in c.execute('select id, dict from zstd_dict'):
            d = make_dict(blob)
            dicts[dict_id(d)] = d

    def raw(v):
        if v is None:
            return b''
        if isinstance(v, str):
            return v.encode('utf-8', 'surrogatepass')
        return decompress(v, dicts[frame_dict_id(v)])

    split = 'line_content' in tables
    sql = ('select l.lineIndex, l.id, lc.content from line l left join line_content lc on lc.id = l.id'
           if split else 'select l.lineIndex, l.id, l.content from line l') + ' where l.bookId = ? order by l.lineIndex, l.id'
    for b in books:
        rows = [(li, i, raw(v)) for li, i, v in c.execute(sql, (b,))]
        notes = c.execute('select count(*) from line_note n join line l on l.id = n.lineId where l.bookId = ?', (b,)).fetchone()[0] \
            if 'line_note' in tables else 0
        whole = hashlib.sha256(b''.join(r[2] + b'\x00' for r in rows)).hexdigest()[:16]
        print(f'book {b} rows {len(rows)} notes {notes} sha {whole}')
        for li, i, r in rows:
            print(f'{b} {li} {i} {len(r)} {hashlib.sha256(r).hexdigest()[:12]} {int(r.startswith(b"\xef\xbb\xbf"))}')


if __name__ == '__main__':
    main()
