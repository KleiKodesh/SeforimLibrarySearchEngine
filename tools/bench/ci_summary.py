"""Summarise a full-DB run (CI workflow fullrun.yml, or any run-all OUT_DIR).

usage: python -I -X utf8 tools/bench/ci_summary.py <out dir> [<summary.json> <summary.md>]
Reads <out dir>/logs/*.log (last line `EXIT <code> (<min> min)`), parity/*-diffs.json
(compare.py --diff-out), otz-hitcount.json (hitcount.py) and verify-xl.json
(verify_xl_ss.py); writes a machine-readable summary.json and a markdown report (the
workflow appends it to $GITHUB_STEP_SUMMARY). Exit 1 when any unit failed.
Times from a shared GitHub runner are NOT representative: speed is measured locally only.
"""
import json, re, sys
from pathlib import Path

NOTE = ("Times below come from a shared GitHub runner and are NOT representative; "
        "speed benchmarks run locally only (tools/bench/AGENT-RUNBOOK.md).")


def units(out):
    rows = []
    for f in sorted((out / "logs").glob("*.log")):
        txt = f.read_text(encoding="utf-8", errors="replace").splitlines()
        m = next((re.match(r"EXIT (\d+) \((\d+) min\)", l) for l in reversed(txt) if l.startswith("EXIT ")), None)
        rows.append({"unit": f.stem, "status": ("ok" if m and m.group(1) == "0" else "failed" if m else "no exit line"),
                     "exit": int(m.group(1)) if m else None, "minutes": int(m.group(2)) if m else None,
                     "log": f"logs/{f.name}"})
    return rows


def build_reports(out):
    res = {}
    for f in sorted((out / "logs").glob("build-*.log")):
        if f.stem.startswith("build-cargo") or f.stem.startswith("build-dotnet"):
            continue
        keep = [l.rstrip() for l in f.read_text(encoding="utf-8", errors="replace").splitlines()
                if re.match(r"\s*(stage: |stage memory: |read queue |process peak |index size: |build: |EXIT )", l)
                or re.match(r"\s+\.[\w]+\s+[0-9.]+ MB", l)]
        res[f.stem[len("build-"):]] = keep
    return res


def parity(out):
    res = []
    for f in sorted((out / "parity").glob("*-diffs.json")):
        s, fl = f.stem[:-len("-diffs")].rsplit("-", 1)
        d = json.loads(f.read_text(encoding="utf-8"))
        res.append({"set": s, "flavour": fl, "queries": d["queries"], "identical": d["identical"],
                    "identical_hits": d["identical_hits"], "diff_queries": len(d["diffs"]),
                    "diff_file": f"parity/{f.name}"})
    return res


def main(out, js=None, md=None):
    out = Path(out)
    js = Path(js or out / "summary.json"); md = Path(md or out / "summary.md")
    u = units(out)
    summary = {"note": NOTE, "units": u, "build_reports": build_reports(out), "parity": parity(out)}
    hc = out / "otz-hitcount.json"
    if hc.exists():
        h = json.loads(hc.read_text(encoding="utf-8"))
        summary["hitcount"] = {st: {"queries": len(rows), "equal": sum(r["class"] == "equal" for r in rows),
                                    "unclassified": sum(r["class"] == "unclassified" for r in rows),
                                    "classes": sorted({r["class"] for r in rows})} for st, rows in h["sets"].items()}
    vx = out / "verify-xl.json"
    if vx.exists():
        v = json.loads(vx.read_text(encoding="utf-8"))
        summary["verify_xl"] = {"same_section_queries": len(v["same_section"]), "cross_line_queries": len(v["cross_line"]),
                                "failures": v.get("failures", [])}
    js.write_text(json.dumps(summary, indent=1), encoding="utf-8")

    L = ["# Full-DB run", "", f"> {NOTE}", "", "## Units", "", "| unit | status | exit | min |", "|---|---|---|---|"]
    L += [f"| {r['unit']} | {r['status']} | {r['exit']} | {r['minutes']} |" for r in u]
    if summary["parity"]:
        L += ["", "## FtsLib parity", "", "(positions flavour: snippet / distance differences are the named deviation of issue #21; "
              "only hit set and order must match)", "",
              "| set | flavour | identical | identical hits | queries |", "|---|---|---|---|---|"]
        L += [f"| {p['set']} | {p['flavour']} | {p['identical']} | {p['identical_hits']} | {p['queries']} |" for p in summary["parity"]]
    if hc.exists():
        L += ["", "## Otzaria hit counts", "", "| set | n | query | dist | otzaria | ours | class | reason |", "|---|---|---|---|---|---|---|---|"]
        for st, rows in h["sets"].items():
            L += [f"| {st} | {r['n']} | {r['query'].replace('|', '/')} | {r['dist']} | {r['otz']} | {r['ours']} | {r['class']} | {r['reason']} |"
                  for r in rows]
    if vx.exists():
        L += ["", "## Cross-line + sameSection (full DB)", "",
              "| set | n | sameSection ours | recount | missing | extra |", "|---|---|---|---|---|---|"]
        L += [f"| {r['set']} | {r['n']} | {r['ss_hits']} | {r['expected']} | {r['missing']} | {r['extra']} |" for r in v["same_section"]]
        L += ["", "| set | n | in-line | with cross | cross-only | lost in-line | consecutive | gapped | other book |",
              "|---|---|---|---|---|---|---|---|---|"]
        L += [f"| {r['set']} | {r['n']} | {r['inline']} | {r['cross_on']} | {r['cross_only']} | {r['lost_inline']} | "
              f"{r['consecutive']} | {r['gapped']} | {len(r.get('other_book_pairs', []))} |" for r in v["cross_line"]]
        L += ["", f"failures: {', '.join(v.get('failures', [])) or 'none'}"]
    bad = [r["unit"] for r in u if r["status"] == "failed"]
    L += ["", f"**Failed units:** {', '.join(bad) if bad else 'none'}", ""]
    md.write_text("\n".join(L), encoding="utf-8")
    print(f"units {len(u)}, failed {len(bad)}: {' '.join(bad)}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:4]))
