//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
mod backreads;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod cardmeter;
#[cfg(all(windows, feature = "diagnostics"))]
mod renderdoc;
#[cfg(all(windows, feature = "diagnostics"))]
mod shot;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod drvwatch;
#[cfg(windows)]
mod fpsbox;
#[cfg(windows)]
mod spread;
#[cfg(windows)]
mod draw;
pub mod dxgi;
pub mod fence;
pub mod frame;
#[cfg(windows)]
pub mod framelog;
#[cfg(windows)]
mod heap_worker;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
pub mod list;
#[cfg(windows)]
pub mod upscale;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod plaincopy;
#[cfg(windows)]
mod primitive;
#[cfg(all(windows, feature = "diagnostics"))]
mod profile;
pub mod query;
pub mod queue;
#[cfg(windows)]
mod queue2;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod tiles;
pub mod signature;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod texback;

#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_refreshed, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use watch::{
    title_memory_committed, title_memory_reading, title_memory_remapped, title_memory_remapping, title_memory_written,
    writing as kernel_will_write,
};

/// This crate's unit tests run one at a time, as fork commit 89e4127 runs
/// them in CI: on Microsoft's software adapter (no card, or a card the tests
/// do not use) its shader compiler has crashed the test program (access
/// violation in d3d10warp) when several tests used it at once. Set before
/// `main` so libtest reads it; `--test-threads` or an explicit
/// `RUST_TEST_THREADS` still win.
#[cfg(all(test, windows))]
mod one_test_at_a_time {
    extern "C" fn set() {
        if std::env::var_os("RUST_TEST_THREADS").is_none() {
            // SAFETY: before `main`, no other thread reads the environment.
            unsafe { std::env::set_var("RUST_TEST_THREADS", "1") };
        }
    }

    // The C runtime calls the initializers in `.CRT$XCU` before `main`.
    #[used]
    #[unsafe(link_section = ".CRT$XCU")]
    static SET: extern "C" fn() = set;

    #[test]
    fn tests_run_one_at_a_time() {
        assert!(std::env::var_os("RUST_TEST_THREADS").is_some());
    }
}
