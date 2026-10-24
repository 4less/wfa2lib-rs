//! Benchmark shaped like flexalign's use of the aligner.
//!
//! flexalign runs gap-affine (match 0, mismatch 10, gap-open 12, gap-extend 3)
//! with full CIGAR output, the WF-adaptive heuristic (10, 50, 1), a max-score
//! abort derived from a minimum ANI, and ends-free spans for dovetail/overhang
//! anchors on short reads. This binary replays those shapes on deterministic
//! synthetic data and prints one JSON line per workload:
//!
//!   {"workload":..., "pairs":..., "ns_per_pair":..., "peak_heap":..., "checksum":...}
//!
//! `ns_per_pair` is the minimum over `reps` passes. `checksum` folds every
//! score and CIGAR, so two builds that disagree on output are caught.
//!
//! Usage: flexalign_bench [reps] [scale]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use wfa2lib_rs::aligner::{AffineAligner, AlignStatus, AlignmentScope};
use wfa2lib_rs::heuristic::HeuristicStrategy;
use wfa2lib_rs::penalties::{AffinePenalties, WavefrontPenalties};

// ---------------------------------------------------------------------------
// Counting allocator: peak live heap per workload.
// ---------------------------------------------------------------------------

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            let old = layout.size();
            if new_size >= old {
                let live = LIVE.fetch_add(new_size - old, Ordering::Relaxed) + (new_size - old);
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(old - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn reset_peak() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

// ---------------------------------------------------------------------------
// Deterministic data generation.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn base(&mut self) -> u8 {
        b"ACGT"[self.below(4)]
    }
}

fn random_seq(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.base()).collect()
}

