//! Wavefront slab allocator with arena-backed storage.
//!
//! Wavefront offset arrays (and backtrace arrays) are allocated from a
//! `ByteArena` bump allocator instead of per-wavefront `alloc()`, matching C's
//! mm_allocator bump pattern.
//!
//! Wavefront structs live in a second, dedicated arena: densely packed (64 bytes
//! each, so the many look-back wavefronts of affine2p stay in L2) and at stable
//! addresses, so the `*mut Wavefront` pointers kept by the wavefront components
//! stay valid however many wavefronts get allocated.
//!
//! Two modes (as in C's `wavefront_slab`):
//! - `Reuse`: every wavefront has the current (growing) length and freed ones
//!   go to a free list. Used with modular (score-only) wavefronts.
//! - `Tight`: wavefronts are allocated at their requested length; only those of
//!   the initial length are recycled. Used when all wavefronts are kept
//!   (CIGAR mode), so memory follows the wavefronts' actual sizes.

use std::mem::size_of;

use crate::arena::ByteArena;
use crate::offset::wavefront_length;
use crate::wavefront::{Wavefront, WavefrontStatus};

const WF_SLAB_EXPAND_FACTOR: f32 = 1.5;

/// Slab allocation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlabMode {
    /// Keep all wavefronts at the current length; reuse freed ones.
    Reuse,
    /// Allocate wavefronts at their requested length; reuse only initial-length ones.
    Tight,
}

/// Wavefront slab allocator.
pub struct WavefrontSlab {
    /// Whether wavefronts need backtrace vectors.
    allocate_backtrace: bool,
    /// Slab strategy.
    slab_mode: SlabMode,
    /// Initial wavefront element count.
    init_wf_length: i32,
    /// Current wavefront element count (may grow in Reuse mode).
    current_wf_length: i32,
    /// All wavefronts handed out since the last arena reset.
    wavefronts: Vec<*mut Wavefront>,
    /// Free wavefronts (stack).
    free_list: Vec<*mut Wavefront>,
    /// Wavefronts marked `Deallocated` since the last arena reset: their arena
    /// space is dead until `clear()`.
    num_deallocated: usize,
    /// Total memory used in bytes.
    memory_used: u64,
    /// Tight mode, `recycle()`d: wavefronts[recycle_next..recycle_end] are handed
    /// out again in allocation order before anything new is allocated.
    recycle_next: usize,
    recycle_end: usize,
    /// Wavefronts handed out since the last `recycle()`/`clear()`.
    num_handed_out: usize,
    /// Arena for all per-wavefront arrays (offsets, bt_pcigar, bt_prev). No zeroing.
    arena: ByteArena,
    /// Arena for the `Wavefront` structs themselves (stable addresses).
    struct_arena: ByteArena,
}

// Raw pointers to arena memory owned by this slab; never shared between threads.
unsafe impl Send for WavefrontSlab {}

impl WavefrontSlab {
    /// Create a new slab.
    pub fn new(init_wf_length: i32, allocate_backtrace: bool, slab_mode: SlabMode) -> Self {
        // Per-wavefront arena bytes: offsets (+ bt_pcigar + bt_prev).
        let arrays_per_wf = if allocate_backtrace { 3 } else { 1 };
        let bytes_per_wf = (init_wf_length as usize) * 4 * arrays_per_wf;
        // Initial chunk: 64 KB (a 150 bp CIGAR alignment uses ~20 KB of
        // wavefront arrays); the arena adds chunks of this size as needed,
        // or a larger one for a single oversized wavefront.
        let arena_capacity = (bytes_per_wf * 4).clamp(65536, 65536);
        Self {
            allocate_backtrace,
            slab_mode,
            init_wf_length,
            current_wf_length: init_wf_length,
            wavefronts: Vec::with_capacity(128),
            free_list: Vec::with_capacity(32),
            num_deallocated: 0,
            memory_used: 0,
            recycle_next: 0,
            recycle_end: 0,
            num_handed_out: 0,
            arena: ByteArena::new(arena_capacity),
            struct_arena: ByteArena::new(256 * size_of::<Wavefront>()),
        }
    }

    /// Kept for API compatibility: wavefront addresses are stable, so nothing
    /// needs reserving.
    pub fn reserve(&mut self, _num_wavefronts: usize) {}

