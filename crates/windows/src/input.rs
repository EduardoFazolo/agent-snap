//! Global, listen-only input tap built on `WH_MOUSE_LL` / `WH_KEYBOARD_LL`.
//!
//! Low-level hooks must live on a thread that pumps messages, so `start` spawns a dedicated
//! thread that installs both hooks and runs `GetMessageW` until `stop` posts `WM_QUIT`.
//! Every hook invocation ends in `CallNextHookEx`; nothing is ever swallowed.

use std::sync::mpsc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_snap_core::platform::{InputTap, MouseButton, RawInput, RawKind};
use anyhow::{anyhow, Context, Result};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, GetKeyState, GetKeyboardLayout, MapVirtualKeyExW, ToUnicodeEx, HKL,
    MAPVK_VK_TO_CHAR, VK_APPS, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24,
    VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_NUMLOCK, VK_PRIOR,
    VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VK_SNAPSHOT, VK_SPACE,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics,
    GetWindowThreadProcessId, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK,
    KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, SM_CXDOUBLECLK, SM_CYDOUBLECLK, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::dpi;

/// Height of one "line" of wheel scrolling, in points. Windows reports wheel motion in lines
/// (WHEEL_DELTA = 3 lines by default); the recorder wants a distance in points.
const LINE_POINTS: f64 = 16.0;
const LINES_PER_NOTCH: f64 = 3.0;

/// Shared between `start`/`stop` and the hook procedures (which are plain functions).
struct HookState {
    on_input: Box<dyn FnMut(RawInput) + Send>,
    scale: f64,
    /// Bit mask of buttons currently held: 1 = left, 2 = right, 4 = other.
    buttons: u8,
    last_down: Option<(Instant, MouseButton, i32, i32)>,
    click_count: u32,
    double_click_time: Duration,
    double_click_half: (i32, i32),
}

static STATE: Mutex<Option<HookState>> = Mutex::new(None);

pub struct LowLevelHookTap {
    thread: Option<(u32, JoinHandle<()>)>,
}

impl LowLevelHookTap {
    pub fn new() -> Self {
        Self { thread: None }
    }
}

impl Default for LowLevelHookTap {
    fn default() -> Self {
        Self::new()
    }
}

impl InputTap for LowLevelHookTap {
    fn start(&mut self, on_input: Box<dyn FnMut(RawInput) + Send>) -> Result<()> {
        self.stop();
        dpi::ensure_dpi_aware();
        // SAFETY: plain Win32 calls with no pointers.
        let (dbl_ms, cx, cy) = unsafe { (GetDoubleClickTime(), GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)) };
        *lock_state() = Some(HookState {
            on_input,
            scale: dpi::primary_scale(),
            buttons: 0,
            last_down: None,
            click_count: 0,
            double_click_time: Duration::from_millis(dbl_ms.max(1) as u64),
            double_click_half: ((cx / 2).max(1), (cy / 2).max(1)),
        });

        let (tx, rx) = mpsc::channel::<Result<u32>>();
        let handle = std::thread::Builder::new()
            .name("agent-snap-input-hooks".into())
            .spawn(move || hook_thread(tx))
            .context("spawn hook thread")?;
        match rx.recv() {
            Ok(Ok(tid)) => {
                self.thread = Some((tid, handle));
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                *lock_state() = None;
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                *lock_state() = None;
                Err(anyhow!("hook thread exited before reporting"))
            }
        }
    }

    fn stop(&mut self) {
        if let Some((tid, handle)) = self.thread.take() {
            // SAFETY: `tid` is the id of a thread we own that pumps messages.
            if let Err(e) = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) } {
                log::warn!("PostThreadMessageW(WM_QUIT): {e}");
            }
            let _ = handle.join();
        }
        *lock_state() = None;
    }
}

impl Drop for LowLevelHookTap {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock_state() -> std::sync::MutexGuard<'static, Option<HookState>> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn hook_thread(ready: mpsc::Sender<Result<u32>>) {
    // SAFETY: GetModuleHandleW(None) returns the executable's module; the hooks are in-process
    // (thread id 0 = global) so the module handle is only informational for LL hooks.
    let installed = unsafe {
        let hmod = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).ok();
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0);
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hmod, 0);
        match (mouse, keyboard) {
            (Ok(m), Ok(k)) => Ok((m, k)),
            (Ok(m), Err(e)) => {
                let _ = UnhookWindowsHookEx(m);
                Err(anyhow!("WH_KEYBOARD_LL: {e}"))
            }
            (Err(e), Ok(k)) => {
                let _ = UnhookWindowsHookEx(k);
                Err(anyhow!("WH_MOUSE_LL: {e}"))
            }
            (Err(e), Err(_)) => Err(anyhow!("WH_MOUSE_LL: {e}")),
        }
    };
    let (mouse, keyboard): (HHOOK, HHOOK) = match installed {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: no arguments.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a live out-pointer; no window filter.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // WM_QUIT (0) or error (-1)
        }
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: handles came from SetWindowsHookExW on this thread.
    unsafe {
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
    }
}

