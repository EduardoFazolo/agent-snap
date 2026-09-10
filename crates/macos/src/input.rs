//! Global listen-only CGEventTap running on its own thread with a CFRunLoop.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_snap_core::platform::{InputTap, MouseButton, RawInput, RawKind};
use anyhow::{bail, Result};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

use crate::util::SendCell;

type InputCallback = Box<dyn FnMut(RawInput) + Send>;

struct TapState {
    cb: InputCallback,
    tap: Option<CFRetained<CFMachPort>>,
}

/// CGEventTap-backed global input listener.
#[derive(Default)]
pub struct MacInputTap {
    thread: Option<JoinHandle<()>>,
    runloop: Option<SendCell<CFRetained<CFRunLoop>>>,
}

impl MacInputTap {
    pub fn new() -> Self {
        Self::default()
    }
}

fn mask_of(types: &[CGEventType]) -> CGEventMask {
    types.iter().fold(0u64, |m, t| m | (1u64 << t.0))
}

/// Human names for non-printable keys (macOS virtual key codes).
pub fn key_name(code: i64) -> Option<&'static str> {
    Some(match code {
        36 | 76 => "Enter",
        48 => "Tab",
        49 => "Space",
        51 => "Backspace",
        53 => "Escape",
        117 => "Delete",
        123 => "Left",
        124 => "Right",
        125 => "Down",
        126 => "Up",
        115 => "Home",
        119 => "End",
        116 => "PageUp",
        121 => "PageDown",
        114 => "Help",
        71 => "Clear",
        122 => "F1",
        120 => "F2",
        99 => "F3",
        118 => "F4",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        100 => "F8",
        101 => "F9",
        109 => "F10",
        103 => "F11",
        111 => "F12",
        105 => "F13",
        107 => "F14",
        113 => "F15",
        106 => "F16",
        64 => "F17",
        79 => "F18",
        80 => "F19",
        90 => "F20",
        _ => return None,
    })
}

/// Build the label ("Cmd+Shift+S", "Enter", "a") and printable char for a key-down.
pub fn key_label(code: i64, flags: CGEventFlags, chars: &str) -> (String, Option<char>) {
    let mut parts: Vec<&str> = Vec::new();
    if flags.contains(CGEventFlags::MaskControl) {
        parts.push("Ctrl");
    }
    if flags.contains(CGEventFlags::MaskAlternate) {
        parts.push("Alt");
    }
    if flags.contains(CGEventFlags::MaskShift) {
        parts.push("Shift");
    }
    if flags.contains(CGEventFlags::MaskCommand) {
        parts.push("Cmd");
    }
    let first = chars.chars().next().filter(|c| (*c as u32) >= 32 && (*c as u32) != 127);
    let named = key_name(code);
    let base: String = match (named, first) {
        (Some(n), _) => n.to_string(),
        (None, Some(_)) => chars.to_uppercase(),
        (None, None) => format!("key{code}"),
    };
    let mut label = parts.join("+");
    if !label.is_empty() {
        label.push('+');
    }
    label.push_str(&base);

    let modifier_held = flags.contains(CGEventFlags::MaskCommand) || flags.contains(CGEventFlags::MaskControl);
    let printable = if modifier_held || (named.is_some() && code != 49) { None } else { first };
    (label, printable)
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    let raw_ptr = event.as_ptr();
    if info.is_null() {
        return raw_ptr;
    }
    // SAFETY: `info` is the Box<TapState> leaked in `start`, freed only after the run loop ends.
    let state = unsafe { &mut *(info as *mut TapState) };
    let ev = unsafe { event.as_ref() };

    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        if let Some(tap) = &state.tap {
            CGEvent::tap_enable(tap, true);
        }
        return raw_ptr;
    }

    let t = Instant::now();
    let loc = CGEvent::location(Some(ev));
    let field = |f: CGEventField| CGEvent::integer_value_field(Some(ev), f);

    let kind = match ty {
        CGEventType::LeftMouseDown => RawKind::Down(MouseButton::Left),
        CGEventType::LeftMouseUp => RawKind::Up(MouseButton::Left),
        CGEventType::RightMouseDown => RawKind::Down(MouseButton::Right),
        CGEventType::RightMouseUp => RawKind::Up(MouseButton::Right),
        CGEventType::OtherMouseDown => RawKind::Down(MouseButton::Other),
        CGEventType::OtherMouseUp => RawKind::Up(MouseButton::Other),
        CGEventType::MouseMoved => RawKind::Move,
        CGEventType::LeftMouseDragged => RawKind::Drag(MouseButton::Left),
        CGEventType::RightMouseDragged => RawKind::Drag(MouseButton::Right),
        CGEventType::OtherMouseDragged => RawKind::Drag(MouseButton::Other),
        CGEventType::ScrollWheel => {
            // Pixel deltas are in points for precise devices; wheels report lines, ~10pt each.
            let mut dy = field(CGEventField::ScrollWheelEventPointDeltaAxis1) as f64;
            if dy == 0.0 {
                dy = CGEvent::double_value_field(Some(ev), CGEventField::ScrollWheelEventFixedPtDeltaAxis1) * 10.0;
            }
            // CG: negative = user scrolls down. Contract: positive = user scrolls down.
            RawKind::Scroll { dy: -dy }
        }
        CGEventType::KeyDown => {
            let code = field(CGEventField::KeyboardEventKeycode);
            let flags = CGEvent::flags(Some(ev));
            let mut buf = [0u16; 8];
            let mut len: usize = 0;
            unsafe {
                CGEvent::keyboard_get_unicode_string(Some(ev), buf.len() as _, &mut len as *mut usize as *mut _, buf.as_mut_ptr());
            }
            let chars = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
            let (label, printable) = key_label(code, flags, &chars);
            RawKind::KeyDown { label, printable }
        }
        _ => return raw_ptr,
    };

    let click_count = match kind {
        RawKind::Down(_) => field(CGEventField::MouseEventClickState).max(1) as u32,
        _ => 1,
    };
    let raw = RawInput { t, kind, x: loc.x, y: loc.y, click_count };
    let _ = catch_unwind(AssertUnwindSafe(|| (state.cb)(raw)));
    raw_ptr
}

