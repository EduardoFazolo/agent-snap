//! Turns a recording (video + event log) into flow.md with focused composites.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{Context, Result};
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{imageops, ImageEncoder, Rgba, RgbaImage};
use lru::LruCache;

use crate::frames::FrameSource;
use crate::model::{fmt_t, AxTarget, PointI, RectI, Session, Step, StepKind, WindowInfo};
use crate::options::Options;
use crate::platform::Ocr;

pub const PAD: f64 = 60.0;
pub const MIN_SIZE: (f64, f64) = (700.0, 450.0);
pub const MAX_SIZE: (f64, f64) = (1800.0, 1200.0);
pub const DIFF_RADIUS: f64 = 650.0;
pub const CELL: usize = 16;
pub const MARGIN: f64 = 28.0;
pub const GAP: f64 = 70.0;
pub const LABEL_H: f64 = 46.0;
pub const MIN_PANEL_SCALE: f64 = 0.42;
pub const BRIEF_DWELL: f64 = 1.5;
pub const SETTLE_MAX: f64 = 2.0;
pub const SCREEN_UPDATE_FRACTION: f64 = 0.15;
pub const CURSOR_RADIUS: f64 = 70.0;

static FONT_DATA: &[u8] = include_bytes!("../assets/Inter-SemiBold.ttf");

/// Rough Claude cost of one image: downscaled to ≤1568px long edge and ≤1.15MP, then w*h/750.
pub fn image_tokens(width: u32, height: u32) -> usize {
    let (w, h) = (width as f64, height as f64);
    let s = 1.0f64.min(1568.0 / w.max(h)).min((1_150_000.0 / (w * h)).sqrt());
    ((w * s) * (h * s) / 750.0) as usize
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub steps: usize,
    pub images: usize,
    pub image_tokens: usize,
    pub text_tokens: usize,
}

impl Stats {
    pub fn tokens(&self) -> usize {
        self.image_tokens + self.text_tokens
    }
}

/// Float rect with CGRect semantics (origin top-left, `max = origin + size`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Rect { x, y, w, h }
    }
    fn from_i(r: &RectI) -> Self {
        Rect::new(r.x as f64, r.y as f64, r.w as f64, r.h as f64)
    }
    fn around(x: f64, y: f64, half: f64) -> Self {
        Rect::new(x - half, y - half, half * 2.0, half * 2.0)
    }
    fn max_x(&self) -> f64 {
        self.x + self.w
    }
    fn max_y(&self) -> f64 {
        self.y + self.h
    }
    fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
    fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        Rect::new(x0, y0, self.max_x().max(o.max_x()) - x0, self.max_y().max(o.max_y()) - y0)
    }
    fn intersection(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.max_x().min(o.max_x());
        let y1 = self.max_y().min(o.max_y());
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        }
    }
    fn intersects(&self, o: &Rect) -> bool {
        self.intersection(o).is_some()
    }
    fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.max_x() && y >= self.y && y < self.max_y()
    }
    fn contains_rect(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.max_x() <= self.max_x() && o.max_y() <= self.max_y()
    }
    fn inset(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w - 2.0 * dx, self.h - 2.0 * dy)
    }
    fn integral(&self) -> Rect {
        let x0 = self.x.floor();
        let y0 = self.y.floor();
        Rect::new(x0, y0, self.max_x().ceil() - x0, self.max_y().ceil() - y0)
    }
    fn to_i(self) -> RectI {
        RectI::from_f64(self.x, self.y, self.w, self.h)
    }
}

fn union_opt(a: Option<Rect>, b: Rect) -> Option<Rect> {
    Some(a.map_or(b, |a| a.union(&b)))
}

#[derive(Clone)]
struct Panel {
    image: Arc<RgbaImage>,
    rect: Rect,
    label: Option<String>,
    marker: Option<(f64, f64)>,
    marker2: Option<(f64, f64)>,
    scale: f64,
}

struct Run {
    window: WindowInfo,
    steps: Vec<Step>,
    start: f64,
    end: f64,
    inputs: usize,
}

pub struct Builder {
    dir: PathBuf,
    session: Session,
    options: Options,
    ocr: Option<Arc<dyn Ocr>>,
    frames: Option<FrameSource>,
    images: LruCache<String, Arc<RgbaImage>>,
    gray_cache: HashMap<String, Arc<Vec<u8>>>,
    stats: Stats,
    font: FontRef<'static>,
}

