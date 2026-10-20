"""Engine comparison report: build stages, index size, search stages per
query, and parity, for several engines on the same seforim.db.

Usage:
  python -I tools/report/compare_report.py <out dir> \\
      --engine "<name>" <search-report.json> <build log> [--engine ...] \\
      [--parity "<name>" <compare.py output>] ...

Writes <out dir>/comparison.md and <out dir>/comparison.html (self-contained,
SVG charts, light and dark). Time units: ms below one second, minutes
otherwise, never seconds (the user's rule).
"""
import html
import json
import re
import sys
from pathlib import Path

COLORS = ["#2563eb", "#16a34a", "#d97706", "#9333ea", "#dc2626"]


def fmt_ms(ms):
    """ms below one second, minutes otherwise."""
    if ms is None:
        return "-"
    if ms < 1000:
        return f"{ms:,.0f} ms"
    m = ms / 60000
    return f"{m:.2f} min" if m < 10 else f"{m:.1f} min"


def fmt_mb(b):
    return "-" if b is None else f"{b / 1e6:,.1f} MB"


def parse_build(log_path):
    text = Path(log_path).read_text(encoding="utf-8", errors="replace")
    stages = [(m.group(1), float(m.group(2)) * 1000)
              for m in re.finditer(r"stage: (.+?): ([\d.]+)s", text)]
    total = re.search(r"build: .*?in ([\d.]+)s", text)
    size = re.search(r"index size: ([\d.]+) MB", text)
    parts = [(m.group(1), float(m.group(2)) * 1e6)
             for m in re.finditer(r"^\s+(\S+)\s+([\d.]+) MB$", text, re.M)]
    exit_ = re.search(r"BUILD EXIT (\d+)", text)
    return {
        "stages": stages,
        "total_ms": float(total.group(1)) * 1000 if total else None,
        "size_bytes": float(size.group(1)) * 1e6 if size else None,
        "parts": [p for p in parts if p[1] > 0],
        "exit": int(exit_.group(1)) if exit_ else None,
    }


def parse_parity(path):
    text = Path(path).read_text(encoding="utf-8", errors="replace")
    summary = re.search(r"(\d+)/(\d+) queries identical", text)
    diffs = [l for l in text.splitlines() if " DIFF " in l]
    return {"same": int(summary.group(1)) if summary else None,
            "total": int(summary.group(2)) if summary else None,
            "diffs": diffs}


def bar_chart(title, labels, series, value_fmt, unit_note=""):
    """Grouped horizontal bars: one group per label, one bar per series.
    series: [(name, color, [values...])]. Log scale when the range is wide."""
    vals = [v for _, _, vs in series for v in vs if v]
    if not vals:
        return ""
    vmax = max(vals)
    vmin = min(vals)
    use_log = vmax / max(vmin, 1e-9) > 200
    import math

    def scale(v):
        if not v:
            return 0
        if use_log:
            lo = math.log10(max(vmin, 1e-3)) - 0.3
            return (math.log10(v) - lo) / (math.log10(vmax) - lo)
        return v / vmax

    bar_h, gap, label_w, plot_w = 12, 10, 230, 520
    group_h = bar_h * len(series) + gap
    height = 40 + group_h * len(labels) + 30
    out = [f'<figure class="chart"><figcaption>{html.escape(title)}'
           f'{" (log scale)" if use_log else ""}{html.escape(unit_note)}</figcaption>',
           f'<svg viewBox="0 0 {label_w + plot_w + 120} {height}" role="img" aria-label="{html.escape(title)}">']
    y = 30
    for i, label in enumerate(labels):
        out.append(f'<text x="{label_w - 8}" y="{y + group_h / 2 - 2}" class="lbl" text-anchor="end">'
                   f'{html.escape(label)}</text>')
        for name, color, vs in series:
            v = vs[i]
            w = max(1, scale(v) * plot_w) if v else 0
            out.append(f'<rect x="{label_w}" y="{y}" width="{w:.1f}" height="{bar_h - 2}" fill="{color}">'
                       f'<title>{html.escape(name)}: {html.escape(value_fmt(v))}</title></rect>')
            out.append(f'<text x="{label_w + w + 4:.1f}" y="{y + bar_h - 3}" class="val">'
                       f'{html.escape(value_fmt(v))}</text>')
            y += bar_h
        y += gap
    lx = label_w
    for name, color, _ in series:
        out.append(f'<rect x="{lx}" y="{height - 18}" width="10" height="10" fill="{color}"/>'
                   f'<text x="{lx + 14}" y="{height - 9}" class="lbl">{html.escape(name)}</text>')
        lx += 14 + 8 * len(name) + 20
    out.append("</svg></figure>")
    return "\n".join(out)


