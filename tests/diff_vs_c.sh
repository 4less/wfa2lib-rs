#!/usr/bin/env bash
# Differential test: run the C reference align_benchmark and the Rust one over a
# matrix of configurations and report every configuration whose output differs.
#
# Usage: tests/diff_vs_c.sh [filter-regex]
#   Requires WFA2-lib/bin/align_benchmark (cd WFA2-lib && make) and a release build.
#   Datasets are generated once into tests/diff_data/ with WFA2-lib's generate_dataset.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
C_BIN="$ROOT/WFA2-lib/bin/align_benchmark"
R_BIN="$ROOT/target/release/align_benchmark"
GEN="$ROOT/WFA2-lib/bin/generate_dataset"
DATA="$ROOT/tests/diff_data"
FILTER="${1:-.}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$DATA"
gen() { [[ -f "$DATA/$1.seq" ]] || "$GEN" -n "$2" -l "$3" -e "$4" -o "$DATA/$1.seq" >/dev/null; }
gen short_e05 2000 100 0.05
gen short_e20 2000 100 0.20
gen mid_e10 300 1000 0.10
gen long_e05 20 5000 0.05
cp "$ROOT/WFA2-lib/tests/wfa.utest.seq" "$DATA/utest.seq"
DATASETS=(utest short_e05 short_e20 mid_e10 long_e05)

ALGOS=(
  "indel|-a indel-wfa"
  "edit|-a edit-wfa"
  "linear|-a gap-linear-wfa"
  "affine|-a gap-affine-wfa"
  "affine.p1|-a gap-affine-wfa --affine-penalties=0,3,1,4"
  "affine.m|-a gap-affine-wfa --affine-penalties=-2,3,1,4"
  "affine2p|-a gap-affine2p-wfa"
)
MODES=(
  "cigar|"
  "score|--wfa-score-only"
  "biwfa|--wfa-memory=ultralow"
)
SPANS=(
  "global|"
  "endsfree|--wfa-span=ends-free,0.1,0.1,0.1,0.1"
  "endsfree.end|--wfa-span=ends-free,0,0.2,0,0.2"
  "extension|--wfa-span=extension"
)
HEURISTICS=(
  "none|"
  "band|--wfa-heuristic=banded-static --wfa-heuristic-parameters=-10,10"
  "bandadapt|--wfa-heuristic=banded-adaptive --wfa-heuristic-parameters=-10,10,1"
  "wfadapt|--wfa-heuristic=wfa-adaptive --wfa-heuristic-parameters=10,50,1"
  "xdrop|--wfa-heuristic=xdrop --wfa-heuristic-parameters=50,1"
  "zdrop|--wfa-heuristic=zdrop --wfa-heuristic-parameters=50,1"
)

# One job per configuration: "name|args"
jobs="$TMP/jobs"; : > "$jobs"
for ds in "${DATASETS[@]}"; do
for a in "${ALGOS[@]}"; do for m in "${MODES[@]}"; do for sp in "${SPANS[@]}"; do for h in "${HEURISTICS[@]}"; do
  name="$ds.${a%%|*}.${m%%|*}.${sp%%|*}.${h%%|*}"
  [[ "$name" =~ $FILTER ]] || continue
  # C refuses BiWFA + ends-free/extension and drops with edit/indel; skip those.
  if [[ "${m%%|*}" == biwfa && "${sp%%|*}" != global ]]; then continue; fi
  if [[ "${h%%|*}" =~ drop && "${a%%|*}" =~ ^(edit|indel)$ ]]; then continue; fi
  echo "$name|$DATA/$ds.seq|${a#*|} ${m#*|} ${sp#*|} ${h#*|}" >> "$jobs"
done; done; done; done; done

run_one() {
  IFS='|' read -r name seq args <<< "$1"
  d="$TMP/$name"; mkdir -p "$d"
  # Cap each run (4 GB address space, 60 s) so one runaway config can't take the machine.
  ( ulimit -v 4000000; timeout 60 "$C_BIN" -i "$seq" -o "$d/c.alg" $args >/dev/null 2>"$d/c.err" ); cs=$?
  ( ulimit -v 4000000; timeout 60 "$R_BIN" -i "$seq" -o "$d/r.alg" $args >/dev/null 2>"$d/r.err" ); rs=$?
  if (( cs != 0 )); then echo "SKIP $name"
  elif (( rs == 0 )) && cmp -s "$d/c.alg" "$d/r.alg"; then echo "PASS $name"
  elif (( rs != 0 )); then echo "FAIL $name (rust exit=$rs)"
  else echo "FAIL $name ($(diff "$d/c.alg" "$d/r.alg" | grep -c '^<') lines differ)"
  fi
  rm -rf "$d"
}
export -f run_one; export C_BIN R_BIN TMP
tr '\n' '\0' < "$jobs" | xargs -0 -P "${JOBS:-2}" -I{} bash -c 'run_one "$@"' _ {} > "$TMP/results"

pass=$(grep -c '^PASS' "$TMP/results"); fail=$(grep -c '^FAIL' "$TMP/results"); skip=$(grep -c '^SKIP' "$TMP/results")
echo "pass=$pass fail=$fail skip(C error)=$skip"
grep '^FAIL' "$TMP/results" | sort
(( fail == 0 ))
