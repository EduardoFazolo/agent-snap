//! Windows backend for agent-snap. See `agent_snap_core::platform` for the contract.
//!
//! The crate body is compiled only on Windows; on other hosts it is an empty library so the
//! workspace still builds everywhere.
#![cfg(target_os = "windows")]

mod capture;
mod dpi;
mod input;
mod ocr;
mod permissions;
mod windows;

use std::sync::Arc;

use agent_snap_core::platform::Backend;

pub use capture::WgcCapture;
pub use input::LowLevelHookTap;
pub use ocr::WinOcr;
pub use permissions::WinPermissions;
pub use windows::UiaWindowTracker;

/// Build the full Windows backend. Makes the process per-monitor DPI aware as a side effect so
/// that every coordinate the backend reports is consistent with the captured frames.
pub fn backend() -> Backend {
    dpi::ensure_dpi_aware();
    Backend {
        capture: Box::new(WgcCapture::new()),
        input: Box::new(LowLevelHookTap::new()),
        windows: Box::new(UiaWindowTracker::new()),
        ocr: Arc::new(WinOcr::new()),
        permissions: Box::new(WinPermissions),
    }
}
