//! Session data model. Serialized as `session.json`, key-compatible with the Swift recorder
//! (camelCase keys, enum values as strings, optional fields omitted).

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

pub use crate::platform::{AxTarget, PointI, RectI, WindowInfo};

impl RectI {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    /// Rounds a float rect to integer pixels.
    pub fn from_f64(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x: x.round() as i32, y: y.round() as i32, w: w.round() as i32, h: h.round() as i32 }
    }
    pub fn max_x(&self) -> i32 {
        self.x + self.w
    }
    pub fn max_y(&self) -> i32 {
        self.y + self.h
    }
    pub fn area(&self) -> i64 {
        self.w as i64 * self.h as i64
    }
    /// Smallest rect containing both (CGRect.union semantics).
    pub fn union(&self, o: &RectI) -> RectI {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.max_x().max(o.max_x());
        let y1 = self.max_y().max(o.max_y());
        RectI { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }
    pub fn center(&self) -> PointI {
        PointI { x: self.x + self.w / 2, y: self.y + self.h / 2 }
    }
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && x < self.max_x() as f64 && y >= self.y as f64 && y < self.max_y() as f64
    }
}

impl PointI {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
    pub fn from_f64(x: f64, y: f64) -> Self {
        Self { x: x.round() as i32, y: y.round() as i32 }
    }
}

impl WindowInfo {
    /// Identity used to decide "same window" for grouping steps.
    pub fn key(&self) -> String {
        let id = if self.bundle_id.is_empty() { &self.app } else { &self.bundle_id };
        format!("{}|{}", id, self.url.as_deref().unwrap_or(&self.title))
    }

    pub fn describe(&self) -> String {
        let mut s = self.app.clone();
        if !self.title.is_empty() {
            s.push_str(" · ");
            s.push_str(&self.title);
        }
        if let Some(u) = &self.url {
            s.push_str(" · ");
            s.push_str(u);
        }
        if self.is_local == Some(true) {
            s.push_str(" (local)");
        }
        s
    }
}

impl AxTarget {
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.role.is_empty() {
            parts.push(self.role.clone());
        }
        if let Some(n) = self.name.as_deref().filter(|n| !n.is_empty()) {
            parts.push(format!("\"{n}\""));
        } else if let Some(v) = self.value.as_deref().filter(|v| !v.is_empty()) {
            parts.push(format!("\"{v}\""));
        }
        parts.join(" ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    Click,
    DoubleClick,
    RightClick,
    Drag,
    Type,
    Key,
    Scroll,
    Cursor,
    WindowSwitch,
    ScreenUpdate,
}

impl StepKind {
    /// The Swift `rawValue`, used in file names.
    /// Steps caused by the user's hands, as opposed to synthesized or observed ones.
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            StepKind::Click | StepKind::DoubleClick | StepKind::RightClick | StepKind::Drag | StepKind::Type | StepKind::Key | StepKind::Scroll
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            StepKind::Click => "click",
            StepKind::DoubleClick => "doubleClick",
            StepKind::RightClick => "rightClick",
            StepKind::Drag => "drag",
            StepKind::Type => "type",
            StepKind::Key => "key",
            StepKind::Scroll => "scroll",
            StepKind::Cursor => "cursor",
            StepKind::WindowSwitch => "windowSwitch",
            StepKind::ScreenUpdate => "screenUpdate",
        }
    }
}

/// One captured frame: when it arrived and what changed, in pixels. Written for every frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameLog {
    pub t: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<RectI>,
}

/// Sampled cursor position (pixels) so the builder can ignore the cursor when diffing frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CursorSample {
    pub t: f64,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub index: usize,
    /// Seconds since session start.
    pub t: f64,
    /// When the input ended (mouse up, last key). Same as t for instant events.
    #[serde(rename = "endT", default, skip_serializing_if = "Option::is_none")]
    pub end_t: Option<f64>,
    pub kind: StepKind,
    pub label: String,
    /// Anchor point in captured-image pixels (click point, or focused element center).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<PointI>,
    #[serde(rename = "endPoint", default, skip_serializing_if = "Option::is_none")]
    pub end_point: Option<PointI>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AxTarget>,
    pub window: WindowInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(rename = "beforeT", default, skip_serializing_if = "Option::is_none")]
    pub before_t: Option<f64>,
    #[serde(rename = "afterT", default, skip_serializing_if = "Option::is_none")]
    pub after_t: Option<f64>,
    /// Union of OS-reported dirty rects between before and after (pixels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<RectI>,
}

