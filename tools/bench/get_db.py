"""Find seforim.db on this machine, or download it from Otzaria's official release.

usage: python3 -I tools/bench/get_db.py <data dir> [--db PATH] [--find-only]
Prints the resolved path on the LAST stdout line. Exit 0 = found/downloaded, 1 = not found.

Search order: --db, then the defaults per OS (Otzaria app data dir + /books/seforim.db,
Zayit's databases dir), then <data dir>/seforim.db.
Download source = what the Otzaria app itself uses (lib/empty_library/bloc/
empty_library_bloc.dart, _fetchLatestDatabaseAsset): the latest release of
github.com/Otzaria/SeforimLibrary, first asset found of seforim-schema7.db.zst,
seforim-schema6.db.zst, seforim.db.zst (or its .manifest.json split parts).
Resumes a partial download (HTTP Range), checks size and the sha256 digest GitHub
publishes, then unpacks zstd (Python 3.14 compression.zstd, else the zstandard module,
else the zstd CLI). ASCII output only.
"""
import hashlib, json, os, platform, shutil, subprocess, sys, urllib.request
from pathlib import Path

API = "https://api.github.com/repos/Otzaria/SeforimLibrary/releases/latest"
NAMES = ["seforim-schema7.db.zst", "seforim-schema6.db.zst", "seforim.db.zst"]


def candidates():
    h = Path.home()
    s = platform.system()
    if s == "Windows":
        a = Path(os.environ.get("APPDATA", h / "AppData/Roaming"))
        return [a / "otzaria/books/seforim.db", a / "io.github.kdroidfilter.seforimapp/databases/seforim.db"]
    if s == "Darwin":
        b = h / "Library/Application Support"
        return [b / "otzaria/books/seforim.db", b / "io.github.kdroidfilter.seforimapp/databases/seforim.db"]
    return [h / ".local/share/otzaria/books/seforim.db",
            h / ".local/share/io.github.kdroidfilter.seforimapp/databases/seforim.db",
            h / ".config/io.github.kdroidfilter.seforimapp/databases/seforim.db"]


def get_json(url):
    r = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json", "User-Agent": "seforim-bench"})
    with urllib.request.urlopen(r, timeout=60) as f:
        return json.load(f)


def download(url, dest, size=None, sha=None):
    dest = Path(dest)
    have = dest.stat().st_size if dest.exists() else 0
    if size and have == size:
        print(f"already downloaded: {dest.name}", flush=True)
    else:
        if size and have > size:
            dest.unlink(); have = 0
        req = urllib.request.Request(url, headers={"User-Agent": "seforim-bench"})
        if have:
            req.add_header("Range", f"bytes={have}-")
            print(f"resuming {dest.name} at {have / 1e6:,.0f} MB", flush=True)
        with urllib.request.urlopen(req, timeout=120) as r, open(dest, "ab" if have else "wb") as f:
            if have and r.status != 206:
                f.truncate(0); have = 0
            total = size or (have + int(r.headers.get("Content-Length", 0)))
            done, last = have, 0
            while True:
                b = r.read(1 << 20)
                if not b:
                    break
                f.write(b); done += len(b)
                if done - last > 50e6 or done == total:
                    last = done
                    pct = f" {100 * done / total:.1f}%" if total else ""
                    print(f"  {dest.name}: {done / 1e6:,.0f} / {total / 1e6:,.0f} MB{pct}", flush=True)
    if size and dest.stat().st_size != size:
        raise SystemExit(f"size mismatch for {dest.name}: {dest.stat().st_size} != {size} (rerun to resume)")
    if sha:
        print(f"checking sha256 of {dest.name}", flush=True)
        h = hashlib.sha256()
        with open(dest, "rb") as f:
            for b in iter(lambda: f.read(1 << 22), b""):
                h.update(b)
        if h.hexdigest() != sha:
            dest.unlink()
            raise SystemExit(f"sha256 mismatch for {dest.name}: deleted, rerun to download again")


def unzstd(src, dst):
    print(f"unpacking {Path(src).name} -> {dst}", flush=True)
    tmp = Path(str(dst) + ".part")
    try:
        from compression import zstd as z  # Python 3.14+
        with z.open(src, "rb") as i, open(tmp, "wb") as o:
            shutil.copyfileobj(i, o, 1 << 22)
    except ImportError:
        try:
            import zstandard
            with open(src, "rb") as i, open(tmp, "wb") as o:
                zstandard.ZstdDecompressor().copy_stream(i, o, read_size=1 << 22, write_size=1 << 22)
        except ImportError:
            if not shutil.which("zstd"):
                raise SystemExit("no zstd decoder: install Python 3.14+, `pip install zstandard`, or the zstd CLI")
            subprocess.run(["zstd", "-d", "-f", "--long=31", str(src), "-o", str(tmp)], check=True)
    os.replace(tmp, dst)


def main():
    a = sys.argv[1:]
    data = Path(a[0]); data.mkdir(parents=True, exist_ok=True)
    explicit = a[a.index("--db") + 1] if "--db" in a and a[a.index("--db") + 1] else None
    paths = ([Path(explicit)] if explicit else []) + candidates() + [data / "seforim.db"]
    for p in paths:
        if p.is_file() and p.stat().st_size > 0:
            print(f"seforim.db found ({p.stat().st_size / 1e9:.2f} GB)")
            print(p)
            return 0
    if explicit:
        print(f"--db given but not found: {explicit}")
        return 1
    if "--find-only" in a:
        print("seforim.db not found in: " + "; ".join(str(p) for p in paths))
        return 1
    print("seforim.db not found locally; downloading Otzaria's latest release", flush=True)
    rel = get_json(API)
    assets = {x["name"]: x for x in rel.get("assets", [])}
    dst = data / "seforim.db"
    for name in NAMES:
        if name in assets:
            x = assets[name]
            print(f"release {rel.get('tag_name')}: {name} ({x['size'] / 1e9:.2f} GB compressed)", flush=True)
            arc = data / name
            dig = (x.get("digest") or "").removeprefix("sha256:") or None
            download(x["browser_download_url"], arc, x["size"], dig)
            unzstd(arc, dst)
            break
        m = assets.get(name + ".manifest.json")
        if m:
            man = get_json(m["browser_download_url"])
            print(f"release {rel.get('tag_name')}: {name} in {len(man['parts'])} parts", flush=True)
            arc = data / name
            parts = []
            for part in man["parts"]:
                pp = data / part["name"]
                download(assets[part["name"]]["browser_download_url"], pp, part["size"], part.get("sha256"))
                parts.append(pp)
            with open(arc, "wb") as o:
                for pp in parts:
                    with open(pp, "rb") as i:
                        shutil.copyfileobj(i, o, 1 << 22)
            unzstd(arc, dst)
            break
    else:
        print("no seforim.db asset in the latest release; download it with the Otzaria app and pass --db")
        return 1
    (data / "seforim.db.source.txt").write_text(f"{API}\nrelease {rel.get('tag_name')}\n", encoding="utf-8")
    print(dst)
    return 0


if __name__ == "__main__":
    sys.exit(main())