/// Mutate `src` with substitutions and short indels at roughly `rate`.
fn mutate(rng: &mut Rng, src: &[u8], rate: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + 8);
    let mut i = 0;
    while i < src.len() {
        if rng.unit() < rate {
            match rng.below(10) {
                0..=6 => {
                    // substitution
                    let mut b = rng.base();
                    while b == src[i] {
                        b = rng.base();
                    }
                    out.push(b);
                    i += 1;
                }
                7 | 8 => {
                    // deletion of 1..3
                    i += 1 + rng.below(3);
                }
                _ => {
                    // insertion of 1..3
                    for _ in 0..(1 + rng.below(3)) {
                        out.push(rng.base());
                    }
                }
            }
        } else {
            out.push(src[i]);
            i += 1;
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Span {
    End2End,
    /// (pattern_begin, pattern_end, text_begin, text_end) free lengths, clamped at align time.
    EndsFree(i32, i32, i32, i32),
}

struct Case {
    pattern: Vec<u8>,
    text: Vec<u8>,
    span: Span,
    max_steps: i32,
}

/// flexalign's abort score: ceil((1 - min_ani) * overlap * mismatch) + 1.
fn ani_abort(min_ani: f64, overlap: usize) -> i32 {
    ((1.0 - min_ani) * overlap as f64 * 10.0).ceil() as i32 + 1
}

fn make_e2e(rng: &mut Rng, n: usize, len: usize, rate: f64, min_ani: f64) -> Vec<Case> {
    (0..n)
        .map(|_| {
            let pattern = random_seq(rng, len);
            let text = mutate(rng, &pattern, rate);
            let max_steps = ani_abort(min_ani, len);
            Case { pattern, text, span: Span::End2End, max_steps }
        })
        .collect()
}

/// Read vs. reference window with overhang on both sides (dovetail-style spans).
fn make_dovetail(rng: &mut Rng, n: usize, len: usize, slack: usize, rate: f64) -> Vec<Case> {
    (0..n)
        .map(|_| {
            let core = random_seq(rng, len);
            let pattern = mutate(rng, &core, rate);
            let mut text = random_seq(rng, slack);
            text.extend_from_slice(&core);
            text.extend(random_seq(rng, slack));
            let s = slack as i32;
            let max_steps = ani_abort(0.90, len);
            Case { pattern, text, span: Span::EndsFree(s, s, 2 * s, 2 * s), max_steps }
        })
        .collect()
}

/// One-sided extension (anchor.rs: `set_ends_free(q_dove, 0, r_dove, 0)`).
fn make_extension(rng: &mut Rng, n: usize, len: usize, dove: usize, rate: f64) -> Vec<Case> {
    (0..n)
        .map(|_| {
            let core = random_seq(rng, len);
            let pattern = mutate(rng, &core, rate);
            let mut text = random_seq(rng, dove / 2);
            text.extend_from_slice(&core);
            let d = dove as i32;
            let max_steps = ani_abort(0.90, len);
            Case { pattern, text, span: Span::EndsFree(d, 0, d, 0), max_steps }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Running.
// ---------------------------------------------------------------------------

fn new_aligner() -> AffineAligner {
    let penalties = WavefrontPenalties::new_affine(AffinePenalties {
        match_: 0,
        mismatch: 10,
        gap_opening: 12,
        gap_extension: 3,
    });
    let mut aligner = AffineAligner::new(penalties);
    aligner.set_alignment_scope(AlignmentScope::ComputeAlignment);
    aligner.set_heuristic(HeuristicStrategy::WfAdaptive {
        min_wavefront_length: 10,
        max_distance_threshold: 50,
        steps_between_cutoffs: 1,
    });
    aligner
}

fn fold(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x100_0000_01b3)
}

fn run_cases(aligner: &mut AffineAligner, cases: &[Case]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for c in cases {
        aligner.set_max_alignment_steps(c.max_steps);
        let score = match c.span {
            Span::End2End => {
                aligner.alignment_span = wfa2lib_rs::aligner::AlignmentSpan::End2End;
                aligner.extension = false;
                aligner.align_end2end(&c.pattern, &c.text)
            }
            Span::EndsFree(pb, pe, tb, te) => {
                let (pl, tl) = (c.pattern.len() as i32, c.text.len() as i32);
                aligner.set_alignment_free_ends(pb.min(pl), pe.min(pl), tb.min(tl), te.min(tl));
                aligner.align_endsfree(&c.pattern, &c.text)
            }
        };
        h = fold(h, score as u64);
        h = fold(h, aligner.status() as u64);
        if aligner.status() != AlignStatus::MaxStepsReached {
            // Fold the CIGAR 8 bytes at a time (a byte-wise fold was ~7% of
            // the measured time on short reads).
            let ops = aligner.cigar().operations_slice();
            let mut chunks = ops.chunks_exact(8);
            for c in &mut chunks {
                h = fold(h, u64::from_le_bytes(c.try_into().unwrap()));
            }
            let mut tail = 0u64;
            for &op in chunks.remainder() {
                tail = (tail << 8) | op as u64;
            }
            h = fold(h, tail ^ (ops.len() as u64) << 56);
        }
    }
    h
}

fn bench(name: &str, cases: &[Case], reps: usize) {
    let before = LIVE.load(Ordering::Relaxed);
    let mut aligner = new_aligner();
    // The aligner's own footprint before any alignment (arena chunks, null/victim wavefronts, ...).
    let aligner_heap = LIVE.load(Ordering::Relaxed) - before;
    // Warm-up (also sizes internal buffers like a long-lived flexalign worker).
    let live0 = reset_peak();
    let checksum = run_cases(&mut aligner, cases);
    let mut best = f64::INFINITY;
    for _ in 0..reps {
        let t0 = Instant::now();
        let h = run_cases(&mut aligner, cases);
        let dt = t0.elapsed().as_secs_f64();
        assert_eq!(h, checksum, "non-deterministic output in {name}");
        best = best.min(dt);
    }
    let peak = PEAK.load(Ordering::Relaxed) - live0;
    println!(
        "{{\"workload\":\"{name}\",\"pairs\":{},\"ns_per_pair\":{:.1},\"peak_heap\":{peak},\"aligner_heap\":{aligner_heap},\"checksum\":\"{checksum:016x}\"}}",
        cases.len(),
        best * 1e9 / cases.len() as f64
    );
    drop(aligner);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let reps: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let scale: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let only: Option<&str> = args.get(3).map(|s| s.as_str());

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // Workloads, in flexalign order of importance.
    let workloads: Vec<(&str, Vec<Case>)> = vec![
        // Illumina-length read vs. its reference window, end-to-end, ~2% divergence.
        ("e2e_150_2pct", make_e2e(&mut rng, 20_000 * scale, 150, 0.02, 0.90)),
        // Same, but the pair is too divergent: the ANI abort fires (Status::Dropped).
        ("e2e_150_drop", make_e2e(&mut rng, 20_000 * scale, 150, 0.15, 0.90)),
        // Dovetail: both ends free, read inside a longer reference window.
        ("dovetail_150", make_dovetail(&mut rng, 20_000 * scale, 150, 30, 0.02)),
        // One-sided overhang extension (anchor extension step).
        ("extend_150", make_extension(&mut rng, 20_000 * scale, 150, 40, 0.02)),
        // Longer pairs (merged / long-read style), 5% divergence.
        ("e2e_1k_5pct", make_e2e(&mut rng, 2_000 * scale, 1000, 0.05, 0.85)),
        // Very similar long pairs: dominated by extend, almost no compute.
        ("e2e_1k_05pct", make_e2e(&mut rng, 2_000 * scale, 1000, 0.005, 0.95)),
    ];

    for (name, cases) in &workloads {
        if let Some(o) = only {
            if o != *name {
                continue;
            }
        }
        bench(name, cases, reps);
    }
    wfa2lib_rs::prof::report();
}