def main(argv):
    out = Path(argv[0])
    out.mkdir(parents=True, exist_ok=True)
    engines, parity = [], {}
    i = 1
    while i < len(argv):
        if argv[i] == "--engine":
            name, sj, blog = argv[i + 1:i + 4]
            engines.append({"name": name, "search": json.loads(Path(sj).read_text(encoding="utf-8")),
                            "build": parse_build(blog)})
            i += 4
        elif argv[i] == "--parity":
            parity[argv[i + 1]] = parse_parity(argv[i + 2])
            i += 3
        else:
            raise SystemExit(f"unknown argument {argv[i]}")

    queries = [r["query"] + (f"  (dist {r['dist']})" if r["dist"] != "-" else "")
               for r in engines[0]["search"]["rows"]]

    def col(e, key):
        return [r.get(key) for r in e["search"]["rows"]]

    md = ["# Engine comparison: FtsLib vs tantivy engine", "",
          "Same seforim.db (7,080,797 lines), same queries, same machine, one run at a time.",
          "Times: ms below one second, minutes otherwise.", ""]

    # ── build ──
    md += ["## Index build", "", "| engine | " + " | ".join(
        ["total"] + sorted({s for e in engines for s, _ in e["build"]["stages"]}, key=str)) + " | size | exit |"]
    stage_names = sorted({s for e in engines for s, _ in e["build"]["stages"]}, key=str)
    md.append("|---" * (len(stage_names) + 4) + "|")
    for e in engines:
        b = e["build"]
        st = dict(b["stages"])
        md.append(f"| {e['name']} | {fmt_ms(b['total_ms'])} | " +
                  " | ".join(fmt_ms(st.get(s)) for s in stage_names) +
                  f" | {fmt_mb(b['size_bytes'])} | {b['exit']} |")
    md += ["", "Index size by part:", ""]
    for e in engines:
        md.append(f"- **{e['name']}**: " + ", ".join(f"{n} {fmt_mb(v)}" for n, v in e["build"]["parts"]))

    # ── search ──
    md += ["", "## Search, per query", "",
           "engine hits = lines the engine matched; walks+collect = FST walks / FM lookups + "
           "postings (engine); host = text fetch + snippet locate + widening; first = first "
           "batch of located results; total = the whole search reading EVERY hit.", ""]
    head = "| query | " + " | ".join(
        f"{e['name']} {k}" for k in ("first", "total") for e in engines) + " |"
    md += [head, "|---" * (1 + 2 * len(engines)) + "|"]
    for qi, q in enumerate(queries):
        cells = []
        for k in ("first_ms", "total_ms"):
            for e in engines:
                cells.append(fmt_ms(e["search"]["rows"][qi].get(k)))
        md.append(f"| {q} | " + " | ".join(cells) + " |")
    md += ["", "Stage detail (engine part):", "",
           "| query | " + " | ".join(f"{e['name']} hits | {e['name']} walks+collect | {e['name']} host"
                                    for e in engines) + " |",
           "|---" * (1 + 3 * len(engines)) + "|"]
    for qi, q in enumerate(queries):
        cells = []
        for e in engines:
            r = e["search"]["rows"][qi]
            cells += [f"{r['engine_hits'] or r['hits'] or 0:,.0f}", fmt_ms(r.get("walks_collect_ms")),
                      fmt_ms(r.get("host_ms"))]
        md.append(f"| {q} | " + " | ".join(cells) + " |")

    def total(e, k):
        return sum(v for v in col(e, k) if v)

    md += ["", "Sums over all queries:", "", "| engine | first batch | total | failed runs |", "|---|---|---|---|"]
    for e in engines:
        failed = sum(1 for r in e["search"]["rows"] if r["exit"] != 0)
        md.append(f"| {e['name']} | {fmt_ms(total(e, 'first_ms'))} | {fmt_ms(total(e, 'total_ms'))} | {failed} |")

    # ── parity ──
    if parity:
        md += ["", "## Parity with FtsLib", ""]
        for name, p in parity.items():
            md.append(f"- **{name}**: {p['same']}/{p['total']} queries identical.")
            for d in p["diffs"]:
                md.append(f"  - {d.strip()}")
    (out / "comparison.md").write_text("\n".join(md) + "\n", encoding="utf-8")

    # ── HTML with charts ──
    series = lambda key: [(e["name"], COLORS[k % len(COLORS)], col(e, key)) for k, e in enumerate(engines)]
    charts = [
        bar_chart("Index build, total", [e["name"] for e in engines],
                  [("total", COLORS[0], [e["build"]["total_ms"] for e in engines])], fmt_ms),
        bar_chart("Index size", [e["name"] for e in engines],
                  [("size", COLORS[1], [e["build"]["size_bytes"] for e in engines])], fmt_mb),
        bar_chart("Build stages", stage_names,
                  [(e["name"], COLORS[k % len(COLORS)], [dict(e["build"]["stages"]).get(s) for s in stage_names])
                   for k, e in enumerate(engines)], fmt_ms),
        bar_chart("Search: time to the first batch of results", queries, series("first_ms"), fmt_ms),
        bar_chart("Search: total, reading every hit", queries, series("total_ms"), fmt_ms),
        bar_chart("Search stage: engine walks + collect", queries,
                  [s for s in series("walks_collect_ms") if any(s[2])], fmt_ms),
        bar_chart("Search stage: host (text fetch + locate + widening)", queries,
                  [s for s in series("host_ms") if any(s[2])], fmt_ms),
    ]
    body_md = "\n".join(md)
    page = f"""<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Engine Comparison</title>
<style>
:root{{--bg:#fff;--fg:#1f2328;--muted:#59636e;--line:#d1d9e0}}
@media (prefers-color-scheme: dark){{:root:not([data-theme="light"]){{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--line:#3d444d}}}}
:root[data-theme="dark"]{{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--line:#3d444d}}
body{{background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif;margin:0 auto;max-width:1100px;padding:16px}}
.chart{{margin:24px 0;overflow-x:auto}} figcaption{{font-weight:600;margin-bottom:6px}}
svg{{width:100%;min-width:640px;height:auto}} .lbl{{fill:var(--fg);font-size:11px}} .val{{fill:var(--muted);font-size:10px}}
pre{{white-space:pre-wrap;border:1px solid var(--line);padding:12px;border-radius:6px;font-size:12px}}
</style></head><body>
<h1>Engine comparison</h1>
<p>Same seforim.db, same queries, same machine, one run at a time. Times in ms below one second, minutes otherwise.</p>
{''.join(c for c in charts if c)}
<h2>Tables</h2><pre>{html.escape(body_md)}</pre>
</body></html>"""
    (out / "comparison.html").write_text(page, encoding="utf-8")
    print(f"wrote {out / 'comparison.md'} and {out / 'comparison.html'}")


if __name__ == "__main__":
    main(sys.argv[1:])
