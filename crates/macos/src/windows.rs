//! Frontmost window + best-effort AX lookups. Every call is bounded (AX messaging timeout and a
//! wall-clock budget) and never panics; failures degrade to None / defaults.

use std::ffi::c_void;
use std::io::Read;
use std::process::{Command, Stdio};
use std::ptr::{null, NonNull};
use std::time::{Duration, Instant};

use agent_snap_core::platform::{AxTarget, RectI, WindowInfo, WindowTracker};
use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize};

use crate::util::{cf_string, cf_to_string, truncate_chars};

const AX_TIMEOUT_S: f32 = 0.1;
const NAME_MAX: usize = 60;
const HIT_BUDGET: Duration = Duration::from_millis(40);

const INTERACTIVE_ROLES: &[&str] = &[
    "AXButton",
    "AXPopUpButton",
    "AXMenuButton",
    "AXMenuItem",
    "AXMenuBarItem",
    "AXLink",
    "AXTextField",
    "AXTextArea",
    "AXSecureTextField",
    "AXComboBox",
    "AXCheckBox",
    "AXRadioButton",
    "AXSlider",
    "AXDisclosureTriangle",
    "AXCell",
    "AXRow",
    "AXIncrementor",
    "AXColorWell",
    "AXTabGroup",
    "AXToolbar",
    "AXSearchField",
];

const BROWSER_SCRIPTS: &[(&str, &str)] = &[
    ("com.google.Chrome", "tell application id \"com.google.Chrome\" to get URL of active tab of front window"),
    ("com.google.Chrome.canary", "tell application id \"com.google.Chrome.canary\" to get URL of active tab of front window"),
    ("com.brave.Browser", "tell application id \"com.brave.Browser\" to get URL of active tab of front window"),
    ("com.microsoft.edgemac", "tell application id \"com.microsoft.edgemac\" to get URL of active tab of front window"),
    ("company.thebrowser.Browser", "tell application id \"company.thebrowser.Browser\" to get URL of active tab of front window"),
    ("com.vivaldi.Vivaldi", "tell application id \"com.vivaldi.Vivaldi\" to get URL of active tab of front window"),
    ("com.apple.Safari", "tell application id \"com.apple.Safari\" to get URL of front document"),
];

fn attr(e: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let mut v: *const CFType = null();
    let err = unsafe { e.copy_attribute_value(&cf_string(name), NonNull::from(&mut v)) };
    if err != AXError::Success || v.is_null() {
        return None;
    }
    // SAFETY: Copy* returns +1.
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(v as *mut CFType)) })
}

fn attr_string(e: &AXUIElement, name: &str) -> Option<String> {
    let v = attr(e, name)?;
    let s = v.downcast_ref::<CFString>()?.to_string();
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn attr_element(e: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    let v = attr(e, name)?;
    let el = v.downcast_ref::<AXUIElement>()?;
    Some(unsafe { CFRetained::retain(NonNull::from(el)) })
}

fn attr_elements(e: &AXUIElement, name: &str, max: usize) -> Vec<CFRetained<AXUIElement>> {
    let Some(v) = attr(e, name) else { return Vec::new() };
    let Some(arr) = v.downcast_ref::<CFArray>() else { return Vec::new() };
    let n = (arr.count().max(0) as usize).min(max);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let p = unsafe { arr.value_at_index(i as isize) } as *const CFType;
        if p.is_null() {
            continue;
        }
        let ty = unsafe { &*p };
        if let Some(el) = ty.downcast_ref::<AXUIElement>() {
            out.push(unsafe { CFRetained::retain(NonNull::from(el)) });
        }
    }
    out
}

fn ax_point(v: &CFType) -> Option<CGPoint> {
    let av = v.downcast_ref::<AXValue>()?;
    let mut p = CGPoint::new(0.0, 0.0);
    let ok = unsafe { av.value(AXValueType::CGPoint, NonNull::new_unchecked(&mut p as *mut CGPoint as *mut c_void)) };
    ok.then_some(p)
}

fn ax_size(v: &CFType) -> Option<CGSize> {
    let av = v.downcast_ref::<AXValue>()?;
    let mut s = CGSize::new(0.0, 0.0);
    let ok = unsafe { av.value(AXValueType::CGSize, NonNull::new_unchecked(&mut s as *mut CGSize as *mut c_void)) };
    ok.then_some(s)
}

