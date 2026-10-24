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

- it11 (v11, taken, 0.985x vs v10, 0.638x vs v0): WF-adaptive reduce vectorised (scalar-first on the end diagonal, then 8-lane blocks). Ends-free 0.96-0.98x, 1k 0.98x, short e2e neutral.

- it12 (v12, DROPPED): M hull from kernel oob masks instead of trimming M: +4-6% slower on wide wavefronts (both branchy and cmov variants). Lesson: never add per-block work to the kernel; trims are cheap after v8.

- it13 (v13, FAILED): I1/D1 pads of 8 slots → non-deterministic output. Invariant: outputs must be null over their whole allocation outside [lo,hi]
  (allocation = min(lo-pad, historic_min_lo)..), because inputs are read over the *output's* range, which follows the widest (older, unpruned) input.
  Harness: checksum folds 8 bytes at a time now (bench-only; was 7% of 150bp time). Baseline binary rebuilt from unchanged code.

- it14 (v14, DROPPED): 64-char extend compare (zmm, and 2x ymm): +4-5% slower on drop/dovetail. Extend's hot branch is load-bound; 32B compare stays.
  Dovetail profile: ends-free extend = 46% of cycles (43 diagonals/step), compute 26%, cutoff 11%, init_alignment 2.4%.

- it15 (v15, taken, 0.989x vs v11, 0.631x vs v0): begin-free init as zero fill + identity ramp with exclusive ranges (ends-free 0.965x). 150bp e2e +2% is noise (unchanged code path).

- it16 (v16, DROPPED): inline(always) apply_heuristic_cutoff (neutral/-); lazy init_ends instead of pad fills (+10-25% slower). Pad fill at allocation is settled.

- it17 (v17, taken, 0.992x vs v15, 0.626x vs v0): pads of the 3 outputs nulled with one length dispatch per side (fill_null_n). Instr/step on the drop case now ~1650 (was ~2120 at v3).
  Per-alignment work (init_alignment, backtrace) measured negligible on short reads; the component-array clear is ~0.2%.

- it18 (v18, taken, 0.953x vs v17, 0.597x vs v0): compute_affine_step specialised on MODULAR (const generic). Bigger than expected (150bp 0.92x): the flag loads/branches and slot() calls were in the way of scheduling.
  Idea: the same specialisation for extend_end2end/extend_endsfree + apply_heuristic_cutoff (Lambda vs packed, heuristic kind) might give a bit more.

- it19 (v19, DROPPED, 0.998x): whole gap-affine main loop specialised on (MODULAR, HEURISTIC, ENDSFREE): neutral. The remaining per-step branches outside the compute step are well predicted; not worth the code.

- it20 (v20, DROPPED, 1.001x): lazy tail_room, kernel count guard removed, unconditional bt_occupancy reset: neutral.
  Validation: tests/wfa_utest.sh (60 C-reference cases: all metrics, ends-free, heuristics, BiWFA; full CIGAR identity) passes on v18.

- it21 (v21, taken for memory, speed 0.999x): per-score component arrays capped at max_alignment_steps+1 (CIGAR mode). Peak heap 150bp 93 KB -> 22 KB; 1k 0.5% 0.50 MB -> 55 KB; 1k 5% 2.2 -> 1.8 MB.
  Note: outputs of a step are already adjacent in the bump arena; "contiguous triple" idea has no slack.

- it22 (v22, taken for memory, speed 0.998x): arena first chunk 64 KB, null/victim +-256: base footprint 293 -> 90 KB. Harness now reports aligner_heap (base) next to growth.
  Memory insight: CIGAR mode keeps all wavefronts: drop case ~230 KB = 151 steps x 3 x 128 slots x 4 B (1.5x headroom + round-to-32). Exact sizing would halve it but hurts recycling.

- it23 (v23, taken for memory, speed 0.996x): Tight sizing rounded to 16 (min 48). 1.25x headroom variant: -30% heap on short reads but +20% on 1k 5% (recycle misses); not taken.