    /// Enable cache coloring on the arena.
    pub fn set_cache_coloring(&mut self, n_colors: u32) {
        self.arena.set_cache_coloring(n_colors);
    }

    /// Get the slab mode.
    pub fn mode(&self) -> SlabMode {
        self.slab_mode
    }

    /// Set the slab mode. All wavefronts are released if it changes.
    pub fn set_mode(&mut self, mode: SlabMode) {
        if mode != self.slab_mode {
            self.slab_mode = mode;
            self.current_wf_length = self.init_wf_length;
            self.clear();
        }
    }

    /// Allocate a wavefront for the diagonal range [min_lo, max_hi].
    ///
    /// The returned pointer stays valid until `clear()`/`reap()` (or until the
    /// wavefront is freed and handed out again).
    pub fn allocate_ptr(&mut self, min_lo: i32, max_hi: i32) -> *mut Wavefront {
        let wf = self.allocate_raw(wavefront_length(min_lo, max_hi));
        unsafe { (*wf).init(min_lo, max_hi) };
        wf
    }

    /// Allocate an output wavefront for `[min_lo, max_hi]` whose `[lo, hi]`
    /// the caller writes in full: only the pads are nulled (see
    /// `Wavefront::init_output`).
    #[inline(always)]
    pub fn allocate_output_ptr(&mut self, min_lo: i32, max_hi: i32, lo: i32, hi: i32) -> *mut Wavefront {
        let wf = self.allocate_raw(wavefront_length(min_lo, max_hi));
        unsafe { (*wf).init_output(min_lo, max_hi, lo, hi) };
        wf
    }

    /// `N` output wavefronts for the same range in one call (see
    /// `allocate_output_ptr`): Tight mode takes the next `N` recycled
    /// wavefronts in one scan.
    #[inline(always)]
    pub fn allocate_output_ptrs<const N: usize>(&mut self, min_lo: i32, max_hi: i32, lo: i32, hi: i32) -> [*mut Wavefront; N] {
        let wf_length = wavefront_length(min_lo, max_hi);
        let mut out = [std::ptr::null_mut(); N];
        if self.slab_mode == SlabMode::Tight && self.recycle_next + N <= self.recycle_end {
            let mut ok = true;
            for (i, slot) in out.iter_mut().enumerate() {
                let wf = unsafe { *self.wavefronts.get_unchecked(self.recycle_next + i) };
                ok &= unsafe { (*wf).wf_elements_allocated } >= wf_length;
                *slot = wf;
            }
            if ok {
                self.recycle_next += N;
                self.num_handed_out += N;
                for &wf in &out {
                    unsafe { (*wf).status = WavefrontStatus::Busy };
                }
            } else {
                for slot in out.iter_mut() {
                    *slot = self.allocate_raw(wf_length);
                }
            }
        } else {
            for slot in out.iter_mut() {
                *slot = self.allocate_raw(wf_length);
            }
        }
        // Same range for all outputs: set the fields, then null the pads of
        // all N arrays with one length dispatch per side.
        unsafe {
            for &wf in &out {
                (*wf).init_output_fields(min_lo, max_hi, lo, hi);
            }
            let lo_ptrs = out.map(|wf| (*wf).slot_ptr(min_lo));
            crate::wavefront::fill_null_n(lo_ptrs, (lo - min_lo) as usize);
            let hi_ptrs = out.map(|wf| (*wf).slot_ptr(hi + 1));
            crate::wavefront::fill_null_n(hi_ptrs, (max_hi - hi) as usize);
        }
        out
    }

