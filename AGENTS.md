# AGENTS.md

Instructions for every coding agent working in this repo (Codex, Gemini, Cursor, Claude, ...).

**Read `CLAUDE.md` first and follow all of it.** It holds the full project rules, how the
parts fit, and the lessons learned. This file repeats only the rules that are broken most
often.

## Most important rules

- **This is a reusable search library, not Otzaria.** It provides the search features
  Otzaria (and KitveiHakodesh) need, but nothing in it is Otzaria-specific. Otzaria's app
  code (its other sources such as user books / attached libraries, downloaders, UI trees,
  its own DBs and settings) is NOT our work and NOT an engine gap: never port it, never
  open issues or plan work for it. When auditing "what Otzaria has", ask only: is this a
  SEARCH capability a host needs? If yes, build it generically; if it is Otzaria app
  plumbing, skip it and say so.
- **The index is the bare minimum.** No seforim.db / SQLite code in Rust or tantivy; the
  host reads the DB and feeds lines.
- **No hacks.** A missing search capability is built properly into the engine.
- **Never add an AI co-author or any AI attribution line** to a commit or PR, in this repo
  or in `tantivy/`.
- **Quick tests only** (unit tests, small fixtures, builds). Full-DB rebuilds, parity sets
  and benchmarks only when the user explicitly asks.
- **Commit only your own work:** stage explicit paths (never `git add -A` / `.`), and never
  reset the shared git index; other sessions may be editing the tree.
- **Fix every in-scope gap** you or your review find before committing; report only real
  decisions.
- **Keep README.md current** in the same commit as any feature change.
- **Specs are generated:** run `python -I -X utf8 tools/report/specs_report.py`; never
  hand-edit between the `SPECS:BEGIN` / `SPECS:END` markers.
- **Hebrew text can trigger a network filter (HTTP 418):** keep Hebrew out of prompts and
  output; refer to it by unicode escapes or file:line.