fn frame_of(e: &AXUIElement) -> Option<CGRect> {
    let pv = attr(e, "AXPosition")?;
    let sv = attr(e, "AXSize")?;
    let p = ax_point(&pv)?;
    let s = ax_size(&sv)?;
    if !(p.x.is_finite() && p.y.is_finite() && s.width.is_finite() && s.height.is_finite()) {
        return None;
    }
    Some(CGRect::new(p, s))
}

fn rect_i(r: CGRect) -> RectI {
    RectI {
        x: r.origin.x.round() as i32,
        y: r.origin.y.round() as i32,
        w: r.size.width.round() as i32,
        h: r.size.height.round() as i32,
    }
}

fn contains(r: &CGRect, x: f64, y: f64) -> bool {
    x >= r.origin.x && y >= r.origin.y && x < r.origin.x + r.size.width && y < r.origin.y + r.size.height
}

/// First static text inside an element (button labels, menu items).
fn inner_text(e: &AXUIElement, depth: usize, budget: &mut i32) -> Option<String> {
    if depth >= 4 || *budget <= 0 {
        return None;
    }
    for k in attr_elements(e, "AXChildren", 24) {
        *budget -= 1;
        if *budget <= 0 {
            return None;
        }
        let role = attr_string(&k, "AXRole");
        if role.as_deref() == Some("AXStaticText") {
            if let Some(v) = attr_string(&k, "AXValue") {
                return Some(truncate_chars(&v, 80));
            }
        }
        if let Some(t) = attr_string(&k, "AXTitle") {
            return Some(t);
        }
        if let Some(t) = inner_text(&k, depth + 1, budget) {
            return Some(t);
        }
    }
    None
}

fn describe(e: &AXUIElement) -> AxTarget {
    let role_desc = attr_string(e, "AXRoleDescription");
    let role = attr_string(e, "AXRole");
    let mut name = attr_string(e, "AXTitle")
        .or_else(|| attr_string(e, "AXDescription"))
        .or_else(|| attr_string(e, "AXPlaceholderValue"))
        .or_else(|| attr_element(e, "AXTitleUIElement").and_then(|l| attr_string(&l, "AXValue")))
        .or_else(|| {
            let mut budget = 24;
            inner_text(e, 0, &mut budget)
        })
        .or_else(|| attr_string(e, "AXHelp"));

    let mut value = attr(e, "AXValue").and_then(|v| cf_to_string(&v));
    if let Some(v) = value.as_mut() {
        if let Some(line) = v.lines().next() {
            *v = line.to_string();
        }
        *v = truncate_chars(v, NAME_MAX);
        if v.trim().is_empty() {
            value = None;
        }
    }
    if let Some(n) = name.as_mut() {
        *n = truncate_chars(n, NAME_MAX);
    }
    let bounds = frame_of(e).map(rect_i);
    let role = role_desc.or(role).map(|r| r.replace("AX", "")).unwrap_or_else(|| "unknown".into());
    AxTarget { role, name, value, bounds }
}

fn element_at(root: &AXUIElement, x: f64, y: f64) -> Option<CFRetained<AXUIElement>> {
    let mut el: *const AXUIElement = null();
    let err = unsafe { root.copy_element_at_position(x as f32, y as f32, NonNull::from(&mut el)) };
    if err != AXError::Success || el.is_null() {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(el as *mut AXUIElement)) })
}

/// AX-based window tracker for macOS.
pub struct MacWindowTracker {
    url_cache: Option<(String, Option<String>, Instant)>,
}

impl Default for MacWindowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl MacWindowTracker {
    pub fn new() -> Self {
        // Setting the timeout on the system-wide element makes it the default for all elements.
        let sys = unsafe { AXUIElement::new_system_wide() };
        unsafe { sys.set_messaging_timeout(AX_TIMEOUT_S) };
        Self { url_cache: None }
    }

    fn system_wide() -> CFRetained<AXUIElement> {
        let sys = unsafe { AXUIElement::new_system_wide() };
        unsafe { sys.set_messaging_timeout(AX_TIMEOUT_S) };
        sys
    }