    /// Hand out a busy wavefront with at least `wf_length` elements; the
    /// caller initializes it.
    #[inline(always)]
    fn allocate_raw(&mut self, wf_length: i32) -> *mut Wavefront {
        match self.slab_mode {
            SlabMode::Reuse => {
                // Grow current_wf_length if needed
                if wf_length > self.current_wf_length {
                    self.current_wf_length = (wf_length as f32 * WF_SLAB_EXPAND_FACTOR) as i32;
                    self.reap_free();
                }
                // Reuse a free wavefront. Wavefronts handed out before the last
                // growth come back (via `reap_cigar`) too small: retire those.
                while let Some(wf) = self.free_list.pop() {
                    if unsafe { (*wf).wf_elements_allocated } < wf_length {
                        self.retire(wf);
                        continue;
                    }
                    return Self::set_busy(wf);
                }
                self.allocate_new(self.current_wf_length)
            }
            SlabMode::Tight => {
                self.num_handed_out += 1;
                // Alignments of similar sequences request similar wavefronts in
                // the same order: hand out the previous alignment's wavefronts.
                while self.recycle_next < self.recycle_end {
                    let wf = self.wavefronts[self.recycle_next];
                    self.recycle_next += 1;
                    if unsafe { (*wf).wf_elements_allocated } >= wf_length {
                        return Self::set_busy(wf);
                    }
                }
                while let Some(wf) = self.free_list.pop() {
                    if unsafe { (*wf).wf_elements_allocated } >= wf_length {
                        return Self::set_busy(wf);
                    }
                    self.retire(wf);
                }
                self.allocate_new(Self::tight_alloc_length(wf_length))
            }
        }
    }

    /// Free (return) a wavefront to the slab. Null pointers are ignored.
    ///
    /// `wf` must come from this slab's `allocate_ptr` and not be freed already.
    #[inline(always)]
    pub(crate) fn free_ptr(&mut self, wf: *mut Wavefront) {
        if wf.is_null() {
            return;
        }
        let wf_length = unsafe { (*wf).wf_elements_allocated };
        // Use >= for Reuse: oversized wavefronts can serve smaller future requests.
        // Tight: any wavefront can serve a request that fits (checked on pop).
        let repurpose = match self.slab_mode {
            SlabMode::Reuse => wf_length >= self.current_wf_length,
            SlabMode::Tight => true,
        };
        if repurpose {
            unsafe { (*wf).status = WavefrontStatus::Free };
            self.free_list.push(wf);
        } else {
            self.retire(wf);
        }
    }

    /// Put every live wavefront back on the free list, keeping the arena (and
    /// so the arrays' memory) for the next alignment.
    ///
    /// Falls back to `clear()` when that would keep dead arena space: in Tight
    /// mode (wavefronts of arbitrary length are not recycled) or when wavefronts
    /// were retired.
    pub fn reap_cigar(&mut self) {
        if self.slab_mode == SlabMode::Tight || self.num_deallocated > 0 {
            self.clear();
            return;
        }
        self.free_list.clear();
        for &wf in &self.wavefronts {
            unsafe {
                if (*wf).status != WavefrontStatus::Deallocated {
                    (*wf).status = WavefrontStatus::Free;
                    self.free_list.push(wf);
                }
            }
        }
    }

    /// Tight mode: start a new alignment reusing every wavefront of the previous
    /// ones in order (no arena reset, so no struct/array set-up). Falls back to
    /// `clear()` when the slab holds far more wavefronts than the last alignment
    /// used, so memory stays bounded by the largest recent alignment.
    pub fn recycle(&mut self) {
        let len = self.wavefronts.len();
        if self.slab_mode != SlabMode::Tight || self.num_deallocated > 0 || len > 2 * self.num_handed_out + 256 {
            self.clear();
            return;
        }
        self.free_list.clear();
        self.recycle_next = 0;
        self.recycle_end = len;
        self.num_handed_out = 0;
    }

    /// Back to the initial length; release all wavefronts.
    pub fn reap(&mut self) {
        self.current_wf_length = self.init_wf_length;
        self.clear();
    }

    /// Release all wavefronts and reset the arenas (invalidates all pointers).
    pub fn clear(&mut self) {
        // All arrays and structs are arena-backed: Wavefront::drop has nothing
        // to free, so the arena resets reclaim everything.
        self.wavefronts.clear();
        self.free_list.clear();
        self.num_deallocated = 0;
        self.memory_used = 0;
        self.recycle_next = 0;
        self.recycle_end = 0;
        self.num_handed_out = 0;
        self.arena.reset();
        self.struct_arena.reset();
        if self.slab_mode == SlabMode::Tight {
            self.current_wf_length = self.init_wf_length;
        }
    }