fn emit(st: &mut HookState, kind: RawKind, px: i32, py: i32, click_count: u32, t: Instant) {
    let ev = RawInput { t, kind, x: px as f64 / st.scale, y: py as f64 / st.scale, click_count };
    (st.on_input)(ev);
}

fn held_button(mask: u8) -> Option<MouseButton> {
    if mask & 1 != 0 {
        Some(MouseButton::Left)
    } else if mask & 2 != 0 {
        Some(MouseButton::Right)
    } else if mask & 4 != 0 {
        Some(MouseButton::Other)
    } else {
        None
    }
}

fn button_bit(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Other => 4,
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let t = Instant::now();
        // SAFETY: for WH_MOUSE_LL with code == HC_ACTION, lparam points to a MSLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let (px, py) = (info.pt.x, info.pt.y);
        // Never panic inside a hook: a poisoned lock or a callback panic must not take the hook chain down.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(st) = lock_state().as_mut() {
                handle_mouse(st, wparam.0 as u32, info, px, py, t);
            }
        }));
    }
    // SAFETY: forwarding the same arguments we received.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn handle_mouse(st: &mut HookState, msg: u32, info: &MSLLHOOKSTRUCT, px: i32, py: i32, t: Instant) {
    let down = |st: &mut HookState, b: MouseButton| {
        let count = match st.last_down {
            Some((lt, lb, lx, ly))
                if lb == b
                    && t.duration_since(lt) <= st.double_click_time
                    && (px - lx).abs() <= st.double_click_half.0
                    && (py - ly).abs() <= st.double_click_half.1 =>
            {
                st.click_count + 1
            }
            _ => 1,
        };
        st.click_count = count;
        st.last_down = Some((t, b, px, py));
        st.buttons |= button_bit(b);
        emit(st, RawKind::Down(b), px, py, count, t);
    };
    let up = |st: &mut HookState, b: MouseButton| {
        st.buttons &= !button_bit(b);
        emit(st, RawKind::Up(b), px, py, 1, t);
    };
    match msg {
        WM_MOUSEMOVE => {
            let kind = match held_button(st.buttons) {
                Some(b) => RawKind::Drag(b),
                None => RawKind::Move,
            };
            emit(st, kind, px, py, 1, t);
        }
        WM_LBUTTONDOWN => down(st, MouseButton::Left),
        WM_LBUTTONUP => up(st, MouseButton::Left),
        WM_RBUTTONDOWN => down(st, MouseButton::Right),
        WM_RBUTTONUP => up(st, MouseButton::Right),
        WM_MBUTTONDOWN | WM_XBUTTONDOWN => down(st, MouseButton::Other),
        WM_MBUTTONUP | WM_XBUTTONUP => up(st, MouseButton::Other),
        WM_MOUSEWHEEL => {
            // HIWORD(mouseData) is a signed delta; positive = wheel rotated forward = content scrolls up,
            // and the contract wants positive when the user scrolls down.
            let delta = (info.mouseData >> 16) as u16 as i16 as f64;
            let dy = -(delta / 120.0) * LINES_PER_NOTCH * LINE_POINTS;
            if dy != 0.0 {
                emit(st, RawKind::Scroll { dy }, px, py, 1, t);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
            let t = Instant::now();
            // SAFETY: for WH_KEYBOARD_LL with code == HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let (vk, scan) = (info.vkCode, info.scanCode);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(key) = describe_key(vk, scan) {
                    let mut pt = POINT::default();
                    // SAFETY: `pt` is a live out-pointer.
                    let _ = unsafe { GetCursorPos(&mut pt) };
                    if let Some(st) = lock_state().as_mut() {
                        emit(st, RawKind::KeyDown { label: key.0, printable: key.1 }, pt.x, pt.y, 1, t);
                    }
                }
            }));
        }
    }
    // SAFETY: forwarding the same arguments we received.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn is_down(vk: u16) -> bool {
    // SAFETY: plain Win32 call.
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

fn is_toggled(vk: u16) -> bool {
    // SAFETY: plain Win32 call.
    (unsafe { GetKeyState(vk as i32) } as u16) & 0x0001 != 0
}

fn foreground_layout() -> Option<HKL> {
    // SAFETY: plain Win32 calls; a null foreground window yields thread id 0 = current thread's layout.
    unsafe {
        let hwnd = GetForegroundWindow();
        let tid = if hwnd.0.is_null() { 0 } else { GetWindowThreadProcessId(hwnd, None) };
        Some(GetKeyboardLayout(tid))
    }
}

