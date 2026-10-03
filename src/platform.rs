//! The few services that differ between a native build and WebAssembly in
//! a browser (`wasm32-unknown-unknown`), where there is no clock, no
//! sleeping and no process to exit. The host page provides the clock.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
mod imp {
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    static START: OnceLock<Instant> = OnceLock::new();

    pub fn now_seconds() -> f64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
    }

    pub fn monotonic_seconds() -> f64 {
        START.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    pub fn sleep(d: std::time::Duration) {
        std::thread::sleep(d);
    }

    pub fn seed() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(42)
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod imp {
    #[link(wasm_import_module = "env")]
    extern "C" {
        /// Milliseconds since the Unix epoch (`Date.now()` in JavaScript).
        fn cogito_host_now_ms() -> f64;
        /// Milliseconds on a monotonic clock (`performance.now()`).
        fn cogito_host_clock_ms() -> f64;
    }

    pub fn now_seconds() -> f64 {
        unsafe { cogito_host_now_ms() / 1000.0 }
    }

    pub fn monotonic_seconds() -> f64 {
        unsafe { cogito_host_clock_ms() / 1000.0 }
    }

    /// A browser page cannot block, so `sleep` returns at once.
    pub fn sleep(_: std::time::Duration) {}

    pub fn seed() -> u64 {
        let ms = unsafe { cogito_host_now_ms() + cogito_host_clock_ms() * 1000.0 };
        (ms as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
    }
}

pub use imp::*;
