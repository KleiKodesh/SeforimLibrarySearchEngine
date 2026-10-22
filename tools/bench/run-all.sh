#!/usr/bin/env bash
# One-command benchmark: ours (positions / no positions / keep marks) vs FtsLib vs
# Otzaria v5.0.0 on the full seforim.db. macOS, Linux, Windows (Git Bash).
# See tools/bench/README.md and tools/bench/AGENT-RUNBOOK.md.
#
#   tools/bench/run-all.sh [--steps a,b,...] [--db PATH] [--out DIR] [--force] [--dry-run]
#
# Steps (in this order; default = STEPS in bench.env):
#   detect         inventory: OS/arch, tools, submodules, DB, binaries, indexes, results; prints the plan
#   bootstrap      install missing prerequisites (brew on macOS, apt/dnf on Linux, winget on Windows)
#   db             find seforim.db or download it (get_db.py)
#   build-tools    native engine (cargo), C# demo, FtsLibDump, Otzaria driver
#   build-indexes  ours pos, nopos, keep_marks; FtsLib; Otzaria (sequential, one at a time)
#   parity         FtsLib parity sets (FtsLibDump dump vs demo dump, compare.py)
#   speed          every engine on every speed set, median of RUNS after a warm-up, sequential
#   hitcount       Otzaria hit-count comparison (needs the Otzaria speed runs)
#   report         FTS-SPECS-AND-COMPARISONS.md + README/CLAUDE.md Specs blocks
# Resume-safe: a finished unit leaves $OUT_DIR/.done/<unit>; it is skipped next time
# unless --force (an index is also rebuilt when its recorded format changed).
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

source "$HERE/bench.env"
[ -f "$HERE/bench.local.env" ] && source "$HERE/bench.local.env"

FORCE=0; DRY=0; ARG_STEPS=""; ARG_DB=""; ARG_OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --steps) ARG_STEPS="$2"; shift 2;;
    --db) ARG_DB="$2"; shift 2;;
    --out) ARG_OUT="$2"; shift 2;;
    --force) FORCE=1; shift;;
    --dry-run) DRY=1; shift;;
    -h|--help) sed -n '2,22p' "$0"; exit 0;;
    *) echo "unknown argument: $1"; exit 2;;
  esac
done
[ -n "$ARG_STEPS" ] && STEPS="$ARG_STEPS"
[ -n "$ARG_DB" ] && DB_PATH="$ARG_DB"
[ -n "$ARG_OUT" ] && OUT_DIR="$ARG_OUT"

# ---- platform ----
case "$(uname -s)" in
  Darwin) PLAT=mac; LIB=libseforim_search_native.dylib; EXE_SUF="";;
  Linux) PLAT=linux; LIB=libseforim_search_native.so; EXE_SUF="";;
  MINGW*|MSYS*|CYGWIN*) PLAT=windows; LIB=seforim_search_native.dll; EXE_SUF=".exe";;
  *) PLAT=unknown; LIB=libseforim_search_native.so; EXE_SUF="";;
esac
ARCH="$(uname -m)"
export PATH="$HOME/.cargo/bin:$HOME/.dotnet:$PATH"
[ "$PLAT" = windows ] && export MSYS_NO_PATHCONV=1
PY=python3
for c in python3 python; do "$c" -c "import sys" >/dev/null 2>&1 && { PY="$c"; break; }; done
# Windows python wants Windows paths; cygpath when present.
wp() { if [ "$PLAT" = windows ] && command -v cygpath >/dev/null; then cygpath -m "$1"; else echo "$1"; fi; }

OUT_DIR="${OUT_DIR/#\~/$HOME}"
LOGS="$OUT_DIR/logs"; DONE="$OUT_DIR/.done"
IDX_POS="${IDX_POS:-$OUT_DIR/indexes/ours-pos}"
IDX_NOPOS="${IDX_NOPOS:-$OUT_DIR/indexes/ours-nopos}"
IDX_MARKS="${IDX_MARKS:-$OUT_DIR/indexes/ours-marks}"
IDX_FTSLIB="${IDX_FTSLIB:-$OUT_DIR/indexes/ftslib}"
IDX_OTZARIA="${IDX_OTZARIA:-$OUT_DIR/indexes/otzaria}"
KITVEI_DIR="${KITVEI_DIR:-$ROOT/external/KleiKodeshProject/KitveiHakodesh}"

NATIVE="$ROOT/target/release/$LIB"
DEMO="$ROOT/csharp/SeforimSearch.Demo/bin/Release/net10.0/SeforimSearch.Demo$EXE_SUF"
if [ "$PLAT" = windows ]; then FTS_TFM=net10.0-windows; else FTS_TFM=net10.0; fi
FTS="$ROOT/tools/parity/FtsLibDump/bin/Release/$FTS_TFM/FtsLibDump$EXE_SUF"
OTZ="$HERE/otzaria-driver/target/release/otzaria-driver$EXE_SUF"

