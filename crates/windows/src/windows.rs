//! Window and accessibility lookups through Win32 + UI Automation.
//!
//! Everything here is best-effort: any failure yields a default, and the UIA descent in
//! `hit_test` is bounded in depth and wall-clock time so it never stalls the recorder.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::time::{Duration, Instant};

use agent_snap_core::platform::{AxTarget, RectI, WindowInfo, WindowTracker};
use anyhow::{Context, Result};
use windows::core::{BSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_I4};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement, IUIAutomationTreeWalker,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId, UIA_DataItemControlTypeId, UIA_EditControlTypeId,
    UIA_HyperlinkControlTypeId, UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId, UIA_NamePropertyId,
    UIA_RadioButtonControlTypeId, UIA_SliderControlTypeId, UIA_SpinnerControlTypeId, UIA_TabItemControlTypeId,
    UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
};

use crate::dpi;

const NAME_CAP: usize = 60;
const MAX_DEPTH: usize = 12;
const MAX_SIBLINGS: usize = 48;
const HIT_TEST_BUDGET: Duration = Duration::from_millis(40);
const URL_CACHE_TTL: Duration = Duration::from_secs(2);

const BROWSERS: [&str; 6] = ["chrome", "msedge", "brave", "firefox", "vivaldi", "opera"];
/// Accessible names browsers give their address bar (Chromium family, Firefox, older Edge).
const ADDRESS_BAR_NAMES: [&str; 5] = [
    "Address and search bar",
    "Address bar",
    "Search or enter web address",
    "Search or enter address",
    "Search with Google or enter address",
];

thread_local! {
    static UIA: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

/// Run `f` with this thread's lazily created `IUIAutomation` instance.
fn with_uia<T>(f: impl FnOnce(&IUIAutomation) -> Result<T>) -> Result<T> {
    UIA.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            // SAFETY: plain COM initialization; RPC_E_CHANGED_MODE (already STA) is fine for CoCreateInstance.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // SAFETY: CUIAutomation is a registered in-proc server.
            let uia: IUIAutomation =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.context("CoCreateInstance(CUIAutomation)")?;
            *slot = Some(uia);
        }
        f(slot.as_ref().expect("initialized above"))
    })
}

pub struct UiaWindowTracker {
    scale: f64,
    url_cache: HashMap<isize, (Instant, Option<String>)>,
}

impl UiaWindowTracker {
    pub fn new() -> Self {
        dpi::ensure_dpi_aware();
        Self { scale: dpi::primary_scale(), url_cache: HashMap::new() }
    }

    fn to_points(&self, r: RECT) -> RectI {
        let s = self.scale.max(f64::EPSILON);
        RectI {
            x: (r.left as f64 / s).round() as i32,
            y: (r.top as f64 / s).round() as i32,
            w: ((r.right - r.left) as f64 / s).round() as i32,
            h: ((r.bottom - r.top) as f64 / s).round() as i32,
        }
    }

    fn target(&self, el: &IUIAutomationElement) -> AxTarget {
        // SAFETY: `el` is a live UIA element; every call is a COM property read.
        unsafe {
            let role = el.CurrentControlType().map(control_type_name).unwrap_or("Unknown").to_string();
            let name = el.CurrentName().ok().map(|b| b.to_string()).map(|s| cap(&s)).filter(|s| !s.is_empty());
            let value = el
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .and_then(|p| p.CurrentValue().ok())
                .map(|b| cap(&b.to_string()))
                .filter(|s| !s.is_empty());
            let bounds = el
                .CurrentBoundingRectangle()
                .ok()
                .filter(|r| r.right > r.left && r.bottom > r.top)
                .map(|r| self.to_points(r));
            AxTarget { role, name, value, bounds }
        }
    }
}