impl Builder {
    pub fn new(session_dir: impl Into<PathBuf>, options: Options, ocr: Option<Arc<dyn Ocr>>) -> Result<Self> {
        let dir: PathBuf = session_dir.into();
        let dir = dir.canonicalize().unwrap_or(dir);
        let session = Session::load(&dir)?;
        let mut frames = None;
        if let Some(v) = &session.video {
            match FrameSource::open(&dir.join(v), session.video_offset) {
                Ok(src) if src.duration > 0.0 => frames = Some(src),
                Ok(_) => log::warn!("video {v} has no readable duration; building without frames"),
                Err(e) => log::warn!("cannot open video {v}: {e:#}; building without frames"),
            }
        }
        let font = FontRef::try_from_slice(FONT_DATA).context("embedded font")?;
        Ok(Builder {
            dir,
            session,
            options,
            ocr,
            frames,
            images: LruCache::new(NonZeroUsize::new(16).unwrap()),
            gray_cache: HashMap::new(),
            stats: Stats::default(),
            font,
        })
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn composite_width(&self) -> f64 {
        self.options.composite_width as f64
    }
    fn panels_per_image(&self) -> usize {
        self.options.panels_per_image.clamp(1, 6)
    }
    fn quiet(&self) -> f64 {
        self.options.quiet_ms as f64 / 1000.0
    }

    pub fn build(&mut self) -> Result<PathBuf> {
        std::fs::create_dir_all(self.dir.join("frames"))?;
        let comps = self.dir.join("composites");
        std::fs::create_dir_all(&comps)?;
        if let Ok(rd) = std::fs::read_dir(&comps) {
            for f in rd.flatten() {
                let _ = std::fs::remove_file(f.path());
            }
        }

        if self.frames.is_some() {
            if self.options.detect_screen_updates {
                self.synthesize_screen_updates();
            }
            self.sort_steps();
            for i in 0..self.session.steps.len() {
                let mut s = self.session.steps[i].clone();
                // An "after" frame must never reach into the next input: whatever that input
                // changed is its own step, not this one's.
                let next_input = self.session.steps[i + 1..]
                    .iter()
                    .find(|n| n.kind.is_input())
                    .map(|n| n.t);
                self.extract_frames(&mut s, next_input);
                self.label_by_ocr(&mut s);
                self.session.steps[i] = s;
            }
        } else {
            self.sort_steps();
        }

        let runs = self.group_runs();
        let mut md = String::from("# agent-snap session\n\n");
        let dur = self.session.timeline.last().map(|s| s.end).or_else(|| self.session.steps.last().map(|s| s.t)).unwrap_or(0.0);
        md += &format!("- recorded: {}, duration {}\n", self.session.started_at.format("%Y-%m-%dT%H:%M:%SZ"), fmt_t(dur));
        md += &format!(
            "- screen: {}×{} px @{:?}x (all coordinates below are pixels in that space)\n",
            self.session.width, self.session.height, self.session.scale
        );
        if let Some(v) = &self.session.video {
            md += &format!("- video: {} (full recording; timestamps below index into it)\n", self.dir.join(v).display());
        }
        md += &format!("- {} steps across {} window visits\n{{{{COST}}}}\n", self.session.steps.len(), runs.len());
        md += "The images shown inline are the flow. The full frames and video listed under each section are only for zooming in if a step is unclear.\n\n";

        let mut run_no = 0;
        for mut run in runs {
            let has_real = run.steps.iter().any(|s| s.kind != StepKind::WindowSwitch);
            let dwell = run.end - run.start;
            if !has_real && (dwell < BRIEF_DWELL || run.inputs == 0) {
                md += &format!("_{} briefly on {} ({:.1}s, no input)_\n\n", fmt_t(run.start), run.window.describe(), dwell);
                continue;
            }
            run_no += 1;
            md += &format!("## {}. {}\n", run_no, run.window.describe());
            md += &format!("_{} → {}_\n\n", fmt_t(run.start), fmt_t(run.end));

            run.steps = merge_scrolls(&run.steps);
            let (panels, no_change) = self.make_panels(&run);
            for (img_idx, chunk) in self.pack(panels).into_iter().enumerate() {
                let name = format!("composites/run-{:02}-{}.png", run_no, (b'a' + (img_idx % 26) as u8) as char);
                if let Some(img) = self.compose(&chunk) {
                    let path = self.dir.join(&name);
                    save_png(&img, &path, false);
                    md += &format!("![{} steps]({})\n\n", run.window.app, path.display());
                    self.stats.images += 1;
                    self.stats.image_tokens += image_tokens(img.width(), img.height());
                }
            }
            for s in run.steps.iter().filter(|s| s.kind != StepKind::WindowSwitch) {
                let mut line = format!("{}. `{}` {}", s.index, fmt_t(s.t), s.label);
                if let Some(p) = s.point {
                    if !matches!(s.kind, StepKind::Scroll | StepKind::ScreenUpdate | StepKind::Cursor) {
                        line += &format!(" @({},{})", p.x, p.y);
                    }
                }
                if no_change.contains(&s.index) {
                    line += " _(no visible change)_";
                }
                md += &line;
                md += "\n";
            }
            let mut seen = HashSet::new();
            let files: Vec<String> = run
                .steps
                .iter()
                .flat_map(|s| [s.before.clone(), s.after.clone()])
                .flatten()
                .filter(|f| seen.insert(f.clone()))
                .collect();
            if !files.is_empty() {
                md += "\n<details><summary>full frames</summary>\n\n";
                md += &files.iter().map(|f| format!("- {}", self.dir.join(f).display())).collect::<Vec<_>>().join("\n");
                md += "\n\n</details>\n";
            }
            md += "\n";
        }
        self.stats.steps = self.session.steps.len();
        self.stats.text_tokens = md.chars().count() / 4 + 12;
        let cost = format!(
            "- estimated prompt cost: ~{} tokens ({} images ~{}, text ~{})\n",
            self.stats.tokens(),
            self.stats.images,
            self.stats.image_tokens,
            self.stats.text_tokens
        );
        let md = md.replace("{{COST}}", &cost);
        let out = self.dir.join("flow.md");
        std::fs::write(&out, md).with_context(|| format!("writing {}", out.display()))?;
        Ok(out)
    }

    fn sort_steps(&mut self) {
        self.session.steps.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        for (i, s) in self.session.steps.iter_mut().enumerate() {
            s.index = i + 1;
        }
    }

    // MARK: video → frames

    /// Cursor position at session time t (nearest earlier sample).
    fn cursor_at(&self, t: f64) -> Option<(f64, f64)> {
        let c = &self.session.cursor;
        if c.is_empty() {
            return None;
        }
        let (mut lo, mut hi) = (0usize, c.len() - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if c[mid].t <= t {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Some((c[lo].x as f64, c[lo].y as f64))
    }

    /// A dirty rect that is just the cursor moving (small, on the cursor).
    fn is_cursor_only(&self, d: &RectI, t: f64) -> bool {
        if d.w > 110 || d.h > 110 {
            return false;
        }
        let Some((cx, cy)) = self.cursor_at(t) else { return false };
        let r = Rect::from_i(d);
        let dx = (r.x - cx).max(0.0).max(cx - r.max_x());
        let dy = (r.y - cy).max(0.0).max(cy - r.max_y());
        (dx * dx + dy * dy).sqrt() <= CURSOR_RADIUS
    }

    /// Time the screen stopped changing after `from`: last real dirty frame before a quiet gap, capped.
    fn settle_time(&self, from: f64) -> f64 {
        let quiet = self.quiet();
        let mut last = from;
        for f in self.session.frames.iter().filter(|f| f.t > from) {
            if f.t - last > quiet {
                break;
            }
            if let Some(d) = &f.dirty {
                if !self.is_cursor_only(d, f.t) {
                    last = f.t;
                }
            }
            if f.t - from > SETTLE_MAX {
                break;
            }
        }
        last
    }

    fn dirty_union(&self, a: f64, b: f64) -> Option<RectI> {
        let mut u: Option<Rect> = None;
        for f in self.session.frames.iter().filter(|f| f.t > a && f.t <= b) {
            if let Some(d) = &f.dirty {
                if !self.is_cursor_only(d, f.t) {
                    u = union_opt(u, Rect::from_i(d));
                }
            }
        }
        u.map(|r| r.to_i())
    }

    /// Copies `before` cells onto `after` around the cursor positions, so the cursor never counts as change.
    fn mask_cursor(&self, ga: &mut [u8], gb: &[u8], gw: usize, gh: usize, times: &[Option<f64>]) {
        let c = CELL as f64;
        for t in times.iter().flatten() {
            let Some((px, py)) = self.cursor_at(*t) else { continue };
            let r = CURSOR_RADIUS;
            let x0 = ((px - r) / c).max(0.0) as usize;
            let x1 = (((px + r) / c) as i64).min(gw as i64 - 1);
            let y0 = ((py - r) / c).max(0.0) as usize;
            let y1 = (((py + r) / c) as i64).min(gh as i64 - 1);
            if x1 < x0 as i64 || y1 < y0 as i64 {
                continue;
            }
            for y in y0..=y1 as usize {
                for x in x0..=x1 as usize {
                    ga[y * gw + x] = gb[y * gw + x];
                }
            }
        }
    }

    fn extract_frames(&mut self, s: &mut Step, next_input: Option<f64>) {
        if self.frames.is_none() {
            return;
        }
        let n = format!("s{:04}-{}", (s.t * 10.0) as i64, s.kind.as_str());
        let end = s.end();
        // Settle, but stop one frame short of the next input.
        let settle = |b: &Self, from: f64| -> f64 {
            let t = b.settle_time(from);
            match next_input {
                Some(n) if n - 0.03 > from => t.min(n - 0.03),
                _ => t,
            }
        };
        match s.kind {
            StepKind::WindowSwitch => {
                s.after_t = Some(settle(self, s.t));
                s.after = self.save_frame(s.after_t.unwrap(), &n, "after");
            }
            StepKind::Cursor => {
                s.after_t = Some(end);
                s.after = self.save_frame(end, &n, "after");
            }
            StepKind::ScreenUpdate => {
                s.before_t = Some(s.t - 0.03);
                s.before = self.save_frame(s.before_t.unwrap(), &n, "before");
                s.after_t = Some(settle(self, s.t));
                s.after = self.save_frame(s.after_t.unwrap(), &n, "after");
                s.dirty = self.dirty_union(s.before_t.unwrap(), s.after_t.unwrap());
            }
            _ => {
                s.before_t = Some(s.t - 0.03);
                s.before = self.save_frame(s.before_t.unwrap(), &n, "before");
                s.after_t = Some(settle(self, end));
                s.after = self.save_frame(s.after_t.unwrap(), &n, "after");
                s.dirty = self.dirty_union(s.before_t.unwrap(), s.after_t.unwrap());
            }
        }
    }

    /// A frame straight from the video, not written to disk.
    fn frame_image(&mut self, t: f64) -> Option<Arc<RgbaImage>> {
        self.frames.as_mut()?.frame(t)
    }

    fn save_frame(&mut self, t: f64, n: &str, suffix: &str) -> Option<String> {
        let img = self.frames.as_mut()?.frame(t)?;
        let rel = format!("frames/{n}-{suffix}.png");
        save_png(&img, &self.dir.join(&rel), true);
        self.images.put(rel.clone(), img);
        Some(rel)
    }

    /// Large repaints with no input nearby become "Screen updated" steps.
    fn synthesize_screen_updates(&mut self) {
        let total = (self.session.width as f64) * (self.session.height as f64);
        let mut busy: Vec<(f64, f64)> = Vec::new();
        for s in &self.session.steps {
            busy.push((s.t - 0.1, self.settle_time(s.end()) + 1.0));
        }
        let switches: Vec<f64> = self.session.steps.iter().filter(|s| s.kind == StepKind::WindowSwitch).map(|s| s.t).collect();
        // A screen change before the user's first action is recording warm-up, not a reaction to
        // anything they did. Only synthesize updates once real input has happened.
        let first_input = self.session.steps.iter().filter(|s| s.kind.is_input()).map(|s| s.t).fold(f64::INFINITY, f64::min);
        let fs = self.session.frames.clone();
        let mut extra: Vec<Step> = Vec::new();
        let mut i = 0;
        while i < fs.len() {
            let f = &fs[i];
            i += 1;
            let Some(d) = &f.dirty else { continue };
            if (d.area() as f64) / total < SCREEN_UPDATE_FRACTION {
                continue;
            }
            if f.t <= 1.0 || f.t < first_input || switches.iter().any(|s| f.t - s > -0.1 && f.t - s < 1.5) || busy.iter().any(|b| f.t >= b.0 && f.t <= b.1) {
                continue;
            }
            let win = self
                .session
                .timeline
                .iter()
                .find(|seg| seg.start <= f.t && f.t <= seg.end)
                .map(|seg| seg.window.clone())
                .or_else(|| self.session.steps.iter().rev().find(|s| s.t <= f.t).map(|s| s.window.clone()))
                .or_else(|| self.session.timeline.first().map(|seg| seg.window.clone()));
            let Some(w) = win else { continue };
            let mut s = Step::new(f.t, StepKind::ScreenUpdate, "Screen updated (no input)", w);
            s.point = Some(PointI::new(d.x + d.w / 2, d.y + d.h / 2));
            s.end_t = Some(f.t);
            extra.push(s);
            let st = self.settle_time(f.t);
            busy.push((f.t, st + 1.0));
            while i < fs.len() && fs[i].t <= st + 1.0 {
                i += 1;
            }
        }
        self.session.steps.extend(extra);
    }

    // MARK: OCR labels

    const WEAK_ROLES: [&'static str; 8] = ["scroll area", "group", "web area", "unknown", "application", "window", "layout area", "text"];

    fn target_is_weak(t: Option<&AxTarget>) -> bool {
        let Some(t) = t else { return true };
        if t.name.as_deref().is_some_and(|n| !n.is_empty()) {
            return false;
        }
        if t.value.as_deref().is_some_and(|v| !v.is_empty()) {
            return false;
        }
        let role = if t.role.is_empty() { "unknown" } else { t.role.as_str() };
        Self::WEAK_ROLES.contains(&role)
    }

    /// Last time in the 5s before `t` when the pointer was well away from `p`, if any.
    fn uncovered_time(&self, p: PointI, t: f64) -> Option<f64> {
        let reach = CURSOR_RADIUS + 20.0;
        self.session
            .cursor
            .iter()
            .rev()
            .filter(|c| c.t < t && c.t > t - 5.0)
            .find(|c| ((c.x - p.x).pow(2) + (c.y - p.y).pow(2)) as f64 > reach * reach)
            .map(|c| c.t)
    }

    /// Reads the text under/near the click from the pixels when the app gave no usable name.
    fn label_by_ocr(&mut self, s: &mut Step) {
        let Some(ocr) = self.ocr.clone() else { return };
        if !matches!(s.kind, StepKind::Click | StepKind::DoubleClick | StepKind::RightClick | StepKind::Drag) || !Self::target_is_weak(s.target.as_ref()) {
            return;
        }
        let (Some(p), Some(path)) = (s.point, s.before.clone()) else { return };
        // At click time the pointer sits on the very text we want to read. Prefer the last
        // moment before the click when the pointer was still clear of it.
        let img = match self.uncovered_time(p, s.t) {
            Some(t) => self.frame_image(t).or_else(|| self.load_image(&path)),
            None => self.load_image(&path),
        };
        let Some(img) = img else { return };
        let (w, h) = (img.width() as f64, img.height() as f64);
        let crop = Rect::new((p.x as f64 - 600.0).max(0.0), (p.y as f64 - 110.0).max(0.0), 1200.0, 220.0);
        let Some(crop) = crop.intersection(&Rect::new(0.0, 0.0, w, h)) else { return };
        let ci = imageops::crop_imm(&*img, crop.x as u32, crop.y as u32, crop.w as u32, crop.h as u32).to_image();
        let obs = ocr.recognize(&ci);
        if obs.is_empty() {
            return;
        }
        let local = (p.x as f64 - crop.x, p.y as f64 - crop.y);
        // (text, distance, off-row): same-row text outranks text above or below at any distance.
        let mut best: Option<(String, f64, bool)> = None;
        for o in obs {
            let r = Rect::from_i(&o.bounds);
            let (dist, same_row) = if r.inset(-12.0, -12.0).contains_point(local.0, local.1) {
                (0.0, true)
            } else {
                let dx = (r.x - local.0).max(0.0).max(local.0 - r.max_x());
                let dy = (r.y - local.1).max(0.0).max(local.1 - r.max_y());
                ((dx * dx + dy * dy).sqrt(), dy == 0.0)
            };
            // Text on the same row as the click (a field's own text, a row label) counts even
            // when the click landed in the middle of a wide control.
            let reach = if same_row { 600.0 } else { 90.0 };
            let key = (!same_row, dist);
            if dist <= reach && best.as_ref().is_none_or(|b| key < (b.2, b.1)) {
                best = Some((o.text.clone(), dist, !same_row));
            }
        }
        let Some((text, dist, _)) = best else { return };
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        let verb = match s.kind {
            StepKind::DoubleClick => "Double-click",
            StepKind::RightClick => "Right-click",
            StepKind::Drag => "Drag",
            _ => "Click",
        };
        let near = if dist == 0.0 { "" } else { " near" };
        let mut label = format!("{verb}{near} \"{t}\"");
        if s.kind == StepKind::Drag {
            if let Some(e) = s.end_point {
                label += &format!(" to ({},{})", e.x, e.y);
            }
        }
        s.label = label + " (text read from screen)";
        s.target = Some(AxTarget { role: "text".into(), name: Some(t.to_string()), value: None, bounds: s.target.as_ref().and_then(|t| t.bounds) });
    }

    // MARK: grouping

    fn group_runs(&self) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        for s in &self.session.steps {
            let key = s.window.key();
            let seg = self.session.timeline.iter().find(|seg| seg.start <= s.t + 0.01 && s.t <= seg.end + 0.01 && seg.window.key() == key);
            match runs.last_mut() {
                Some(last) if last.window.key() == key => {
                    if s.kind != StepKind::WindowSwitch {
                        last.steps.push(s.clone());
                    }
                    if let Some(seg) = seg {
                        last.end = last.end.max(seg.end);
                        last.inputs += seg.input_events;
                    }
                }
                _ => runs.push(Run {
                    window: s.window.clone(),
                    steps: vec![s.clone()],
                    start: seg.map_or(s.t, |g| g.start),
                    end: seg.map_or(s.t, |g| g.end),
                    inputs: seg.map_or(1, |g| g.input_events),
                }),
            }
        }
        for r in runs.iter_mut() {
            if let Some(last_t) = r.steps.last().map(|s| s.t) {
                if last_t > r.end {
                    r.end = last_t;
                }
            }
            if r.steps.iter().any(|s| s.kind != StepKind::WindowSwitch) {
                r.inputs = r.inputs.max(1);
            }
        }
        runs
    }

    // MARK: panels

    fn load_image(&mut self, rel: &str) -> Option<Arc<RgbaImage>> {
        if let Some(i) = self.images.get(rel) {
            return Some(i.clone());
        }
        let img = image::open(self.dir.join(rel)).ok()?.into_rgba8();
        let img = Arc::new(img);
        self.images.put(rel.to_string(), img.clone());
        Some(img)
    }

    fn load_opt(&mut self, rel: Option<&str>) -> Option<Arc<RgbaImage>> {
        self.load_image(rel?)
    }

    fn make_panels(&mut self, run: &Run) -> (Vec<Panel>, HashSet<usize>) {
        let mut panels: Vec<Panel> = Vec::new();
        let mut no_change: HashSet<usize> = HashSet::new();
        let mut prev_rect: Option<Rect> = None;
        let real: Vec<&Step> = run.steps.iter().filter(|s| s.kind != StepKind::WindowSwitch && s.kind != StepKind::Cursor).collect();

        if real.is_empty() {
            if let Some(sw) = run.steps.first() {
                if let Some(path) = sw.after.clone() {
                    if let Some(img) = self.load_image(&path) {
                        let r = clamp(Rect::new(0.0, 0.0, img.width() as f64, img.height() as f64), &img, &run.window);
                        panels.push(Panel { image: img, rect: r, label: Some(format!("Landed on {}", run.window.app)), marker: None, marker2: None, scale: 1.0 });
                    }
                }
            }
            return (panels, no_change);
        }

        for (i, s) in real.iter().enumerate() {
            let before = self.load_opt(s.before.as_deref());
            let after = self.load_opt(s.after.as_deref());
            let Some(reference) = after.clone().or_else(|| before.clone()) else { continue };
            if i > 0 {
                if let (Some(b), Some(a)) = (&s.before, &s.after) {
                    if self.same_image(b, a, before.as_deref(), after.as_deref(), &[s.before_t, s.after_t]) {
                        no_change.insert(s.index);
                        continue;
                    }
                }
            }
            let mut rect = self.focus_rect(s, before.as_deref(), after.as_deref(), &reference, &run.window);
            if let Some(p) = prev_rect {
                if p.inset(-8.0, -8.0).contains_rect(&rect) {
                    rect = p;
                } else if p.intersects(&rect) {
                    let u = p.union(&rect);
                    if u.w <= MAX_SIZE.0 && u.h <= MAX_SIZE.1 {
                        rect = u;
                    }
                }
            }
            prev_rect = Some(rect);
            if i > 0 && !matches!(s.kind, StepKind::Type | StepKind::Key | StepKind::Drag) {
                if let (Some(b), Some(a)) = (&before, &after) {
                    if b.width() == a.width()
                        && b.height() == a.height()
                        && self.changed_fraction(s.before.as_deref().unwrap_or(""), b, s.after.as_deref().unwrap_or(""), a, &rect, &[s.before_t, s.after_t]) < 0.02
                    {
                        no_change.insert(s.index);
                        continue;
                    }
                }
            }
            if i == 0 {
                if let (Some(b), Some(_)) = (&before, &s.before) {
                    panels.push(Panel { image: b.clone(), rect, label: Some("Before".into()), marker: None, marker2: None, scale: 1.0 });
                }
            }
            if let (Some(a), Some(_)) = (&after, &s.after) {
                let show_marker = s.kind != StepKind::Scroll && s.kind != StepKind::ScreenUpdate;
                let pt = |p: Option<PointI>| p.map(|p| (p.x as f64, p.y as f64));
                panels.push(Panel {
                    image: a.clone(),
                    rect,
                    label: Some(s.label.clone()),
                    marker: if show_marker { pt(s.point) } else { None },
                    marker2: if show_marker { pt(s.end_point) } else { None },
                    scale: 1.0,
                });
            }
        }
        (panels, no_change)
    }

    fn same_image(&mut self, pa: &str, pb: &str, a: Option<&RgbaImage>, b: Option<&RgbaImage>, times: &[Option<f64>]) -> bool {
        if pa == pb {
            return true;
        }
        let (Some(a), Some(b)) = (a, b) else { return false };
        if a.width() != b.width() || a.height() != b.height() {
            return false;
        }
        let gw = a.width() as usize / CELL;
        let gh = a.height() as usize / CELL;
        if gw == 0 || gh == 0 {
            return false;
        }
        let gb = self.gray_grid(pa, a, gw, gh);
        let mut ga = (*self.gray_grid(pb, b, gw, gh)).clone();
        self.mask_cursor(&mut ga, &gb, gw, gh, times);
        let changed = ga.iter().zip(gb.iter()).filter(|(x, y)| (**x as i32 - **y as i32).abs() > 8).count();
        changed <= 3
    }

    fn pack(&self, panels: Vec<Panel>) -> Vec<Vec<Panel>> {
        let composite_width = self.composite_width();
        let max_h = composite_width * 0.6;
        let fit = |mut ps: Vec<Panel>| -> Vec<Panel> {
            for p in ps.iter_mut() {
                p.scale = 1.0f64.min(max_h / p.rect.h);
            }
            let sum: f64 = ps.iter().map(|p| p.rect.w * p.scale).sum();
            let w = MARGIN * 2.0 + GAP * (ps.len() as f64 - 1.0) + sum;
            if w > composite_width {
                let f = (composite_width - MARGIN * 2.0 - GAP * (ps.len() as f64 - 1.0)) / sum;
                for p in ps.iter_mut() {
                    p.scale *= f;
                }
            }
            ps
        };
        let mut out: Vec<Vec<Panel>> = Vec::new();
        let mut cur: Vec<Panel> = Vec::new();
        for p in panels {
            let mut trial = cur.clone();
            trial.push(p.clone());
            let trial = fit(trial);
            if cur.is_empty() || (trial.iter().all(|t| t.scale >= MIN_PANEL_SCALE) && trial.len() <= self.panels_per_image()) {
                cur.push(p);
            } else {
                out.push(fit(std::mem::take(&mut cur)));
                cur.push(p);
            }
        }
        if !cur.is_empty() {
            out.push(fit(cur));
        }
        out
    }

    fn focus_rect(&mut self, step: &Step, before: Option<&RgbaImage>, after: Option<&RgbaImage>, reference: &RgbaImage, window: &WindowInfo) -> Rect {
        let (w, h) = (reference.width() as f64, reference.height() as f64);
        let anchor = step
            .point
            .map(|p| (p.x as f64, p.y as f64))
            .or_else(|| step.dirty.map(|d| Rect::from_i(&d).center()))
            .unwrap_or((w / 2.0, h / 2.0));
        let mut bx = Rect::around(anchor.0, anchor.1, 40.0);
        // Swift: `if let ... d = changedBBox(...) { union } else if let dirty ... { union }` — the dirty
        // rect is the fallback whenever no pixel diff was found near the anchor.
        let mut diffed = false;
        if let (Some(b), Some(a)) = (before, after) {
            if b.width() == a.width() && b.height() == a.height() {
                if let Some(d) = self.changed_bbox(step.before.as_deref().unwrap_or(""), b, step.after.as_deref().unwrap_or(""), a, anchor, DIFF_RADIUS, &[step.before_t, step.after_t]) {
                    bx = bx.union(&d);
                    diffed = true;
                }
            }
        }
        if !diffed {
            if let Some(d) = step.dirty {
                let d = Rect::from_i(&d);
                if d.w * d.h < w * h * 0.5 {
                    bx = bx.union(&d);
                }
            }
        }
        if let Some(tb) = step.target.as_ref().and_then(|t| t.bounds) {
            let tb = Rect::from_i(&tb);
            if tb.w < w * 0.9 && tb.h < h * 0.9 {
                bx = bx.union(&tb);
            }
        }
        if let Some(e) = step.end_point {
            bx = bx.union(&Rect::around(e.x as f64, e.y as f64, 40.0));
        }
        bx = bx.inset(-PAD, -PAD);
        if bx.w < MIN_SIZE.0 {
            bx = bx.inset(-(MIN_SIZE.0 - bx.w) / 2.0, 0.0);
        }
        if bx.h < MIN_SIZE.1 {
            bx = bx.inset(0.0, -(MIN_SIZE.1 - bx.h) / 2.0);
        }
        if bx.w > MAX_SIZE.0 {
            bx = Rect::new(anchor.0 - MAX_SIZE.0 / 2.0, bx.y, MAX_SIZE.0, bx.h);
        }
        if bx.h > MAX_SIZE.1 {
            bx = Rect::new(bx.x, anchor.1 - MAX_SIZE.1 / 2.0, bx.w, MAX_SIZE.1);
        }
        clamp(bx, reference, window)
    }

    #[allow(clippy::too_many_arguments)]
    fn changed_bbox(&mut self, pb: &str, before: &RgbaImage, pa: &str, after: &RgbaImage, near: (f64, f64), radius: f64, times: &[Option<f64>]) -> Option<Rect> {
        let c = CELL;
        let gw = before.width() as usize / c;
        let gh = before.height() as usize / c;
        if gw == 0 || gh == 0 {
            return None;
        }
        let gb = self.gray_grid(pb, before, gw, gh);
        let mut ga = (*self.gray_grid(pa, after, gw, gh)).clone();
        self.mask_cursor(&mut ga, &gb, gw, gh, times);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (usize::MAX, usize::MAX, -1i64, -1i64);
        let r2 = radius * radius;
        for y in 0..gh {
            for x in 0..gw {
                let i = y * gw + x;
                if (gb[i] as i32 - ga[i] as i32).abs() > 10 {
                    let cx = (x * c + c / 2) as f64;
                    let cy = (y * c + c / 2) as f64;
                    let dx = cx - near.0;
                    let dy = cy - near.1;
                    if dx * dx + dy * dy <= r2 {
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x as i64);
                        max_y = max_y.max(y as i64);
                    }
                }
            }
        }
        if max_x < 0 {
            return None;
        }
        Some(Rect::new(
            (min_x * c) as f64,
            (min_y * c) as f64,
            ((max_x as usize - min_x + 1) * c) as f64,
            ((max_y as usize - min_y + 1) * c) as f64,
        ))
    }

    fn changed_fraction(&mut self, pb: &str, before: &RgbaImage, pa: &str, after: &RgbaImage, rect: &Rect, times: &[Option<f64>]) -> f64 {
        let c = CELL;
        let gw = before.width() as usize / c;
        let gh = before.height() as usize / c;
        if gw == 0 || gh == 0 {
            return 1.0;
        }
        let gb = self.gray_grid(pb, before, gw, gh);
        let mut ga = (*self.gray_grid(pa, after, gw, gh)).clone();
        self.mask_cursor(&mut ga, &gb, gw, gh, times);
        let x0 = (rect.x.max(0.0) as usize / c).max(0);
        let x1 = ((rect.max_x() as i64 / c as i64).min(gw as i64 - 1)).max(-1);
        let y0 = (rect.y.max(0.0) as usize / c).max(0);
        let y1 = ((rect.max_y() as i64 / c as i64).min(gh as i64 - 1)).max(-1);
        if x1 < x0 as i64 || y1 < y0 as i64 {
            return 1.0;
        }
        let (mut changed, mut total) = (0usize, 0usize);
        for y in y0..=y1 as usize {
            for x in x0..=x1 as usize {
                total += 1;
                if (gb[y * gw + x] as i32 - ga[y * gw + x] as i32).abs() > 10 {
                    changed += 1;
                }
            }
        }
        changed as f64 / total.max(1) as f64
    }

    /// Mean luminance per `CELL`-sized cell, cached per frame file.
    fn gray_grid(&mut self, key: &str, img: &RgbaImage, gw: usize, gh: usize) -> Arc<Vec<u8>> {
        let cacheable = !key.is_empty() && gw == img.width() as usize / CELL && gh == img.height() as usize / CELL;
        if cacheable {
            if let Some(g) = self.gray_cache.get(key) {
                return g.clone();
            }
        }
        let g = Arc::new(gray_grid(img, gw, gh));
        if cacheable {
            self.gray_cache.insert(key.to_string(), g.clone());
        }
        g
    }

    // MARK: composite

    fn compose(&self, panels: &[Panel]) -> Option<RgbaImage> {
        if panels.is_empty() {
            return None;
        }
        let border = 1.0;
        let scaled: Vec<(&Panel, f64, (f64, f64))> = panels.iter().map(|p| (p, p.scale, (p.rect.w * p.scale, p.rect.h * p.scale))).collect();
        let total_w = MARGIN * 2.0 + scaled.iter().map(|s| s.2 .0).sum::<f64>() + GAP * (panels.len() as f64 - 1.0);
        let max_h = scaled.iter().map(|s| s.2 .1).fold(0.0, f64::max);
        let total_h = MARGIN * 2.0 + LABEL_H + max_h;
        let (wi, hi) = (total_w.ceil() as u32, total_h.ceil() as u32);
        let mut out = RgbaImage::from_pixel(wi, hi, Rgba([255, 255, 255, 255]));
        let label_color = Rgba([219, 77, 71, 255]);
        let red = [230u8, 56, 51];
        let gray_border = Rgba([191, 191, 191, 255]);
        let gray_arrow = [115u8, 115, 115];
        let px_scale = PxScale::from(24.0 * self.font.height_unscaled() / self.font.units_per_em().unwrap_or(1000.0));
        let text_h = 29.0;
        let mut x = MARGIN;
        for (i, (p, s, size)) in scaled.iter().enumerate() {
            let top = MARGIN + LABEL_H;
            let frame = Rect::new(x, top, size.0, size.1);
            let fx = frame.x.round() as i64;
            let fy = frame.y.round() as i64;
            let fw = (frame.w.round() as i64).max(1) as u32;
            let fh = (frame.h.round() as i64).max(1) as u32;
            if let Some(crop) = crop_rect(&p.image, &p.rect) {
                let drawn = if crop.width() == fw && crop.height() == fh { crop } else { imageops::resize(&crop, fw, fh, imageops::FilterType::Triangle) };
                imageops::replace(&mut out, &drawn, fx, fy);
            }
            let br = imageproc::rect::Rect::at((fx as f64 - border) as i32, (fy as f64 - border) as i32).of_size(fw + 2, fh + 2);
            imageproc::drawing::draw_hollow_rect_mut(&mut out, br, gray_border);
            if let Some(l) = &p.label {
                let text = truncate_to_width(&self.font, px_scale, l, frame.w);
                let ty = MARGIN + (LABEL_H - text_h) / 2.0;
                imageproc::drawing::draw_text_mut(&mut out, label_color, fx as i32, ty.round() as i32, px_scale, &self.font, &text);
            }
            for m in [p.marker, p.marker2].into_iter().flatten() {
                if p.rect.contains_point(m.0, m.1) {
                    draw_arrow(&mut out, (frame.x + (m.0 - p.rect.x) * s, frame.y + (m.1 - p.rect.y) * s), red);
                }
            }
            if i < scaled.len() - 1 {
                let y = top + max_h / 2.0;
                let from = (frame.max_x() + 12.0, y);
                let to = (frame.max_x() + GAP - 12.0, y);
                fill_capsule(&mut out, from, to, 2.0, gray_arrow);
                fill_triangle(&mut out, (to.0 + 10.0, y), (to.0 - 10.0, y - 10.0), (to.0 - 10.0, y + 10.0), gray_arrow);
            }
            x += size.0 + GAP;
        }
        Some(out)
    }
}

fn clamp(r: Rect, img: &RgbaImage, window: &WindowInfo) -> Rect {
    let mut bounds = Rect::new(0.0, 0.0, img.width() as f64, img.height() as f64);
    let wb = Rect::from_i(&window.bounds);
    if wb.w > 200.0 && wb.h > 200.0 {
        if let Some(b) = bounds.intersection(&wb.inset(-4.0, -4.0)) {
            bounds = b;
        } else {
            bounds = Rect::new(0.0, 0.0, 0.0, 0.0);
        }
    }
    let mut out = r;
    if out.w > bounds.w {
        out.w = bounds.w;
    }
    if out.h > bounds.h {
        out.h = bounds.h;
    }
    if out.x < bounds.x {
        out.x = bounds.x;
    }
    if out.y < bounds.y {
        out.y = bounds.y;
    }
    if out.max_x() > bounds.max_x() {
        out.x = bounds.max_x() - out.w;
    }
    if out.max_y() > bounds.max_y() {
        out.y = bounds.max_y() - out.h;
    }
    out.integral()
}

/// Consecutive scroll steps become one step (first before, last after).
fn merge_scrolls(steps: &[Step]) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    let mut dirs: Vec<&str> = Vec::new();
    for s in steps {
        let dir = if s.label.starts_with("Scroll down") { "down" } else { "up" };
        if s.kind == StepKind::Scroll && out.last().is_some_and(|l| l.kind == StepKind::Scroll) {
            dirs.push(dir);
            let last = out.last_mut().unwrap();
            last.after = s.after.clone();
            last.point = s.point;
            let mut parts: Vec<String> = Vec::new();
            let mut i = 0;
            while i < dirs.len() {
                let mut j = i;
                while j + 1 < dirs.len() && dirs[j + 1] == dirs[i] {
                    j += 1;
                }
                let n = j - i + 1;
                parts.push(if n > 1 { format!("{} ×{}", dirs[i], n) } else { dirs[i].to_string() });
                i = j + 1;
            }
            last.label = format!("Scroll {}", parts.join(", "));
        } else {
            if s.kind == StepKind::Scroll {
                dirs = vec![dir];
            }
            out.push(s.clone());
        }
    }
    out
}

fn crop_rect(img: &RgbaImage, r: &Rect) -> Option<RgbaImage> {
    let full = Rect::new(0.0, 0.0, img.width() as f64, img.height() as f64);
    let c = r.intersection(&full)?.integral();
    let (x, y, w, h) = (c.x as u32, c.y as u32, c.w as u32, c.h as u32);
    if w == 0 || h == 0 {
        return None;
    }
    Some(imageops::crop_imm(img, x, y, w, h).to_image())
}

fn gray_grid(img: &RgbaImage, gw: usize, gh: usize) -> Vec<u8> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut out = vec![0u8; gw * gh];
    let raw = img.as_raw();
    for cy in 0..gh {
        let y0 = cy * h / gh;
        let y1 = ((cy + 1) * h / gh).max(y0 + 1).min(h);
        for cx in 0..gw {
            let x0 = cx * w / gw;
            let x1 = ((cx + 1) * w / gw).max(x0 + 1).min(w);
            let mut sum: u64 = 0;
            for y in y0..y1 {
                let row = y * w * 4;
                for x in x0..x1 {
                    let i = row + x * 4;
                    sum += raw[i] as u64 * 299 + raw[i + 1] as u64 * 587 + raw[i + 2] as u64 * 114;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            out[cy * gw + cx] = (sum / (n * 1000)).min(255) as u8;
        }
    }
    out
}

fn save_png(img: &RgbaImage, path: &Path, fast: bool) {
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8();
    let Ok(file) = std::fs::File::create(path) else {
        log::warn!("cannot create {}", path.display());
        return;
    };
    let w = std::io::BufWriter::new(file);
    let enc = PngEncoder::new_with_quality(w, if fast { CompressionType::Fast } else { CompressionType::Default }, PngFilter::Adaptive);
    if let Err(e) = enc.write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8) {
        log::warn!("cannot write {}: {e}", path.display());
    }
}

fn text_width(font: &FontRef<'_>, scale: PxScale, text: &str) -> f64 {
    let f = font.as_scaled(scale);
    let mut w = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            w += f.kern(p, id);
        }
        w += f.h_advance(id);
        prev = Some(id);
    }
    w as f64
}

