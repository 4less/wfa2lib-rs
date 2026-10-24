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

- it4 (v4, taken, 0.934x vs v3, 0.716x vs v0): ends-free extend checks termination once per diagonal (no second pass). Dovetail/extension 0.84-0.86x.
  Note: dovetail spans make the score-0 wavefront ~91 diagonals wide, so ends-free steps are width-bound (extend), unlike e2e 150bp (~9 diagonals).

- it5 (v5, taken, 0.960x vs v4, 0.687x vs v0): AVX2 affine kernel without scalar tail (overlapping last block; masked loads + pad reset for <8 diagonals); fill_null short-pad path; prefetches removed (neutral).
  Dropped again: scalar-first vector trim (+7% slower on drop case). Vector trim seems to lose from store-forwarding on just-written nulls; leave trim scalar.
  Per-line instruction shares inside compute_affine_step (before v5): scalar tail 21%, trim 13%, fill 12%, prefetch 5%, kernel blocks ~15%, alloc/recycle ~8%.

- it6 (v6, taken, 0.987x vs v5, 0.678x vs v0): 32-byte AVX2 compare in extend (long match runs: 150bp 2% 0.95x, 1k 5% 0.95x); hand-vectorised min-distance pass (overlapping last block). Drop case / ends-free neutral.

- it7 (v7, taken, 0.995x vs v6, 0.675x vs v0; memory 5-18x lower): Tight slab sizes wavefronts by need (1.5x rounded to 32, min 64) instead of 1000 elements each.
  Dropped: gather-based AVX2 extend at count>=8/32 (+20% slower on drop case; gathers lose on short wavefronts).

- it8 (v8, taken, 0.983x vs v7, 0.664x vs v0 → GOAL REACHED at 50.6% speedup): affine trim scans only within the inputs' init hull (outside is null by invariant).
  Dropped: running min-distance in extend (+7%/+14% slower). Note: ends-free wavefronts average ~43 diagonals (begin-free span) vs ~9 for e2e; inherent (C does the same).
  Vector trim keeps losing: likely store-forwarding stalls (32B loads spanning several of the kernel's 32B stores); kernel-mask-based trim judged not worth it.

- it9 (v9, taken, 0.989x vs v8, 0.657x vs v0): affine kernel inline(always) under compile-time AVX2 (cfg_attr; inline(always)+target_feature is rejected by rustc).

- it10 (v10, taken, 0.985x vs v9, 0.647x vs v0): AVX-512VL unsigned compare-to-mask + mask blend for the kernel bounds check (cfg target_feature); allocate_output_ptrs::<3> single slab call.
  Observed: extend loop is already ~14 instr/diagonal (AVX-512 masked compare), branch on "32 chars all match" is unpredictable at low divergence; inherent.

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