    fn frontmost_pid() -> Option<(String, String, i32)> {
        let ws = NSWorkspace::sharedWorkspace();
        let app = ws.frontmostApplication()?;
        let name = app.localizedName().map(|s| s.to_string()).unwrap_or_else(|| "?".into());
        let bid = app.bundleIdentifier().map(|s| s.to_string()).unwrap_or_default();
        Some((name, bid, app.processIdentifier()))
    }

    fn focused_element() -> Option<CFRetained<AXUIElement>> {
        attr_element(&Self::system_wide(), "AXFocusedUIElement")
    }

    fn run_osascript(script: &str) -> Option<String> {
        let mut child = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    let mut out = String::new();
                    child.stdout.take()?.read_to_string(&mut out).ok()?;
                    let s = out.trim();
                    return (!s.is_empty()).then(|| s.to_string());
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }
}

impl WindowTracker for MacWindowTracker {
    fn front(&mut self) -> WindowInfo {
        let Some((app, bundle_id, pid)) = Self::frontmost_pid() else {
            return WindowInfo { app: "?".into(), ..Default::default() };
        };
        let mut info = WindowInfo { app, bundle_id, ..Default::default() };
        let ax_app = unsafe { AXUIElement::new_application(pid) };
        unsafe { ax_app.set_messaging_timeout(AX_TIMEOUT_S) };
        if let Some(win) = attr_element(&ax_app, "AXFocusedWindow") {
            info.title = attr_string(&win, "AXTitle").unwrap_or_default();
            if let Some(f) = frame_of(&win) {
                info.bounds = rect_i(f);
            }
        }
        info
    }

    fn hit_test(&mut self, x: f64, y: f64) -> Option<AxTarget> {
        let start = Instant::now();
        let root = element_at(&Self::system_wide(), x, y).or_else(|| {
            let (_, _, pid) = Self::frontmost_pid()?;
            let app = unsafe { AXUIElement::new_application(pid) };
            unsafe { app.set_messaging_timeout(AX_TIMEOUT_S) };
            element_at(&app, x, y)
        })?;

        let mut chain: Vec<CFRetained<AXUIElement>> = vec![root.clone()];
        let mut cur = root;
        for _ in 0..40 {
            if start.elapsed() > HIT_BUDGET {
                break;
            }
            let kids = attr_elements(&cur, "AXChildren", 500);
            if kids.is_empty() {
                break;
            }
            let mut best: Option<(CFRetained<AXUIElement>, f64)> = None;
            for k in kids {
                let Some(f) = frame_of(&k) else { continue };
                if !contains(&f, x, y) {
                    continue;
                }
                let area = f.size.width * f.size.height;
                if best.as_ref().is_none_or(|(_, a)| area < *a) {
                    best = Some((k, area));
                }
            }
            let Some((b, _)) = best else { break };
            chain.push(b.clone());
            cur = b;
        }

        for e in chain.iter().rev().take(6) {
            if let Some(r) = attr_string(e, "AXRole") {
                if INTERACTIVE_ROLES.contains(&r.as_str()) {
                    return Some(describe(e));
                }
            }
        }
        for e in chain.iter().rev() {
            let d = describe(e);
            if d.name.is_some() || d.value.is_some() {
                return Some(d);
            }
        }
        chain.last().map(|e| describe(e))
    }

    fn focused(&mut self) -> Option<AxTarget> {
        Self::focused_element().map(|e| describe(&e))
    }

    fn focused_is_secure(&mut self) -> bool {
        let Some(e) = Self::focused_element() else { return false };
        attr_string(&e, "AXSubrole").as_deref() == Some("AXSecureTextField")
            || attr_string(&e, "AXRole").as_deref() == Some("AXSecureTextField")
    }

    fn tab_url(&mut self, window: &WindowInfo) -> Option<String> {
        let script = BROWSER_SCRIPTS.iter().find(|(b, _)| *b == window.bundle_id).map(|(_, s)| *s)?;
        let key = format!("{}|{}", window.bundle_id, window.title);
        if let Some((k, url, t)) = &self.url_cache {
            if *k == key && t.elapsed() < Duration::from_secs(2) {
                return url.clone();
            }
        }
        let url = Self::run_osascript(script);
        self.url_cache = Some((key, url.clone(), Instant::now()));
        url
    }
}
