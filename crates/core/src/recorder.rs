//! Records a video of the main display plus a timestamped log of everything that happened:
//! every frame's arrival and dirty area, every gesture, every window switch. Accessibility
//! names are attached when an app answers quickly; nothing waits on them.
//!
//! Threads: the backend delivers frames and input on its own threads; a 30 fps ticker thread feeds
//! the encoder (re-sending the last frame when nothing changed); a worker thread owns the window
//! tracker, the coalescer and the session and does all bookkeeping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::encoder::{VideoWriter, FPS};
use crate::model::{fmt_t, CursorSample, FrameLog, PointI, RectI, Session, Step, StepKind, WindowSegment};
use crate::options::Options;
use crate::platform::{AxTarget, Backend, CaptureInfo, Frame, RawInput, RawKind, ScreenCapture, WindowInfo, WindowTracker};
use crate::semantics::{CoalesceEvent, Coalescer, Gesture, GestureKind};

pub const VIDEO_FILE: &str = "recording.mp4";
const POLL_INTERVAL: Duration = Duration::from_millis(300);
const CURSOR_SAMPLE_GAP: f64 = 0.05;

type StepCallback = Arc<dyn Fn(&Step) + Send + Sync>;

enum Msg {
    Input(RawInput),
    Stop,
}

#[derive(Default)]
struct Shared {
    /// Newest captured frame and whether it has not been written yet.
    latest: Mutex<(Option<Frame>, bool)>,
    frame_log: Mutex<Vec<FrameLog>>,
    video_offset: Mutex<Option<f64>>,
    stop: AtomicBool,
}

struct Running {
    tx: Sender<Msg>,
    worker: JoinHandle<(Session, Vec<CursorSample>)>,
    ticker: JoinHandle<VideoWriter>,
    capture: Box<dyn ScreenCapture>,
    input: Box<dyn crate::platform::InputTap>,
    shared: Arc<Shared>,
}

pub struct Recorder {
    out_dir: PathBuf,
    options: Options,
    /// Bundle id / process id of the recorder UI itself; interactions with it are ignored.
    pub ignore_bundle_id: Option<String>,
    /// Global rect (points, top-left origin) of our own tray/menu item; clicks inside are ignored.
    pub ignore_rect: Option<RectI>,
    on_step: Option<StepCallback>,
    backend: Option<Backend>,
    running: Option<Running>,
}

