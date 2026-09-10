//! macOS backend: ScreenCaptureKit capture, CGEventTap input, AX window tracking, Vision OCR.
//! See agent_snap_core::platform for the contract.

#![cfg(target_os = "macos")]

mod capture;
mod input;
mod ocr;
mod permissions;
mod util;
mod windows;

pub use capture::MacCapture;
pub use input::MacInputTap;
pub use ocr::VisionOcr;
pub use permissions::MacPermissions;
pub use windows::MacWindowTracker;

use std::sync::Arc;

use agent_snap_core::platform::Backend;

/// Bundle every macOS implementation.
pub fn backend() -> Backend {
    Backend {
        capture: Box::new(MacCapture::new()),
        input: Box::new(MacInputTap::new()),
        windows: Box::new(MacWindowTracker::new()),
        ocr: Arc::new(VisionOcr::new()),
        permissions: Box::new(MacPermissions::new()),
    }
}
