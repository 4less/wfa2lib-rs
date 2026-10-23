# flexalign speed loop (started 2026-10-24)

Goal: 50% speedup (geo-mean time 0.667x of v0) on flexalign-shaped workloads, no test regression.
Stop after 25 iterations. Loop job: cron 58f5e5de every 10 min (session-only).

## Procedure per iteration
1. Edit, `cargo build --release --example flexalign_bench`, quick run `taskset -c 4 target/release/examples/flexalign_bench 3 1`
   (checksums must equal previous version's; output must be identical).
2. `cargo test --release` (212 unit + 18 integration must pass) — needs ./WFA2-lib checkout (gitignored, cloned at 6176f94).
3. `cp target/release/examples/flexalign_bench $S/bin/cand_vN; python3 bench/flexalign_ab.py --label "vN ..." --candidate $S/bin/cand_vN --baseline $S/bin/best --note "..."`
   where S = scratchpad dir. Status taken if geo_ratio < 0.995, else dropped. On test failure: `--status failed`.
4. taken → `cp cand_vN $S/bin/best`, commit (code + bench/flexalign_history.jsonl + bench/flexalign_report.html).
   dropped/failed → `git checkout` the code, still commit history+report.
5. Report: bench/flexalign_report.html.

## Iterations
- it1 (v1, taken, 0.849x): WF-adaptive cutoff without distance buffer. Commit 1a7a355.
- it2 (v2, taken, 0.956x vs v1): NULL-fill at allocation, trim nulls trimmed diagonals, init_ends calls removed from compute steps.
  Measured: 150bp reads average ~9 diagonals/step (26 non-null steps for 2% e2e; 138 for the drop case, width ~36) → per-step fixed overhead (~450 cycles) dominates, kernel ~10 cycles.

- it3 (v3, taken, 0.945x vs v2, 0.767x vs v0): AVX2 fill_null; output wavefronts null only their pads (init_output / allocate_output_ptr).
  Dropped sub-experiments: AVX2 trim_ends (+8% slower: trims are 0-2 diagonals, scalar loop exits immediately);
  in-place regrow of too-small recycled wavefronts (+5% slower: dead arena bytes forced clear() every few alignments).
  Instruction profile (drop case): allocate_ptr was 19% of instructions (fill dominated), run_cases/extend 26%, cutoff 16%, compute 36%. IPC ~3.8 → instruction-bound.

## Backlog (ideas, highest expected value first)
- Per-step fixed overhead is THE lever for 150bp: 3 slab allocs (one recycle scan each) → allocate M/I1/D1 as one triple; trim_ends scalar scans → AVX2 + analytic null prefix for I1/D1; skip I1/D1 when both their inputs are null (needs kernel variants; backtrace already guards by init range).
- Phase-timing (rdtsc) is misleading at this granularity (~40 cycles per probe); use perf with CARGO_PROFILE_RELEASE_DEBUG=1 and --sort srcline.
- perf profile after v1: compute_affine_step (kernel inlined), extend (inlined in run_cases), apply_heuristic_cutoff remainder, slab allocate_ptr ~4%.
- Drop case (score 151 steps) dominates 150bp cost: many null steps; make the null-step path cheaper (skip 3 slab allocs when lo>hi etc.).
- In CIGAR (Tight) mode each step allocates 3 wavefronts from slab with recycle scan; consider arena bump per alignment.
- init_alignment: cigar.resize/clear, components.resize/clear per alignment; check cost for 150bp (3 µs total!).
- extend: check blockwise compare uses 64-bit; endsfree extend termination check per diagonal.
- Heuristic min pass: verify LLVM vectorised it (perf annotate); otherwise hand AVX2.
- Backtrace cost for CIGAR mode (backtrace_affine).