impl Recorder {
    pub fn new(backend: Backend, out_dir: impl Into<PathBuf>, options: Options) -> Self {
        Recorder {
            out_dir: out_dir.into(),
            options,
            ignore_bundle_id: None,
            ignore_rect: None,
            on_step: None,
            backend: Some(backend),
            running: None,
        }
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// Called on the worker thread for every appended step.
    pub fn on_step(&mut self, f: impl Fn(&Step) + Send + Sync + 'static) {
        self.on_step = Some(Arc::new(f));
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn start(&mut self) -> Result<()> {
        if self.running.is_some() {
            anyhow::bail!("already recording");
        }
        let mut backend = self.backend.take().context("recorder already used")?;
        std::fs::create_dir_all(&self.out_dir).with_context(|| format!("creating {}", self.out_dir.display()))?;
        let t0 = Instant::now();
        let shared = Arc::new(Shared::default());

        let sh = shared.clone();
        let info: CaptureInfo = backend.capture.start(
            FPS,
            Box::new(move |frame: Frame| {
                let mut l = sh.latest.lock().unwrap_or_else(|e| e.into_inner());
                *l = (Some(frame), true);
            }),
        )?;

        let mut session = Session::new(info.width, info.height, info.scale);
        session.video = Some(VIDEO_FILE.to_string());
        let writer = VideoWriter::new(&self.out_dir.join(VIDEO_FILE), info.width, info.height)?;
        let encoder = writer.encoder().to_string();

        let ticker = {
            let sh = shared.clone();
            let (w, h) = (info.width, info.height);
            std::thread::Builder::new()
                .name("agent-snap.ticker".into())
                .spawn(move || tick_loop(writer, sh, t0, w, h))
                .context("spawning ticker")?
        };

        let (tx, rx) = mpsc::channel::<Msg>();
        let worker = {
            let mut w = Worker {
                tracker: backend.windows,
                coalescer: Coalescer::new(),
                session,
                info: info.clone(),
                options: self.options.clone(),
                t0,
                ignore_bundle_id: self.ignore_bundle_id.clone(),
                ignore_rect: self.ignore_rect,
                on_step: self.on_step.clone(),
                cursor_log: Vec::new(),
                last_cursor_t: -1.0,
                pending: None,
                pending_ignored: false,
                current_window: None,
                segment_start: 0.0,
                segment_inputs: 0,
                stopped: false,
            };
            std::thread::Builder::new()
                .name("agent-snap.recorder".into())
                .spawn(move || {
                    w.run(rx);
                    (w.session, w.cursor_log)
                })
                .context("spawning worker")?
        };

        let itx = tx.clone();
        backend.input.start(Box::new(move |raw: RawInput| {
            let _ = itx.send(Msg::Input(raw));
        }))?;

        log::info!("recording {}x{} @{}x ({encoder}) -> {}", info.width, info.height, info.scale, self.out_dir.display());
        self.running = Some(Running { tx, worker, ticker, capture: backend.capture, input: backend.input, shared });
        Ok(())
    }

    /// Stops everything, writes `session.json` and returns the session.
    pub fn stop(&mut self) -> Result<Session> {
        let mut r = self.running.take().context("not recording")?;
        r.input.stop();
        let _ = r.tx.send(Msg::Stop);
        let (mut session, cursor) = r.worker.join().map_err(|_| anyhow::anyhow!("recorder worker panicked"))?;
        r.shared.stop.store(true, Ordering::SeqCst);
        let writer = r.ticker.join().map_err(|_| anyhow::anyhow!("ticker panicked"))?;
        r.capture.stop();
        let frames = writer.frames();
        if let Err(e) = writer.finish() {
            log::error!("video writer: {e:#}");
        }
        session.frames = std::mem::take(&mut *r.shared.frame_log.lock().unwrap_or_else(|e| e.into_inner()));
        session.cursor = cursor;
        session.video_offset = r.shared.video_offset.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(0.0);
        if frames == 0 {
            session.video = None;
        }
        session.save(&self.out_dir)?;
        log::info!("video frames: {frames}");
        Ok(session)
    }
}

/// Constant frame rate: one frame per tick, re-sending the last one when the screen was static.
fn tick_loop(mut writer: VideoWriter, sh: Arc<Shared>, t0: Instant, w: u32, h: u32) -> VideoWriter {
    let period = Duration::from_secs_f64(1.0 / FPS as f64);
    let mut first: Option<Instant> = None;
    let mut n: u64 = 0;
    'outer: loop {
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
        let nominal = match first {
            Some(f) => {
                let target = f + period * n as u32;
                while Instant::now() < target {
                    if sh.stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    std::thread::sleep((target - Instant::now()).min(Duration::from_millis(2)));
                }
                target
            }
            None => {
                let has = sh.latest.lock().unwrap_or_else(|e| e.into_inner()).0.is_some();
                if !has {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let now = Instant::now();
                first = Some(now);
                *sh.video_offset.lock().unwrap_or_else(|e| e.into_inner()) = Some(now.duration_since(t0).as_secs_f64());
                now
            }
        };
        let (frame, is_new) = {
            let mut l = sh.latest.lock().unwrap_or_else(|e| e.into_inner());
            let out = (l.0.clone(), l.1);
            l.1 = false;
            out
        };
        let Some(frame) = frame else { continue };
        let ok = if is_new { writer.write(&frame) } else { writer.write_duplicate() };
        if ok {
            let dirty = if is_new {
                Some(frame.dirty.iter().fold(None, |acc: Option<RectI>, d| Some(acc.map_or(*d, |a| a.union(d)))).unwrap_or(RectI::new(0, 0, w as i32, h as i32)))
            } else {
                None
            };
            sh.frame_log.lock().unwrap_or_else(|e| e.into_inner()).push(FrameLog { t: nominal.duration_since(t0).as_secs_f64(), dirty });
        }
        n += 1;
    }
    writer
}

struct Pending {
    target: Option<AxTarget>,
    window: WindowInfo,
    secure: bool,
}

struct Worker {
    tracker: Box<dyn WindowTracker>,
    coalescer: Coalescer,
    session: Session,
    info: CaptureInfo,
    options: Options,
    t0: Instant,
    ignore_bundle_id: Option<String>,
    ignore_rect: Option<RectI>,
    on_step: Option<StepCallback>,
    cursor_log: Vec<CursorSample>,
    last_cursor_t: f64,
    pending: Option<Pending>,
    pending_ignored: bool,
    current_window: Option<WindowInfo>,
    segment_start: f64,
    segment_inputs: usize,
    stopped: bool,
}

impl Worker {
    fn run(&mut self, rx: mpsc::Receiver<Msg>) {
        let mut next_poll = Instant::now();
        loop {
            let now = Instant::now();
            let mut deadline = next_poll;
            if let Some(d) = self.coalescer.next_deadline() {
                deadline = deadline.min(d);
            }
            let wait = deadline.saturating_duration_since(now);
            match rx.recv_timeout(wait) {
                Ok(Msg::Input(raw)) => self.handle_input(raw),
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            self.coalescer.poll(now);
            self.process_events();
            if now >= next_poll {
                self.poll_window();
                next_poll = now + POLL_INTERVAL;
            }
        }
        self.coalescer.flush_all();
        self.process_events();
        self.stopped = true;
        let now = self.elapsed_now();
        if let Some(w) = self.current_window.take() {
            self.session.timeline.push(WindowSegment { start: self.segment_start, end: now, window: w, input_events: self.segment_inputs });
        }
    }

    fn elapsed(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.t0).as_secs_f64()
    }
    fn elapsed_now(&self) -> f64 {
        self.elapsed(Instant::now())
    }
    fn px(&self, x: f64, y: f64) -> PointI {
        PointI::from_f64(x * self.info.scale, y * self.info.scale)
    }
    fn px_rect(&self, r: &RectI) -> RectI {
        let s = self.info.scale;
        RectI::from_f64(r.x as f64 * s, r.y as f64 * s, r.w as f64 * s, r.h as f64 * s)
    }
    fn px_target(&self, mut t: AxTarget) -> AxTarget {
        t.bounds = t.bounds.map(|b| self.px_rect(&b));
        t
    }

    /// Frontmost window with URL (when enabled), locality flag and pixel bounds.
    fn front_window(&mut self) -> WindowInfo {
        let mut w = self.tracker.front();
        if !self.options.capture_urls {
            w.url = None;
        } else if w.url.is_none() {
            w.url = self.tracker.tab_url(&w);
        }
        w.is_local = w.url.as_deref().map(is_local_url);
        w.bounds = self.px_rect(&w.bounds);
        w
    }

    fn handle_input(&mut self, raw: RawInput) {
        if self.stopped {
            return;
        }
        if matches!(raw.kind, RawKind::Move | RawKind::Drag(_)) {
            let t = self.elapsed(raw.t);
            if t - self.last_cursor_t >= CURSOR_SAMPLE_GAP {
                let p = self.px(raw.x, raw.y);
                self.cursor_log.push(CursorSample { t, x: p.x, y: p.y });
                self.last_cursor_t = t;
            }
            self.coalescer.handle(raw);
            self.process_events();
            return;
        }
        self.segment_inputs += 1;
        self.coalescer.handle(raw);
        self.process_events();
    }

    fn process_events(&mut self) {
        for ev in self.coalescer.drain() {
            match ev {
                CoalesceEvent::Begin(raw) => self.begin_gesture(&raw),
                CoalesceEvent::Gesture(g) => self.finish_gesture(&g),
            }
        }
    }

    fn is_mine(&self, w: &WindowInfo) -> bool {
        self.ignore_bundle_id.as_deref().is_some_and(|m| !m.is_empty() && m == w.bundle_id)
    }

    fn begin_gesture(&mut self, raw: &RawInput) {
        if self.stopped {
            return;
        }
        let win = self.front_window();
        if self.is_mine(&win) {
            self.pending_ignored = true;
            return;
        }
        if let Some(r) = self.ignore_rect {
            if matches!(raw.kind, RawKind::Down(_)) && r.contains_point(raw.x, raw.y) {
                self.pending_ignored = true;
                return;
            }
        }
        self.pending_ignored = false;
        if self.pending.is_some() {
            return;
        }
        let mut target = None;
        let mut secure = false;
        match raw.kind {
            RawKind::Down(_) => {
                target = self.tracker.hit_test(raw.x, raw.y).map(|t| self.px_target(t));
            }
            RawKind::KeyDown { .. } => {
                secure = self.tracker.focused_is_secure();
                target = self.tracker.focused().map(|t| self.px_target(t));
            }
            _ => {}
        }
        self.pending = Some(Pending { target, window: win, secure });
    }

    fn finish_gesture(&mut self, g: &Gesture) {
        if self.stopped {
            return;
        }
        let start = self.elapsed(g.start_t);
        let end = self.elapsed(g.end_t);
        if g.kind == GestureKind::Cursor {
            let win = self.front_window();
            if self.is_mine(&win) {
                return;
            }
            let target = self.tracker.hit_test(g.x, g.y).map(|t| self.px_target(t));
            let pt = self.px(g.x, g.y);
            let secs = format!("{:.1}", end - start);
            let r = (g.radius * self.info.scale).round() as i64;
            let path = (g.path_length * self.info.scale).round() as i64;
            let over = target.as_ref().map(|t| format!(" over {}", t.describe())).unwrap_or_default();
            let label = format!("Cursor moved{over} for {secs}s, no click (path {path}px, within {r}px of ({},{}))", pt.x, pt.y);
            let mut step = Step::new(start, StepKind::Cursor, label, win);
            step.point = Some(pt);
            step.target = target;
            step.end_t = Some(end);
            self.append(step);
            return;
        }
        if self.pending_ignored {
            self.pending_ignored = false;
            self.pending = None;
            return;
        }
        let Some(p) = self.pending.take() else { return };
        let target = p.target;
        let pt = self.px(g.x, g.y);
        let mut step = Step::new(start, StepKind::Click, "", p.window);
        step.point = Some(pt);
        step.end_point = g.end_point.map(|(x, y)| self.px(x, y));
        step.end_t = Some(end);
        let tdesc = target.as_ref().map(|t| format!(" {}", t.describe())).unwrap_or_default();
        let anchor_ok = target
            .as_ref()
            .and_then(|t| t.bounds)
            .is_some_and(|b| b.w < self.info.width as i32 / 2 && b.h < self.info.height as i32 / 2);
        let anchor = target.as_ref().and_then(|t| t.bounds).map(|b| PointI::new(b.x + b.w / 2, b.y + b.h / 2));
        match g.kind {
            GestureKind::Click => {
                step.kind = StepKind::Click;
                step.label = if tdesc.is_empty() { format!("Click at ({},{})", pt.x, pt.y) } else { format!("Click{tdesc}") };
            }
            GestureKind::DoubleClick => {
                step.kind = StepKind::DoubleClick;
                step.label = format!("Double-click{tdesc}");
            }
            GestureKind::RightClick => {
                step.kind = StepKind::RightClick;
                step.label = format!("Right-click{tdesc}");
            }
            GestureKind::Drag => {
                step.kind = StepKind::Drag;
                let e = step.end_point.unwrap_or(pt);
                step.label = format!("Drag{tdesc} to ({},{})", e.x, e.y);
            }
            GestureKind::Type => {
                step.kind = StepKind::Type;
                let text = if p.secure || !self.options.capture_typed_text { "[redacted]".to_string() } else { g.text.clone() };
                step.label = format!("Type \"{text}\"{}", if tdesc.is_empty() { String::new() } else { format!(" in{tdesc}") });
                step.text = Some(text);
                if anchor_ok {
                    step.point = anchor;
                }
            }
            GestureKind::Key => {
                step.kind = StepKind::Key;
                step.text = Some(g.text.clone());
                step.label = format!("Press {}", g.text);
                if anchor_ok {
                    step.point = anchor;
                }
            }
            GestureKind::Scroll => {
                step.kind = StepKind::Scroll;
                let dir = if g.scroll_dy > 0.0 { "down" } else { "up" };
                step.label = format!("Scroll {dir}{}", if tdesc.is_empty() { String::new() } else { format!(" in{tdesc}") });
            }
            GestureKind::Cursor => return,
        }
        step.target = target;
        self.append(step);
    }

    fn append(&mut self, mut step: Step) {
        step.index = self.session.steps.len() + 1;
        log::info!("[{}] {}", fmt_t(step.t), step.label);
        if let Some(cb) = &self.on_step {
            cb(&step);
        }
        self.session.steps.push(step);
    }

    fn poll_window(&mut self) {
        if self.stopped {
            return;
        }
        let w = self.front_window();
        if self.is_mine(&w) {
            return;
        }
        let now = self.elapsed_now();
        if let Some(cur) = self.current_window.clone() {
            if cur.key() == w.key() {
                if cur != w {
                    self.current_window = Some(w);
                }
                return;
            }
            self.coalescer.flush_all();
            self.process_events();
            self.session.timeline.push(WindowSegment { start: self.segment_start, end: now, window: cur, input_events: self.segment_inputs });
        }
        self.current_window = Some(w.clone());
        self.segment_start = now;
        self.segment_inputs = 0;
        if self.session.timeline.is_empty() && self.session.steps.is_empty() {
            return;
        }
        let step = Step::new(now, StepKind::WindowSwitch, format!("Switch to {}", w.describe()), w);
        self.append(step);
    }
}

/// True for localhost-style hosts (same rule as the Swift tracker).
pub fn is_local_url(url: &str) -> bool {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = if hostport.starts_with('[') {
        hostport.trim_start_matches('[').split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host == "127.0.0.1" || host == "0.0.0.0" || host.ends_with(".local") || host.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls() {
        assert!(is_local_url("http://localhost:3000/x"));
        assert!(is_local_url("http://127.0.0.1/"));
        assert!(is_local_url("https://app.localhost/a?b=c"));
        assert!(is_local_url("http://mac.local:8080"));
        assert!(!is_local_url("https://example.com/localhost"));
        assert!(!is_local_url("https://localhost.example.com"));
    }
}
