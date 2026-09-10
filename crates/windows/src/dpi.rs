//! DPI helpers shared by the capture, input and window modules.
//!
//! Point <-> pixel conversion uses the primary monitor's effective DPI: `pixels = points * scale`.

use std::sync::Once;

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};

static DPI_AWARE: Once = Once::new();

/// Opt the process into per-monitor-v2 DPI awareness. Idempotent; failure (already set by the
/// host, or an older OS) is ignored because every caller falls back to whatever the OS reports.
pub fn ensure_dpi_aware() {
    DPI_AWARE.call_once(|| {
        // SAFETY: plain Win32 call with a constant argument.
        if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            log::debug!("SetProcessDpiAwarenessContext failed (ignored): {e}");
        }
    });
}

/// The primary monitor (the one containing the origin of the virtual screen).
pub fn primary_monitor() -> HMONITOR {
    // SAFETY: plain Win32 call; MONITOR_DEFAULTTOPRIMARY guarantees a non-null result.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

/// Pixels per point of the primary monitor.
pub fn primary_scale() -> f64 {
    let mut dx = 0u32;
    let mut dy = 0u32;
    // SAFETY: out-pointers reference live locals.
    let dpi = match unsafe { GetDpiForMonitor(primary_monitor(), MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
        Ok(()) if dx > 0 => dx,
        // SAFETY: no arguments.
        _ => unsafe { GetDpiForSystem() },
    };
    if dpi == 0 {
        1.0
    } else {
        dpi as f64 / 96.0
    }
}