- it24 (v24, DROPPED, 1.006x): skip the compute at score == max_steps + lazy cutoff scratch buffer: neutral/noise. (C computes then checks too.)

- it25: final validation (212+18 cargo tests, 60 C-reference cases OK). Direct A/B final (v23, 851f3fb) vs v0 (eb936d4) with the same harness:
  geo-mean 0.592x; 150bp e2e 0.67x, drop 0.53x, dovetail 0.58x, extension 0.57x, 1k 5% 0.56x, 1k 0.5% 0.70x. Checksums equal v0's.
  Memory per aligner (150bp, base + growth): v0 ~0.3 MB base + 1.63 MB growth -> 0.09 MB + 0.15-0.22 MB.

## Status: LOOP FINISHED (25 iterations, goal 50% reached at it8). Cron job 58f5e5de cancelled.

## Taken versions (chronological)
v1 cutoff without distance buffer; v2 null-fill at allocation (no init_ends); v3 AVX2 fill + pad-only; v4 one-pass ends-free termination;
v5 kernel without scalar tail; v6 32-byte extend compare + AVX2 min pass; v7 wavefronts sized by need (memory); v8 trim within input hull;
v9 kernel inlined; v10 AVX-512VL bounds check + 3-in-1 alloc; v11 vector reduce scan; v15 begin-free init fill+ramp; v17 one pad-fill dispatch;
v18 compute step specialised on memory mode; v21 arrays capped by max steps (memory); v22 smaller initial arena (memory); v23 round-to-16 sizes (memory).

## Validation round (2026-10-24, after the 25 iterations)
Tooling: WFA2-lib C built in ./WFA2-lib (make lib_wfa; make -C tools/align_benchmark); tests/diff_vs_c.sh = 1710-config
differential matrix vs C (5 datasets x 7 penalty sets x 3 modes x 4 spans x 6 heuristics). Scratch copy with ROOT/R_BIN
env overrides was used to run it for other binaries. Memory-poison debugging: fill arena chunks on reset/new (env hook),
leak+poison or mprotect(PROT_NONE) old heap buffers on Wavefront::resize, then gdb for the faulting backtrace.
- Per-commit sweep: all 17 taken commits pass 230 cargo tests + 60 C-reference cases.
- flexalign's exact config (0,10,12,3; WF-adaptive 10/50/1; ends-free spans; max-steps 60/151/1501): 96/96 identical to C.
- REGRESSION found & fixed (v24): resize_null_victim proposes [lo-inc/2, hi+inc/2]; for a 1-wide wavefront inc/2 = 0 so lo-1 is
  outside the (reallocated) null wavefront -> kernel reads one element before the buffer (freed memory). Same arithmetic in C
  (latent there too); reachable in practice only after the range was re-centred far away, which v22's +-256 initial range made
  common (utest pair 263: 600 vs 1200 bp, then pair 264). Fix: range always covers [lo-1, hi+1], never shrinks. Output-neutral.
  Showed up as 10 WF-adaptive utest configs differing from C/v0 (and failing --check).
- MEMORY REGRESSION found & fixed (v24): 1.5x headroom on every Tight allocation (v7) + 64 KB fixed chunks (v22) gave 1.8x v0's
  peak RSS on utest CIGAR configs (affine2p ends-free 2.4 -> 4.2 GB). Now headroom only <= 1024 elements and chunks grow
  geometrically (64 KB .. 8 MB): peak RSS <= v0 everywhere measured; 150bp footprint unchanged.
- Pre-existing differences vs C (also in v0, NOT from the loop; 649 of 1710 configs): mostly match-reward penalties (-2,3,1,4),
  penalties 0,3,1,4, ends-free with only end-free, X/Z-drop, banded; 236 of them are Rust crashes (segfault 139 / abort 134) or
  hangs (124) in v0 as well, e.g. gap-affine + banded-static on utest, X-drop on utest. Worth a separate bug-fixing effort.

- v24 committed: 213+19 cargo tests, 60/60 C-reference, 1710-config matrix == v0's failure set, flexalign A/B 1.009x (noise). Best binary = v24.

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