    /// Ensure the slab's current wavefront length covers the given diagonal range.
    pub fn ensure_min_length(&mut self, min_lo: i32, max_hi: i32) {
        let wf_length = wavefront_length(min_lo, max_hi);
        if wf_length > self.current_wf_length {
            self.current_wf_length = (wf_length as f32 * WF_SLAB_EXPAND_FACTOR) as i32;
            self.reap_free();
        }
    }

    /// Get total memory used.
    pub fn get_size(&self) -> u64 {
        self.memory_used
    }

    /// Get the number of wavefronts handed out since the last reset.
    pub fn num_wavefronts(&self) -> usize {
        self.wavefronts.len()
    }

    /// Get the number of free wavefronts.
    pub fn num_free(&self) -> usize {
        self.free_list.len()
    }

    // --- Internal ---

    /// Tight mode allocation size for a request of `wf_length` elements:
    /// 50% headroom (so the next alignment's slightly wider wavefront can
    /// recycle it), rounded up to 32 elements. Sizing by need instead of a
    /// fixed 1000 elements keeps memory proportional to the wavefronts and
    /// avoids placing every array at the same 4 KB page offset.
    #[inline(always)]
    fn tight_alloc_length(wf_length: i32) -> i32 {
        ((wf_length + wf_length / 2 + 31) & !31).max(64)
    }

    #[inline(always)]
    fn set_busy(wf: *mut Wavefront) -> *mut Wavefront {
        unsafe { (*wf).status = WavefrontStatus::Busy };
        wf
    }

    fn retire(&mut self, wf: *mut Wavefront) {
        unsafe {
            self.memory_used -= (*wf).get_size();
            (*wf).status = WavefrontStatus::Deallocated;
        }
        self.num_deallocated += 1;
    }

    fn allocate_new(&mut self, wf_length: i32) -> *mut Wavefront {
        let mut wf =
            Wavefront::allocate_from_arena(&mut self.arena, wf_length, self.allocate_backtrace);
        wf.status = WavefrontStatus::Busy;
        self.memory_used += wf.get_size();
        // ByteArena hands out 8-byte aligned memory, enough for Wavefront.
        const _: () = assert!(std::mem::align_of::<Wavefront>() <= 8);
        let ptr = self.struct_arena.alloc_bytes(size_of::<Wavefront>()) as *mut Wavefront;
        unsafe { ptr.write(wf) };
        self.wavefronts.push(ptr);
        ptr
    }

    /// Filter the free list after a `current_wf_length` growth: retire the free
    /// wavefronts that are now too small. Busy wavefronts are left alone.
    fn reap_free(&mut self) {
        let current = self.current_wf_length;
        let mut free_list = std::mem::take(&mut self.free_list);
        free_list.retain(|&wf| {
            if unsafe { (*wf).wf_elements_allocated } >= current {
                true
            } else {
                self.retire(wf);
                false
            }
        });
        self.free_list = free_list;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_slab() {
        let slab = WavefrontSlab::new(100, false, SlabMode::Reuse);
        assert_eq!(slab.num_wavefronts(), 0);
        assert_eq!(slab.num_free(), 0);
    }

    #[test]
    fn test_allocate_and_access() {
        let mut slab = WavefrontSlab::new(100, false, SlabMode::Reuse);
        let wf = unsafe { &mut *slab.allocate_ptr(-5, 5) };
        assert_eq!(wf.status, WavefrontStatus::Busy);
        assert!(!wf.null);
        wf.set_offset(0, 42);
        assert_eq!(wf.get_offset(0), 42);
    }

    #[test]
    fn test_free_and_reuse() {
        let mut slab = WavefrontSlab::new(100, false, SlabMode::Reuse);
        let p1 = slab.allocate_ptr(-5, 5);
        assert_eq!(slab.num_wavefronts(), 1);
        assert_eq!(slab.num_free(), 0);

        slab.free_ptr(p1);
        assert_eq!(slab.num_free(), 1);

        // Next allocation should reuse the free wavefront
        let p2 = slab.allocate_ptr(-3, 3);
        assert_eq!(p2, p1);
        assert_eq!(slab.num_wavefronts(), 1);
        assert_eq!(slab.num_free(), 0);
    }

    #[test]
    fn test_pointers_stable_across_many_allocations() {
        let mut slab = WavefrontSlab::new(20, false, SlabMode::Reuse);
        let ptrs: Vec<_> = (0..1000).map(|i| {
            let p = slab.allocate_ptr(-5, 5);
            unsafe { (*p).set_offset(0, i) };
            p
        }).collect();
        for (i, &p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { (*p).get_offset(0) }, i as i32);
        }
    }