FMT_OURS="$(grep -o 'IndexFormatVersion = "[a-z0-9]*"' csharp/SeforimSearch.Demo/FtsLib/TantivySeforimIndex.cs | cut -d'"' -f2)"
FMT_FTS="$(grep -o 'IndexFormatVersion = "[a-z0-9]*"' "$KITVEI_DIR/CSharpBackend/KitveiHakodesh.Core/SeforimDbFullTextSearch/SeforimDbFtsIndexer.cs" 2>/dev/null | cut -d'"' -f2)"
FMT_OTZ="otzaria-v5.0.0"

has() { case ",$STEPS," in *",$1,"*) return 0;; esac; return 1; }
done_ok() { [ $FORCE = 0 ] && [ -f "$DONE/$1" ] && [ "$(cat "$DONE/$1")" = "${2:-ok}" ]; }
mark() { mkdir -p "$DONE"; echo "${2:-ok}" > "$DONE/$1"; }
FAILED=()
# run <unit> <log name> <cmd...>: full output teed to the log, exit code recorded.
run() {
  local unit="$1" log="$LOGS/$2.log"; shift 2
  if [ $DRY = 1 ]; then echo "  [plan] $unit: $* > $log"; return 0; fi
  mkdir -p "$LOGS"
  echo "== $unit $(date +%T) (log: $log)"
  local t0=$SECONDS
  "$@" 2>&1 | tee "$log"
  local rc=${PIPESTATUS[0]}
  echo "EXIT $rc ($(( (SECONDS - t0) / 60 )) min)" | tee -a "$log"
  [ $rc != 0 ] && { FAILED+=("$unit (exit $rc, $log)"); return $rc; }
  return 0
}

# ---------------- detect ----------------
ver() { command -v "$1" >/dev/null 2>&1 && { "$@" 2>&1 | head -n1; } || echo "MISSING"; }
detect() {
  echo "== inventory ($(date))"
  echo "OS: $PLAT ($(uname -sr)), arch: $ARCH"
  if [ $PLAT = mac ]; then echo "CPU: $(sysctl -n machdep.cpu.brand_string 2>/dev/null), cores $(sysctl -n hw.ncpu), RAM $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
  elif [ $PLAT = linux ]; then echo "CPU: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), cores $(nproc), RAM $(free -g | awk '/Mem/{print $2}') GB"
  else echo "CPU: ${PROCESSOR_IDENTIFIER:-?}, cores ${NUMBER_OF_PROCESSORS:-?}"; fi
  echo "disk free at out dir: $(mkdir -p "$OUT_DIR" && df -h "$OUT_DIR" | awk 'NR==2{print $4}')"
  echo "git: $(ver git --version)"
  echo "cargo: $(ver cargo --version)"
  echo "dotnet SDKs: $(command -v dotnet >/dev/null && dotnet --list-sdks | awk '{print $1}' | tr '\n' ' ' || echo MISSING)"
  echo "python: $(ver "$PY" --version)"
  echo "zstd: $(ver zstd --version) (only needed to unpack a downloaded DB on Python < 3.14 without the zstandard module)"
  echo "submodule tantivy: $([ -f tantivy/Cargo.toml ] && echo ok || echo 'NOT INITIALISED (git submodule update --init --recursive)')"
  echo "KitveiHakodesh: $([ -d "$KITVEI_DIR/CSharpBackend" ] && echo "ok ($KITVEI_DIR)" || echo "MISSING ($KITVEI_DIR)")"
  local db; db="$("$PY" -I "$(wp "$HERE/get_db.py")" "$(wp "$OUT_DIR/data")" --db "$DB_PATH" --find-only | tail -n1)"
  echo "seforim.db: $db"
  for b in "$NATIVE" "$DEMO" "$FTS" "$OTZ"; do echo "binary: $([ -f "$b" ] && echo "built  " || echo "missing") $b"; done
  for i in pos:$IDX_POS:$FMT_OURS nopos:$IDX_NOPOS:$FMT_OURS marks:$IDX_MARKS:$FMT_OURS ftslib:$IDX_FTSLIB:$FMT_FTS otzaria:$IDX_OTZARIA:$FMT_OTZ; do
    IFS=: read -r n p f <<<"$i"
    # (Windows paths contain ':' only after the drive letter when OUT_DIR is a C:/ path; use /c/... in Git Bash)
    local st="missing"; [ -f "$DONE/index-$n" ] && st="built (format $(cat "$DONE/index-$n"), current $f)"
    echo "index $n: $st"
  done
  echo "results: $(ls "$DONE" 2>/dev/null | grep -v '^index-' | tr '\n' ' ')"
  echo "== plan (steps: $STEPS; --force redoes finished units)"
}