/// Human label ("Ctrl+Shift+S", "Enter", "a") and the character the key inserts, if any.
/// Returns `None` for bare modifier presses, which are not key-down events the recorder cares about.
fn describe_key(vk: u32, scan: u32) -> Option<(String, Option<char>)> {
    let vk16 = vk as u16;
    if matches!(
        vk16,
        v if v == VK_SHIFT.0 || v == VK_LSHIFT.0 || v == VK_RSHIFT.0
            || v == VK_CONTROL.0 || v == VK_LCONTROL.0 || v == VK_RCONTROL.0
            || v == VK_MENU.0 || v == VK_LMENU.0 || v == VK_RMENU.0
            || v == VK_LWIN.0 || v == VK_RWIN.0
            || v == VK_CAPITAL.0 || v == VK_NUMLOCK.0 || v == VK_SCROLL.0
    ) {
        return None;
    }

    let ctrl = is_down(VK_CONTROL.0);
    let alt = is_down(VK_MENU.0);
    let shift = is_down(VK_SHIFT.0);
    let win = is_down(VK_LWIN.0) || is_down(VK_RWIN.0);
    // AltGr arrives as Ctrl+Alt on most layouts and produces text rather than a shortcut.
    let altgr = ctrl && alt && is_down(VK_RMENU.0);
    let shortcut = (ctrl || alt || win) && !altgr;

    let hkl = foreground_layout();
    let printable = if shortcut { None } else { typed_char(vk, scan, hkl, shift, ctrl, alt) };
    let special = special_name(vk16);

    let mut label = String::new();
    if ctrl && !altgr {
        label.push_str("Ctrl+");
    }
    if alt && !altgr {
        label.push_str("Alt+");
    }
    if win {
        label.push_str("Win+");
    }
    match (special, printable) {
        (Some(name), _) => {
            if shift {
                label.push_str("Shift+");
            }
            label.push_str(name);
        }
        (None, Some(c)) if !shortcut => label.push(c),
        _ => {
            if shift {
                label.push_str("Shift+");
            }
            // SAFETY: plain Win32 call.
            let base = unsafe { MapVirtualKeyExW(vk, MAPVK_VK_TO_CHAR, hkl) } & 0x7FFF;
            match char::from_u32(base).filter(|c| !c.is_control() && *c != ' ') {
                Some(c) => label.extend(c.to_uppercase()),
                None => label.push_str(&format!("VK{vk:#04X}")),
            }
        }
    }
    Some((label, printable))
}

fn special_name(vk: u16) -> Option<&'static str> {
    const F_NAMES: [&str; 24] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16", "F17",
        "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    if (VK_F1.0..=VK_F24.0).contains(&vk) {
        return Some(F_NAMES[(vk - VK_F1.0) as usize]);
    }
    let name = match vk {
        v if v == VK_RETURN.0 => "Enter",
        v if v == VK_TAB.0 => "Tab",
        v if v == VK_ESCAPE.0 => "Escape",
        v if v == VK_BACK.0 => "Backspace",
        v if v == VK_DELETE.0 => "Delete",
        v if v == VK_SPACE.0 => "Space",
        v if v == VK_LEFT.0 => "Left",
        v if v == VK_RIGHT.0 => "Right",
        v if v == VK_UP.0 => "Up",
        v if v == VK_DOWN.0 => "Down",
        v if v == VK_HOME.0 => "Home",
        v if v == VK_END.0 => "End",
        v if v == VK_PRIOR.0 => "PageUp",
        v if v == VK_NEXT.0 => "PageDown",
        v if v == VK_INSERT.0 => "Insert",
        v if v == VK_SNAPSHOT.0 => "PrintScreen",
        v if v == VK_APPS.0 => "Menu",
        _ => return None,
    };
    Some(name)
}

/// Translate a key press into the character it types using the foreground window's layout.
/// Uses flag 0x4 (Windows 10 1607+) so dead-key state of the real input queue is left untouched.
fn typed_char(vk: u32, scan: u32, hkl: Option<HKL>, shift: bool, ctrl: bool, alt: bool) -> Option<char> {
    let mut state = [0u8; 256];
    if shift {
        state[VK_SHIFT.0 as usize] = 0x80;
    }
    if ctrl {
        state[VK_CONTROL.0 as usize] = 0x80;
    }
    if alt {
        state[VK_MENU.0 as usize] = 0x80;
    }
    if is_toggled(VK_CAPITAL.0) {
        state[VK_CAPITAL.0 as usize] = 0x01;
    }
    let mut buf = [0u16; 8];
    // SAFETY: buffers are live locals of the advertised sizes.
    let n = unsafe { ToUnicodeEx(vk, scan, &state, &mut buf, 0x4, hkl) };
    if n <= 0 {
        return None; // dead key (n < 0) or no translation
    }
    let s = String::from_utf16_lossy(&buf[..n as usize]);
    s.chars().last().filter(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_F12;

    #[test]
    fn special_names_cover_navigation_keys() {
        assert_eq!(special_name(VK_RETURN.0), Some("Enter"));
        assert_eq!(special_name(VK_F12.0), Some("F12"));
        assert_eq!(special_name(VK_PRIOR.0), Some("PageUp"));
        assert_eq!(special_name(0x41), None);
    }

    #[test]
    fn button_mask_prefers_left() {
        assert_eq!(held_button(0), None);
        assert_eq!(held_button(1 | 2), Some(MouseButton::Left));
        assert_eq!(held_button(4), Some(MouseButton::Other));
    }
}