/// Truncates with an ellipsis so the label fits in `max_w` pixels.
fn truncate_to_width(font: &FontRef<'_>, scale: PxScale, text: &str, max_w: f64) -> String {
    if text_width(font, scale, text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut n = chars.len();
    while n > 0 {
        n -= 1;
        let cand: String = chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
        if text_width(font, scale, &cand) <= max_w {
            return cand;
        }
    }
    "…".to_string()
}

/// Anti-aliased fill of the pixels for which `inside` holds, 4×4 supersampled within `bbox`.
fn paint(img: &mut RgbaImage, bbox: Rect, color: [u8; 3], inside: impl Fn(f64, f64) -> bool) {
    let x0 = bbox.x.floor().max(0.0) as i64;
    let y0 = bbox.y.floor().max(0.0) as i64;
    let x1 = (bbox.max_x().ceil() as i64).min(img.width() as i64);
    let y1 = (bbox.max_y().ceil() as i64).min(img.height() as i64);
    for py in y0..y1 {
        for px in x0..x1 {
            let mut count = 0;
            for j in 0..4 {
                for i in 0..4 {
                    if inside(px as f64 + (i as f64 + 0.5) / 4.0, py as f64 + (j as f64 + 0.5) / 4.0) {
                        count += 1;
                    }
                }
            }
            if count == 0 {
                continue;
            }
            let a = count as f64 / 16.0;
            let p = img.get_pixel_mut(px as u32, py as u32);
            for (dst, &c) in p.0.iter_mut().zip(color.iter()) {
                *dst = (*dst as f64 * (1.0 - a) + c as f64 * a).round() as u8;
            }
            p.0[3] = 255;
        }
    }
}

fn seg_dist2(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let len2 = vx * vx + vy * vy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0) };
    let (cx, cy) = (a.0 + t * vx, a.1 + t * vy);
    (p.0 - cx).powi(2) + (p.1 - cy).powi(2)
}