impl Default for UiaWindowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowTracker for UiaWindowTracker {
    fn front(&mut self) -> WindowInfo {
        // SAFETY: plain Win32 call.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return WindowInfo::default();
        }
        let title = window_title(hwnd);
        let exe = process_image(hwnd).unwrap_or_default();
        let app = Path::new(&exe).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bounds = window_bounds(hwnd).map(|r| self.to_points(r)).unwrap_or_default();
        WindowInfo { app, bundle_id: exe, title, url: None, is_local: None, bounds }
    }

    fn hit_test(&mut self, x: f64, y: f64) -> Option<AxTarget> {
        let deadline = Instant::now() + HIT_TEST_BUDGET;
        let pt = POINT { x: (x * self.scale).round() as i32, y: (y * self.scale).round() as i32 };
        let el = with_uia(|uia| {
            // SAFETY: COM calls on a live automation object.
            unsafe {
                let root = uia.ElementFromPoint(pt).context("ElementFromPoint")?;
                let walker = uia.ControlViewWalker().context("ControlViewWalker")?;
                Ok(descend(&walker, root, pt, deadline))
            }
        })
        .map_err(|e| log::debug!("hit_test: {e:#}"))
        .ok()?;
        Some(self.target(&el))
    }

    fn focused(&mut self) -> Option<AxTarget> {
        // SAFETY: COM call on a live automation object.
        let el = with_uia(|uia| unsafe { uia.GetFocusedElement().context("GetFocusedElement") })
            .map_err(|e| log::debug!("focused: {e:#}"))
            .ok()?;
        Some(self.target(&el))
    }

    fn focused_is_secure(&mut self) -> bool {
        // SAFETY: COM calls on live objects.
        with_uia(|uia| unsafe { Ok(uia.GetFocusedElement()?.CurrentIsPassword()?.as_bool()) }).unwrap_or(false)
    }

    fn tab_url(&mut self, window: &WindowInfo) -> Option<String> {
        let app = window.app.to_ascii_lowercase();
        if !BROWSERS.iter().any(|b| app == *b) {
            return None;
        }
        // SAFETY: plain Win32 call.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return None;
        }
        let key = hwnd.0 as isize;
        let now = Instant::now();
        if let Some((t, url)) = self.url_cache.get(&key) {
            if now.duration_since(*t) < URL_CACHE_TTL {
                return url.clone();
            }
        }
        let url = with_uia(|uia| address_bar_value(uia, hwnd))
            .map_err(|e| log::debug!("tab_url: {e:#}"))
            .ok()
            .flatten()
            .filter(|u| !u.is_empty());
        self.url_cache.retain(|_, (t, _)| now.duration_since(*t) < URL_CACHE_TTL);
        self.url_cache.insert(key, (now, url.clone()));
        url
    }
}

/// Walk down from `root` through children containing `pt`, returning the deepest interactive
/// element seen (or the deepest element at all when none is interactive).
///
/// # Safety
/// `walker` and `root` must be live UIA objects.
unsafe fn descend(walker: &IUIAutomationTreeWalker, root: IUIAutomationElement, pt: POINT, deadline: Instant) -> IUIAutomationElement {
    let mut current = root;
    let mut best_interactive: Option<IUIAutomationElement> = None;
    for _ in 0..MAX_DEPTH {
        if Instant::now() >= deadline {
            break;
        }
        // SAFETY: caller guarantees liveness.
        if unsafe { current.CurrentControlType() }.map(is_interactive).unwrap_or(false) {
            best_interactive = Some(current.clone());
        }
        // SAFETY: as above.
        let Ok(mut child) = (unsafe { walker.GetFirstChildElement(&current) }) else { break };
        let mut next: Option<IUIAutomationElement> = None;
        for _ in 0..MAX_SIBLINGS {
            if Instant::now() >= deadline {
                break;
            }
            // SAFETY: as above.
            if let Ok(r) = unsafe { child.CurrentBoundingRectangle() } {
                if contains(&r, pt) {
                    next = Some(child.clone());
                    // Keep scanning: later siblings usually paint on top of earlier ones.
                }
            }
            // SAFETY: as above.
            match unsafe { walker.GetNextSiblingElement(&child) } {
                Ok(sibling) => child = sibling,
                Err(_) => break,
            }
        }
        match next {
            Some(n) => current = n,
            None => break,
        }
    }
    // SAFETY: as above.
    if unsafe { current.CurrentControlType() }.map(is_interactive).unwrap_or(false) {
        return current;
    }
    best_interactive.unwrap_or(current)
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

fn is_interactive(t: UIA_CONTROLTYPE_ID) -> bool {
    [
        UIA_ButtonControlTypeId,
        UIA_HyperlinkControlTypeId,
        UIA_EditControlTypeId,
        UIA_CheckBoxControlTypeId,
        UIA_RadioButtonControlTypeId,
        UIA_ComboBoxControlTypeId,
        UIA_MenuItemControlTypeId,
        UIA_TabItemControlTypeId,
        UIA_ListItemControlTypeId,
        UIA_TreeItemControlTypeId,
        UIA_DataItemControlTypeId,
        UIA_SliderControlTypeId,
        UIA_SpinnerControlTypeId,
    ]
    .contains(&t)
}

fn control_type_name(t: UIA_CONTROLTYPE_ID) -> &'static str {
    match t.0 {
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        50039 => "SemanticZoom",
        50040 => "AppBar",
        _ => "Unknown",
    }
}