impl InputTap for MacInputTap {
    fn start(&mut self, on_input: Box<dyn FnMut(RawInput) + Send>) -> Result<()> {
        self.stop();
        let (tx, rx) = mpsc::channel::<Result<SendCell<CFRetained<CFRunLoop>>, String>>();
        let state_ptr = SendCell(Box::into_raw(Box::new(TapState { cb: on_input, tap: None })));

        let thread = std::thread::Builder::new().name("agent-snap.input".into()).spawn(move || {
            let state_ptr = state_ptr; // move the whole cell
            let sp = state_ptr.0;
            let mask = mask_of(&[
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGEventType::MouseMoved,
                CGEventType::LeftMouseDragged,
                CGEventType::RightMouseDragged,
                CGEventType::OtherMouseDragged,
                CGEventType::ScrollWheel,
                CGEventType::KeyDown,
            ]);
            let tap = unsafe {
                CGEvent::tap_create(
                    CGEventTapLocation::SessionEventTap,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    mask,
                    Some(tap_callback),
                    sp as *mut c_void,
                )
            };
            let Some(tap) = tap else {
                let _ = tx.send(Err(
                    "could not create event tap (grant Accessibility / Input Monitoring to this app)".into(),
                ));
                unsafe { drop(Box::from_raw(sp)) };
                return;
            };
            unsafe { (*sp).tap = Some(tap.clone()) };
            let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
                let _ = tx.send(Err("could not create run loop source".into()));
                unsafe { (*sp).tap = None };
                tap.invalidate();
                unsafe { drop(Box::from_raw(sp)) };
                return;
            };
            let rl = CFRunLoop::current().expect("thread run loop");
            let mode = unsafe { kCFRunLoopCommonModes };
            rl.add_source(Some(&source), mode);
            CGEvent::tap_enable(&tap, true);
            let _ = tx.send(Ok(SendCell(rl.clone())));

            CFRunLoop::run();

            CGEvent::tap_enable(&tap, false);
            rl.remove_source(Some(&source), mode);
            unsafe { (*sp).tap = None };
            tap.invalidate();
            // SAFETY: the tap is invalidated, no more callbacks can reference the state.
            unsafe { drop(Box::from_raw(sp)) };
        })?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(rl)) => {
                self.runloop = Some(rl);
                self.thread = Some(thread);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                bail!(e)
            }
            Err(_) => bail!("event tap thread did not start"),
        }
    }

    fn stop(&mut self) {
        if let Some(SendCell(rl)) = self.runloop.take() {
            rl.stop();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for MacInputTap {
    fn drop(&mut self) {
        self.stop();
    }
}