impl Step {
    pub fn new(t: f64, kind: StepKind, label: impl Into<String>, window: WindowInfo) -> Self {
        Step {
            index: 0,
            t,
            end_t: None,
            kind,
            label: label.into(),
            point: None,
            end_point: None,
            text: None,
            target: None,
            window,
            before: None,
            after: None,
            before_t: None,
            after_t: None,
            dirty: None,
        }
    }
    pub fn end(&self) -> f64 {
        self.end_t.unwrap_or(self.t)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSegment {
    pub start: f64,
    pub end: f64,
    pub window: WindowInfo,
    #[serde(rename = "inputEvents")]
    pub input_events: usize,
}

impl WindowSegment {
    pub fn dwell(&self) -> f64 {
        self.end - self.start
    }
}

mod iso_date {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&d.to_rfc3339_opts(SecondsFormat::Secs, true))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "startedAt", with = "iso_date")]
    pub started_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// Relative path of the recording. Source of truth for every frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// Host time of the first video frame minus session t0: videoTime = t - videoOffset.
    #[serde(rename = "videoOffset", default)]
    pub video_offset: f64,
    #[serde(default)]
    pub frames: Vec<FrameLog>,
    #[serde(default)]
    pub cursor: Vec<CursorSample>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub timeline: Vec<WindowSegment>,
}

impl Session {
    pub fn new(width: u32, height: u32, scale: f64) -> Self {
        let now = Utc::now();
        let started_at = DateTime::from_timestamp(now.timestamp(), 0).unwrap_or(now);
        Session {
            started_at,
            width,
            height,
            scale,
            video: None,
            video_offset: 0.0,
            frames: Vec::new(),
            cursor: Vec::new(),
            steps: Vec::new(),
            timeline: Vec::new(),
        }
    }

    pub fn load(dir: &Path) -> Result<Session> {
        let path = dir.join("session.json");
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join("session.json");
        let data = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))
    }
}

/// `mm:ss.s`, e.g. `01:03.4`.
pub fn fmt_t(t: f64) -> String {
    let m = (t as i64) / 60;
    let s = t - (m * 60) as f64;
    format!("{m:02}:{s:04.1}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_t_matches_swift() {
        assert_eq!(fmt_t(0.0), "00:00.0");
        assert_eq!(fmt_t(63.44), "01:03.4");
        assert_eq!(fmt_t(93.87), "01:33.9");
    }

    #[test]
    fn describe_and_key() {
        let w = WindowInfo {
            app: "Google Chrome".into(),
            bundle_id: "com.google.Chrome".into(),
            title: "Dash".into(),
            url: Some("http://localhost:3000/x".into()),
            is_local: Some(true),
            bounds: RectI::default(),
        };
        assert_eq!(w.describe(), "Google Chrome · Dash · http://localhost:3000/x (local)");
        assert_eq!(w.key(), "com.google.Chrome|http://localhost:3000/x");
        let t = AxTarget { role: "button".into(), name: Some("Reload".into()), value: Some("1".into()), bounds: None };
        assert_eq!(t.describe(), "button \"Reload\"");
        let t = AxTarget { role: "group".into(), name: None, value: Some("v".into()), bounds: None };
        assert_eq!(t.describe(), "group \"v\"");
    }

    #[test]
    fn step_kind_strings() {
        assert_eq!(serde_json::to_string(&StepKind::DoubleClick).unwrap(), "\"doubleClick\"");
        assert_eq!(serde_json::to_string(&StepKind::WindowSwitch).unwrap(), "\"windowSwitch\"");
        assert_eq!(StepKind::ScreenUpdate.as_str(), "screenUpdate");
    }

    /// Numbers compared as f64 so `2` and `2.0` are equal.
    fn normalize(v: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::Number(n) => Value::from(n.as_f64().unwrap()),
            Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
            Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, normalize(v))).collect()),
            other => other,
        }
    }

    #[test]
    fn fixtures_roundtrip() {
        let Some(home) = dirs::home_dir() else { return };
        let root = home.join("agent-snap/sessions");
        let Ok(rd) = std::fs::read_dir(&root) else {
            eprintln!("no fixtures at {}", root.display());
            return;
        };
        let mut n = 0;
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.join("session.json").exists() {
                continue;
            }
            let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("session.json")).unwrap()).unwrap();
            let s = Session::load(&dir).unwrap_or_else(|e| panic!("{}: {e:#}", dir.display()));
            let ours = normalize(serde_json::to_value(&s).unwrap());
            let raw = normalize(raw);
            for (k, v) in raw.as_object().unwrap() {
                assert_eq!(&ours[k], v, "{}: key {k} differs", dir.display());
            }
            let again: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(again, s);
            n += 1;
        }
        eprintln!("roundtripped {n} fixtures");
    }
}