fn fill_capsule(img: &mut RgbaImage, a: (f64, f64), b: (f64, f64), r: f64, color: [u8; 3]) {
    let bbox = Rect::new(a.0.min(b.0) - r - 1.0, a.1.min(b.1) - r - 1.0, (a.0 - b.0).abs() + 2.0 * r + 2.0, (a.1 - b.1).abs() + 2.0 * r + 2.0);
    paint(img, bbox, color, |x, y| seg_dist2((x, y), a, b) <= r * r);
}

fn fill_triangle(img: &mut RgbaImage, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), color: [u8; 3]) {
    let min_x = p0.0.min(p1.0).min(p2.0);
    let min_y = p0.1.min(p1.1).min(p2.1);
    let max_x = p0.0.max(p1.0).max(p2.0);
    let max_y = p0.1.max(p1.1).max(p2.1);
    let bbox = Rect::new(min_x - 1.0, min_y - 1.0, max_x - min_x + 2.0, max_y - min_y + 2.0);
    let edge = |a: (f64, f64), b: (f64, f64), p: (f64, f64)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    paint(img, bbox, color, |x, y| {
        let p = (x, y);
        let (d0, d1, d2) = (edge(p0, p1, p), edge(p1, p2, p), edge(p2, p0, p));
        (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
    });
}

fn fill_ring(img: &mut RgbaImage, c: (f64, f64), r_in: f64, r_out: f64, color: [u8; 3]) {
    let bbox = Rect::new(c.0 - r_out - 1.0, c.1 - r_out - 1.0, 2.0 * r_out + 2.0, 2.0 * r_out + 2.0);
    paint(img, bbox, color, |x, y| {
        let d2 = (x - c.0).powi(2) + (y - c.1).powi(2);
        d2 >= r_in * r_in && d2 <= r_out * r_out
    });
}

/// Red arrow pointing down-right into `p`, with a small ring on the point.
fn draw_arrow(img: &mut RgbaImage, p: (f64, f64), color: [u8; 3]) {
    let len = 110.0;
    let d = (1.0 / 2f64.sqrt(), 1.0 / 2f64.sqrt());
    let tail = (p.0 - d.0 * len, p.1 - d.1 * len);
    let head = (p.0 - d.0 * 22.0, p.1 - d.1 * 22.0);
    fill_capsule(img, tail, head, 3.0, color);
    let perp = (-d.1, d.0);
    let base = (p.0 - d.0 * 34.0, p.1 - d.1 * 34.0);
    fill_triangle(
        img,
        (p.0 - d.0 * 12.0, p.1 - d.1 * 12.0),
        (base.0 + perp.0 * 13.0, base.1 + perp.1 * 13.0),
        (base.0 - perp.0 * 13.0, base.1 - perp.1 * 13.0),
        color,
    );
    fill_ring(img, p, 7.5, 10.5, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_matches_claude_formula() {
        assert_eq!(image_tokens(1000, 1000), 1333);
        assert_eq!(image_tokens(800, 600), 640);
        assert_eq!(image_tokens(3024, 1964), 1533, "capped at 1.15MP");
        assert_eq!(image_tokens(2000, 500), 819, "capped at 1568 long edge");
    }

    #[test]
    fn scroll_merge_wording() {
        let w = WindowInfo::default();
        let mk = |label: &str| {
            let mut s = Step::new(0.0, StepKind::Scroll, label, w.clone());
            s.after = Some(label.to_string());
            s
        };
        let steps = vec![mk("Scroll down in x"), mk("Scroll down"), mk("Scroll up"), mk("Scroll down")];
        let out = merge_scrolls(&steps);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Scroll down ×2, up, down");
        assert_eq!(out[0].after.as_deref(), Some("Scroll down"));
    }

    #[test]
    fn rect_semantics() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(10.0, 0.0, 5.0, 5.0);
        assert!(!a.intersects(&b));
        assert!(a.contains_rect(&Rect::new(1.0, 1.0, 9.0, 9.0)));
        assert_eq!(Rect::new(0.4, 0.6, 1.2, 1.2).integral(), Rect::new(0.0, 0.0, 2.0, 2.0));
    }

    #[test]
    fn label_truncation_and_arrow_paint() {
        let font = FontRef::try_from_slice(FONT_DATA).unwrap();
        let scale = PxScale::from(29.0);
        let t = truncate_to_width(&font, scale, "Click button \"Manage Extensions and a very long name\"", 200.0);
        assert!(t.ends_with('…') && t.chars().count() < 30);
        let mut img = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
        draw_arrow(&mut img, (150.0, 150.0), [230, 56, 51]);
        assert_eq!(img.get_pixel(100, 100).0[1], 56);
        assert_eq!(img.get_pixel(10, 10).0, [255, 255, 255, 255]);
    }
}
