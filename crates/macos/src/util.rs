//! Small helpers shared by the macOS backend modules.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFNumber, CFNumberType, CFRetained, CFString, CFType};
use objc2_core_media::CMTime;

/// Wrapper that asserts `T` may cross threads. Used for Objective-C / CoreFoundation handles that
/// Apple documents as thread-safe (SCStream, AXUIElement, CFRunLoop) but which the bindings do not
/// mark `Send`.
pub(crate) struct SendCell<T>(pub T);
// SAFETY: see type docs; every use site keeps the wrapped handle on one logical owner at a time.
unsafe impl<T> Send for SendCell<T> {}
unsafe impl<T> Sync for SendCell<T> {}

pub(crate) fn cf_string(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// Best-effort integer read of a CFNumber.
pub(crate) fn cf_number_i64(n: &CFNumber) -> Option<i64> {
    let mut out: i64 = 0;
    let ok = unsafe { n.value(CFNumberType::SInt64Type, &mut out as *mut i64 as *mut c_void) };
    ok.then_some(out)
}

pub(crate) fn cf_number_f64(n: &CFNumber) -> Option<f64> {
    let mut out: f64 = 0.0;
    let ok = unsafe { n.value(CFNumberType::Float64Type, &mut out as *mut f64 as *mut c_void) };
    ok.then_some(out)
}

/// Render any CF value as a short string (strings, numbers, booleans). None for other types.
pub(crate) fn cf_to_string(v: &CFType) -> Option<String> {
    if let Some(s) = v.downcast_ref::<CFString>() {
        return Some(s.to_string());
    }
    if let Some(b) = v.downcast_ref::<objc2_core_foundation::CFBoolean>() {
        return Some(if b.value() { "true".into() } else { "false".into() });
    }
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        if let Some(i) = cf_number_i64(n) {
            let f = cf_number_f64(n).unwrap_or(i as f64);
            if (f - i as f64).abs() < 1e-9 {
                return Some(i.to_string());
            }
            return Some(format!("{f}"));
        }
    }
    None
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMClockGetHostTimeClock() -> *const c_void;
    fn CMClockGetTime(clock: *const c_void) -> CMTime;
}

/// Map a CoreMedia host-clock timestamp to an `Instant`. Falls back to `now` when the timestamp is
/// not on the host clock (age negative or implausibly large).
pub(crate) fn instant_from_host_time(pts: CMTime) -> Instant {
    let now = Instant::now();
    let host_now = unsafe { CMClockGetTime(CMClockGetHostTimeClock()) };
    let age = unsafe { host_now.seconds() - pts.seconds() };
    if age.is_finite() && age > 0.0 && age < 0.5 {
        now - Duration::from_secs_f64(age)
    } else {
        now
    }
}

pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}
