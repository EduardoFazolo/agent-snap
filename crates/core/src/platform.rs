//! OS-neutral contract every backend implements. Coordinates:
//! - `points`: logical screen coordinates (what input events and AX report), origin top-left of main display.
//! - `pixels`: frame coordinates. `pixels = points * scale`.
//!
//! Time: `Instant` captured as close as possible to when the OS produced the event.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PointI {
    pub x: i32,
    pub y: i32,
}

/// Static facts about the display being captured.
#[derive(Clone, Debug)]
pub struct CaptureInfo {
    pub width: u32,  // pixels
    pub height: u32, // pixels
    pub scale: f64,  // pixels per point
}

/// One captured frame. Pixel format is always NV12 (Y plane, then interleaved UV plane, both with given strides).
/// Backends must deliver a frame only when the OS produced a new one; the recorder handles constant-fps duplication.
#[derive(Clone)]
pub struct Frame {
    pub t: Instant,
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub y: Arc<[u8]>,
    pub uv: Arc<[u8]>,
    /// Regions changed since previous frame, in pixels, clamped to the frame. Empty = unknown (treat as whole frame).
    pub dirty: Vec<RectI>,
}

pub trait ScreenCapture: Send {
    /// Start delivering frames of the main display at up to `max_fps`, cursor drawn into frames.
    fn start(&mut self, max_fps: u32, on_frame: Box<dyn FnMut(Frame) + Send>) -> Result<CaptureInfo>;
    fn stop(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RawKind {
    Move,
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    /// `dy` in points, positive = content moves up (user scrolls down).
    Scroll { dy: f64 },
    /// `label` is a human name ("Enter", "Cmd+S", "a"); `printable` is the typed char when the key inserts text.
    KeyDown { label: String, printable: Option<char> },
}

#[derive(Clone, Debug)]
pub struct RawInput {
    pub t: Instant,
    pub kind: RawKind,
    /// Cursor position in points at event time.
    pub x: f64,
    pub y: f64,
    /// Click count reported by the OS for Down events (1 = single, 2 = double), else 1.
    pub click_count: u32,
}

pub trait InputTap: Send {
    /// Listen-only global tap. Must never swallow events.
    fn start(&mut self, on_input: Box<dyn FnMut(RawInput) + Send>) -> Result<()>;
    fn stop(&mut self);
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub app: String,
    #[serde(rename = "bundleId", default)]
    pub bundle_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Set by the recorder from `url`: true when the host is localhost/.local. Kept for `session.json` compatibility.
    #[serde(rename = "isLocal", default, skip_serializing_if = "Option::is_none")]
    pub is_local: Option<bool>,
    /// Window frame in pixels.
    #[serde(default)]
    pub bounds: RectI,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AxTarget {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Element frame in pixels, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<RectI>,
}

/// Best-effort semantic lookups. Every method must be fast (<50ms) and never panic; return defaults on failure.
/// Implementations receive and return POINTS; the recorder converts to pixels.
pub trait WindowTracker: Send {
    /// Frontmost window: app name, bundle/process id, title, bounds in points.
    fn front(&mut self) -> WindowInfo;
    /// Deepest interactive element under the point, bounds in points. Names capped at 60 chars by the implementation.
    fn hit_test(&mut self, x: f64, y: f64) -> Option<AxTarget>;
    /// Currently focused element, bounds in points.
    fn focused(&mut self) -> Option<AxTarget>;
    /// True when the focused element is a password/secure field (typed text must be redacted).
    fn focused_is_secure(&mut self) -> bool;
    /// URL of the active browser tab for this window, if it is a known browser. Cached by the implementation.
    fn tab_url(&mut self, window: &WindowInfo) -> Option<String>;
    /// True when the UI element at this screen point (in points) belongs to the recorder's own
    /// process: our tray icon, its menu, our popover. Lets the recorder drop interactions with
    /// its own UI without relying on a static rect. Default false for backends that can't tell.
    fn owns_point(&mut self, _x: f64, _y: f64) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct OcrText {
    pub text: String,
    /// In pixels of the image passed to `recognize`.
    pub bounds: RectI,
}

pub trait Ocr: Send + Sync {
    fn recognize(&self, rgba: &image::RgbaImage) -> Vec<OcrText>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermState {
    Granted,
    Denied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    ScreenRecording,
    Accessibility,
}

pub trait Permissions: Send {
    fn state(&self, p: Permission) -> PermState;
    /// Trigger the OS prompt when possible.
    fn request(&self, p: Permission);
    /// Open the OS settings pane for the permission.
    fn open_settings(&self, p: Permission);
}

/// Everything a backend provides, bundled.
pub struct Backend {
    pub capture: Box<dyn ScreenCapture>,
    pub input: Box<dyn InputTap>,
    pub windows: Box<dyn WindowTracker>,
    pub ocr: Arc<dyn Ocr>,
    pub permissions: Box<dyn Permissions>,
}
