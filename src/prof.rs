//! Cycle counters per alignment phase (feature `phase-timing`, for profiling
//! where `perf` is unavailable). Without the feature, `phase!` is free.

#[cfg(feature = "phase-timing")]
pub static CYCLES: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];
pub const NAMES: [&str; 8] = ["init", "extend", "compute", "c.setup", "c.alloc", "c.kernel", "c.trim", "terminate"];

#[macro_export]
macro_rules! phase {
    ($id:expr, $e:expr) => {{
        #[cfg(feature = "phase-timing")]
        let t0 = unsafe { core::arch::x86_64::_rdtsc() };
        let r = $e;
        #[cfg(feature = "phase-timing")]
        $crate::prof::CYCLES[$id].fetch_add(unsafe { core::arch::x86_64::_rdtsc() } - t0, std::sync::atomic::Ordering::Relaxed);
        r
    }};
}

/// Print accumulated cycles per phase to stderr.
pub fn report() {
    #[cfg(feature = "phase-timing")]
    {
        let total: u64 = CYCLES.iter().take(3).chain(CYCLES.iter().skip(7)).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).sum();
        for (n, c) in NAMES.iter().zip(&CYCLES) {
            let c = c.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!("phase {n:10} {:8.1} Mcyc {:5.1}%", c as f64 / 1e6, 100.0 * c as f64 / total.max(1) as f64);
        }
    }
}