fn cap(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= NAME_CAP {
        s.to_string()
    } else {
        s.chars().take(NAME_CAP).collect()
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 1024];
    // SAFETY: buffer is a live local; GetWindowTextW writes at most its length.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn process_image(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a live out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: limited-information query on a process id we just obtained.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buffer and length are live locals; the handle is valid until CloseHandle below.
    let r = unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    // SAFETY: closing the handle we opened.
    let _ = unsafe { CloseHandle(handle) };
    r.ok()?;
    Some(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

fn window_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a live out-pointer of the size we pass.
    let dwm = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut rect as *mut RECT as *mut _, std::mem::size_of::<RECT>() as u32)
    };
    if dwm.is_ok() && rect.right > rect.left {
        return Some(rect);
    }
    // SAFETY: as above.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

/// Read the ValuePattern of the browser's address bar: an Edit whose accessible name is one of
/// `ADDRESS_BAR_NAMES`, searched with a provider-side (ControlType AND Name) condition.
fn address_bar_value(uia: &IUIAutomation, hwnd: HWND) -> Result<Option<String>> {
    // SAFETY: COM calls on live objects; VARIANTs are built by hand and released below.
    unsafe {
        let root = uia.ElementFromHandle(hwnd).context("ElementFromHandle")?;
        let type_var = variant_i32(UIA_EditControlTypeId.0);
        let type_cond: IUIAutomationCondition =
            uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &type_var).context("CreatePropertyCondition(type)")?;
        for name in ADDRESS_BAR_NAMES {
            let mut name_var = variant_bstr(name);
            let cond = uia
                .CreatePropertyCondition(UIA_NamePropertyId, &name_var)
                .and_then(|name_cond| uia.CreateAndCondition(&type_cond, &name_cond));
            release_bstr_variant(&mut name_var);
            let Ok(cond) = cond else { continue };
            let Ok(el) = root.FindFirst(TreeScope_Descendants, &cond) else { continue };
            if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                if let Ok(v) = p.CurrentValue() {
                    let v = v.to_string();
                    if !v.trim().is_empty() {
                        return Ok(Some(v.trim().to_string()));
                    }
                }
            }
        }
        Ok(None)
    }
}

fn variant_i32(v: i32) -> VARIANT {
    // SAFETY: an all-zero VARIANT is VT_EMPTY, a valid state.
    let mut var: VARIANT = unsafe { std::mem::zeroed() };
    var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
        vt: VT_I4,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0 { lVal: v },
    });
    var
}

fn variant_bstr(s: &str) -> VARIANT {
    // SAFETY: as in `variant_i32`.
    let mut var: VARIANT = unsafe { std::mem::zeroed() };
    var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
        vt: VT_BSTR,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0 { bstrVal: ManuallyDrop::new(BSTR::from(s)) },
    });
    var
}

/// Free the BSTR owned by a variant built with `variant_bstr`.
///
/// # Safety
/// `var` must be a properly initialized VARIANT that is not used afterwards.
unsafe fn release_bstr_variant(var: &mut VARIANT) {
    // SAFETY: VariantClear frees the BSTR and resets the variant to VT_EMPTY.
    let _ = unsafe { VariantClear(var) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_names_at_sixty_chars() {
        let long = "x".repeat(100);
        assert_eq!(cap(&long).chars().count(), NAME_CAP);
        assert_eq!(cap("  hi  "), "hi");
    }

    #[test]
    fn interactive_types() {
        assert!(is_interactive(UIA_ButtonControlTypeId));
        assert!(!is_interactive(UIA_CONTROLTYPE_ID(50020)));
        assert_eq!(control_type_name(UIA_EditControlTypeId), "Edit");
    }
}