    #[test]
    fn test_reuse_grows_and_retires_small() {
        let mut slab = WavefrontSlab::new(10, false, SlabMode::Reuse);
        let small = slab.allocate_ptr(-2, 2);
        slab.free_ptr(small);
        // Larger request: the small free wavefront must not be handed out
        let big = slab.allocate_ptr(-50, 50);
        assert_ne!(big, small);
        assert!(unsafe { (*big).wf_elements_allocated } >= 101);
    }

    #[test]
    fn test_reap_cigar_never_returns_undersized() {
        let mut slab = WavefrontSlab::new(10, false, SlabMode::Reuse);
        let _small = slab.allocate_ptr(-2, 2);
        let _big = slab.allocate_ptr(-50, 50); // grows current length; `small` stays busy
        slab.reap_cigar();
        for _ in 0..4 {
            let p = slab.allocate_ptr(-50, 50);
            assert!(unsafe { (*p).wf_elements_allocated } >= 101);
        }
    }

    #[test]
    fn test_multiple_allocations() {
        let mut slab = WavefrontSlab::new(20, false, SlabMode::Reuse);
        let ptrs: Vec<_> = (0..10).map(|_| slab.allocate_ptr(-5, 5)).collect();
        assert_eq!(slab.num_wavefronts(), 10);
        for &p in &ptrs {
            slab.free_ptr(p);
        }
        assert_eq!(slab.num_free(), 10);
        // Reallocate — should reuse
        for _ in 0..10 {
            slab.allocate_ptr(-5, 5);
        }
        assert_eq!(slab.num_wavefronts(), 10);
        assert_eq!(slab.num_free(), 0);
    }

    #[test]
    fn test_reap() {
        let mut slab = WavefrontSlab::new(20, false, SlabMode::Reuse);
        for _ in 0..5 {
            slab.allocate_ptr(-5, 5);
        }
        slab.reap();
        assert_eq!(slab.num_wavefronts(), 0);
    }

    #[test]
    fn test_tight_mode() {
        let mut slab = WavefrontSlab::new(20, false, SlabMode::Tight);
        let p = slab.allocate_ptr(-5, 5);
        slab.free_ptr(p);
        assert_eq!(slab.allocate_ptr(-3, 3), p);
    }

    #[test]
    fn test_tight_mode_oversized() {
        let mut slab = WavefrontSlab::new(10, false, SlabMode::Tight);
        let p = slab.allocate_ptr(-10, 10);
        // Sized by need (with headroom), not by the initial length.
        let allocated = unsafe { (*p).wf_elements_allocated };
        assert!((21..=64).contains(&allocated), "allocated={allocated}");
        slab.free_ptr(p);
        assert_eq!(slab.num_free(), 1); // reusable for anything that fits
        assert_eq!(slab.allocate_ptr(-10, 10), p);
        let q = slab.allocate_ptr(-100, 100); // does not fit: new allocation
        assert_ne!(q, p);
        assert!(unsafe { (*q).wf_elements_allocated } >= 201);
    }

    #[test]
    fn test_with_backtrace() {
        let mut slab = WavefrontSlab::new(20, true, SlabMode::Reuse);
        let wf = unsafe { &mut *slab.allocate_ptr(-5, 5) };
        assert!(wf.has_backtrace());
        wf.set_bt_pcigar(0, 0xDEAD);
        assert_eq!(wf.get_bt_pcigar(0), 0xDEAD);
    }

    #[test]
    fn test_clear() {
        let mut slab = WavefrontSlab::new(20, false, SlabMode::Reuse);
        for _ in 0..5 {
            slab.allocate_ptr(-5, 5);
        }
        slab.clear();
        assert_eq!(slab.num_wavefronts(), 0);
        assert_eq!(slab.num_free(), 0);
        let p = slab.allocate_ptr(-5, 5);
        assert_eq!(unsafe { (*p).status }, WavefrontStatus::Busy);
    }
}