# ---------------- bootstrap ----------------
bootstrap() {
  case $PLAT in
    mac)
      command -v brew >/dev/null || { echo "Install Homebrew first: https://brew.sh"; return 1; }
      brew install git python zstd
      command -v cargo >/dev/null || { curl https://sh.rustup.rs -sSf | sh -s -- -y; }
      dotnet --list-sdks 2>/dev/null | grep -q '^10\.' || brew install --cask dotnet-sdk;;
    linux)
      if command -v apt-get >/dev/null; then sudo apt-get update && sudo apt-get install -y git python3 zstd build-essential curl
      elif command -v dnf >/dev/null; then sudo dnf install -y git python3 zstd gcc curl; fi
      command -v cargo >/dev/null || { curl https://sh.rustup.rs -sSf | sh -s -- -y; }
      dotnet --list-sdks 2>/dev/null | grep -q '^10\.' || { curl -sSL https://dot.net/v1/dotnet-install.sh | bash -s -- --channel 10.0; };;
    windows)
      echo "Windows: install with winget (if missing): Git.Git, Rustlang.Rustup, Microsoft.DotNet.SDK.10, Python.Python.3.14,"
      echo "  Microsoft.VisualStudio.2022.BuildTools (C++ build tools for Rust; .NET Framework 4.8 targeting pack for the net48 demo)."
      for p in Git.Git Rustlang.Rustup Microsoft.DotNet.SDK.10 Python.Python.3.14; do winget install -e --id $p --accept-package-agreements --accept-source-agreements || true; done;;
  esac
  git submodule update --init --recursive
}

# ---------------- steps ----------------
DB=""
resolve_db() {
  [ -n "$DB" ] && return 0
  if [ $DRY = 1 ]; then DB="${DB_PATH:-<seforim.db: found or downloaded by step db>}"; return 0; fi
  DB="$("$PY" -I "$(wp "$HERE/get_db.py")" "$(wp "$OUT_DIR/data")" --db "$DB_PATH" --find-only | tail -n1)"
  [ -f "$DB" ] || { echo "seforim.db not found: run step db (or pass --db)"; exit 1; }
}

step_db() {
  if [ $DRY = 1 ]; then echo "  [plan] db: find seforim.db (--db, Otzaria/Zayit defaults, $OUT_DIR/data) or download it"; return; fi
  mkdir -p "$LOGS"
  "$PY" -I "$(wp "$HERE/get_db.py")" "$(wp "$OUT_DIR/data")" --db "$DB_PATH" 2>&1 | tee "$LOGS/db.log"
  [ ${PIPESTATUS[0]} = 0 ] || { echo "seforim.db: not available (see $LOGS/db.log)"; exit 1; }
}

step_build_tools() {
  run cargo-engine build-cargo-engine cargo build --release -p engine-ffi
  if [ $PLAT = windows ]; then run dotnet-demo build-dotnet-demo dotnet build csharp/SeforimSearch.Demo -c Release
  else run dotnet-demo build-dotnet-demo dotnet build csharp/SeforimSearch.Demo -c Release -f net10.0 -p:PlatformTarget=AnyCPU; fi
  run dotnet-ftslibdump build-dotnet-ftslibdump dotnet build tools/parity/FtsLibDump -c Release "-p:KitveiHakodeshDir=$(wp "$KITVEI_DIR")"
  run cargo-otzaria build-cargo-otzaria cargo build --release --manifest-path "$(wp "$HERE/otzaria-driver/Cargo.toml")"
}

build_index() { # name dir format cmd...
  local n="$1" dir="$2" fmt="$3"; shift 3
  if done_ok "index-$n" "$fmt" && [ -d "$dir" ]; then echo "index $n: current ($fmt), reused: $dir"; return; fi
  [ $DRY = 0 ] && rm -rf "$dir" && mkdir -p "$(dirname "$dir")"
  run "index-$n" "build-$n" "$@" && [ $DRY = 0 ] && mark "index-$n" "$fmt"
}

step_build_indexes() {
  resolve_db
  build_index pos "$IDX_POS" "$FMT_OURS" "$DEMO" build --db "$DB" --index "$(wp "$IDX_POS")"
  build_index nopos "$IDX_NOPOS" "$FMT_OURS" "$DEMO" build --db "$DB" --index "$(wp "$IDX_NOPOS")" --no-positions
  build_index marks "$IDX_MARKS" "$FMT_OURS" "$DEMO" build --db "$DB" --index "$(wp "$IDX_MARKS")" --keep-marks
  build_index ftslib "$IDX_FTSLIB" "$FMT_FTS" "$FTS" build "$DB" "$(wp "$IDX_FTSLIB")"
  build_index otzaria "$IDX_OTZARIA" "$FMT_OTZ" "$OTZ" build "$DB" "$(wp "$IDX_OTZARIA")"
}

qfile() { echo "tools/parity/queries-$1.txt"; }

step_parity() {
  resolve_db
  for s in $PARITY_SETS; do
    local K=""; [ "$s" = ketiv ] && K=--ketiv
    local Q; Q="$(qfile "$s")"
    local ref="$OUT_DIR/parity/$s-fts"
    if ! done_ok "parity-$s"; then
      [ $DRY = 0 ] && rm -rf "$ref" && mkdir -p "$ref"
      run "parity-$s-ref" "parity-$s-fts" "$FTS" "$DB" "$(wp "$IDX_FTSLIB")" "$Q" "$(wp "$ref")" "$PARITY_MAX_HITS" $K || continue
      local ok=1
      for f in pos:$IDX_POS nopos:$IDX_NOPOS; do
        local fl="${f%%:*}" ix="${f#*:}" o="$OUT_DIR/parity/$s-${f%%:*}"
        [ $DRY = 0 ] && rm -rf "$o" && mkdir -p "$o"
        run "parity-$s-$fl" "parity-$s-$fl-dump" "$DEMO" dump --db "$DB" --index "$(wp "$ix")" --queries "$Q" --out "$(wp "$o")" --max-hits "$PARITY_MAX_HITS" $K || { ok=0; continue; }
        run "parity-$s-$fl-compare" "parity-$s-$fl-compare" "$PY" -I -X utf8 tools/parity/compare.py "$(wp "$ref")" "$(wp "$o")" || ok=0
      done
      [ $DRY = 0 ] && [ $ok = 1 ] && mark "parity-$s"
    else echo "parity $s: done (skipped)"; fi
  done
}

step_speed() {
  resolve_db
  for s in $SPEED_SETS; do
    local Q; Q="$(qfile "$s")"
    done_ok "speed-$s-otz" || { run "speed-$s-otz" "sb-$s-otz" "$PY" -I "$(wp "$HERE/otz_report.py")" "$OTZ" "$DB" "$(wp "$IDX_OTZARIA")" "$Q" "$(wp "$OUT_DIR/sb-$s-otz")" "$RUNS" && [ $DRY = 0 ] && mark "speed-$s-otz"; }
    done_ok "speed-$s-pos" || { run "speed-$s-pos" "sb-$s-pos" "$PY" -I tools/report/search_report.py "$DEMO" "$(wp "$IDX_POS")" "$Q" "$(wp "$OUT_DIR/sb-$s-pos")" --runs "$RUNS" && [ $DRY = 0 ] && mark "speed-$s-pos"; }
    done_ok "speed-$s-nopos" || { run "speed-$s-nopos" "sb-$s-nopos" "$PY" -I tools/report/search_report.py "$DEMO" "$(wp "$IDX_NOPOS")" "$Q" "$(wp "$OUT_DIR/sb-$s-nopos")" --runs "$RUNS" && [ $DRY = 0 ] && mark "speed-$s-nopos"; }
    done_ok "speed-$s-fts" || { run "speed-$s-fts" "sb-$s-fts" "$PY" -I tools/report/search_report.py "$FTS" "$(wp "$IDX_FTSLIB")" "$Q" "$(wp "$OUT_DIR/sb-$s-fts")" --ftslib "$DB" --runs "$RUNS" && [ $DRY = 0 ] && mark "speed-$s-fts"; }
  done
}

step_hitcount() {
  done_ok hitcount && { echo "hitcount: done (skipped)"; return; }
  run hitcount hitcount "$PY" -I -X utf8 "$(wp "$HERE/hitcount.py")" "$(wp "$NATIVE")" "$(wp "$IDX_POS")" "$(wp "$OUT_DIR")" && [ $DRY = 0 ] && mark hitcount
}

step_report() {
  # Build logs are where specs_report.py expects them: <out>/logs/build-<flavour>.log.
  [ $DRY = 0 ] && detect > "$OUT_DIR/machine.txt" 2>&1
  run report report "$PY" -I -X utf8 tools/report/specs_report.py "$(wp "$OUT_DIR")" --prefix ""
}

# ---------------- main ----------------
[ $DRY = 1 ] && echo "DRY RUN: nothing is executed"
{ has detect || has check; } && detect
has bootstrap && { [ $DRY = 1 ] && echo "  [plan] bootstrap: install prerequisites ($PLAT)" || bootstrap; }
has db && step_db
has build-tools && step_build_tools
has build-indexes && step_build_indexes
has parity && step_parity
has speed && step_speed
has hitcount && step_hitcount
has report && step_report
if [ ${#FAILED[@]} -gt 0 ]; then
  echo "== FAILED:"; printf '  %s\n' "${FAILED[@]}"; exit 1
fi
echo "== all requested steps done ($STEPS)"
