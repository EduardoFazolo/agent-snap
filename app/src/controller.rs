//! Recording state machine shared by the tray app (mirrors the Swift `AppController`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agent_snap_core::platform::RectI;
use agent_snap_core::{fmt_t, Builder, Options, Recorder};
use serde::Serialize;

use crate::platform;
use crate::sessions::{self, SessionInfo};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Starting,
    Recording,
    Building,
    Done { path: String, tokens: usize },
    Failed(String),
}

struct Inner {
    phase: Phase,
    options: Options,
    started_at: Option<Instant>,
    step_count: usize,
    last_step: String,
    sessions: Vec<SessionInfo>,
    /// Global rect (points, top-left origin) of the tray icon, from the last tray click.
    tray_rect: Option<RectI>,
}

/// Snapshot pushed to the UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: &'static str,
    pub path: Option<String>,
    pub path_short: Option<String>,
    pub tokens: Option<usize>,
    pub message: Option<String>,
    pub elapsed: String,
    pub step_count: usize,
    pub last_step: String,
    pub options: Options,
    pub output_dir_short: String,
    pub sessions: Vec<SessionInfo>,
    pub platform: &'static str,
    pub bundle_id: &'static str,
}

/// Called with a fresh snapshot after every state change.
type Listener = Box<dyn Fn(&Snapshot) + Send>;

#[derive(Clone)]
pub struct Controller {
    inner: Arc<Mutex<Inner>>,
    recorder: Arc<Mutex<Option<Recorder>>>,
    listeners: Arc<Mutex<Vec<Listener>>>,
}

impl Controller {
    pub fn new() -> Self {
        let options = Options::load();
        let sessions = sessions::list(&options.output_dir);
        Controller {
            inner: Arc::new(Mutex::new(Inner {
                phase: Phase::Idle,
                options,
                started_at: None,
                step_count: 0,
                last_step: String::new(),
                sessions,
                tray_rect: None,
            })),
            recorder: Arc::new(Mutex::new(None)),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called with a fresh snapshot after every state change (from any thread).
    pub fn on_change(&self, f: impl Fn(&Snapshot) + Send + 'static) {
        self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(Box::new(f));
    }

    pub fn notify(&self) {
        let snap = self.snapshot();
        for l in self.listeners.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            l(&snap);
        }
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase.clone()
    }

    pub fn options(&self) -> Options {
        self.lock().options.clone()
    }

    pub fn set_options(&self, o: Options) {
        let mut g = self.lock();
        let refresh = g.options.output_dir != o.output_dir;
        g.options = o;
        if let Err(e) = g.options.save() {
            log::warn!("saving options: {e}");
        }
        if refresh {
            g.sessions = sessions::list(&g.options.output_dir);
        }
        drop(g);
        self.notify();
    }

    pub fn set_tray_rect(&self, r: Option<RectI>) {
        self.lock().tray_rect = r;
    }

    pub fn refresh_sessions(&self) {
        let mut g = self.lock();
        g.sessions = sessions::list(&g.options.output_dir);
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.phase(), Phase::Starting | Phase::Recording | Phase::Building)
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.lock();
        let (phase, path, tokens, message) = match &g.phase {
            Phase::Idle => ("idle", None, None, None),
            Phase::Starting => ("starting", None, None, None),
            Phase::Recording => ("recording", None, None, None),
            Phase::Building => ("building", None, None, None),
            Phase::Done { path, tokens } => ("done", Some(path.clone()), Some(*tokens), None),
            Phase::Failed(m) => ("failed", None, None, Some(m.clone())),
        };
        let elapsed = g.started_at.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
        Snapshot {
            phase,
            path_short: path.as_deref().map(platform::abbreviate),
            path,
            tokens,
            message,
            elapsed: fmt_t(elapsed),
            step_count: g.step_count,
            last_step: g.last_step.clone(),
            options: g.options.clone(),
            output_dir_short: platform::abbreviate(&g.options.output_dir),
            sessions: g.sessions.clone(),
            platform: if cfg!(target_os = "macos") { "macos" } else if cfg!(target_os = "windows") { "windows" } else { "linux" },
            bundle_id: platform::BUNDLE_ID,
        }
    }

    fn set_phase(&self, p: Phase) {
        self.lock().phase = p;
        self.notify();
    }

    /// Starts a recording on a background thread after a short delay (lets the popover close).
    pub fn start(&self) {
        if self.is_busy() {
            return;
        }
        let (dir, options, tray_rect) = {
            let mut g = self.lock();
            g.step_count = 0;
            g.last_step.clear();
            g.started_at = None;
            (g.options.new_session_dir(), g.options.clone(), g.tray_rect)
        };
        self.set_phase(Phase::Starting);
        let this = self.clone();
        std::thread::Builder::new()
            .name("agent-snap-start".into())
            .spawn(move || this.start_blocking(dir, options, tray_rect))
            .ok();
    }

    fn start_blocking(&self, dir: PathBuf, options: Options, tray_rect: Option<RectI>) {
        std::thread::sleep(Duration::from_millis(300));
        let mut rec = Recorder::new(platform::backend(), dir, options);
        rec.ignore_bundle_id = Some(platform::BUNDLE_ID.to_string());
        rec.ignore_rect = tray_rect;
        let this = self.clone();
        rec.on_step(move |step| {
            {
                let mut g = this.lock();
                g.step_count += 1;
                g.last_step = format!("{} {}", fmt_t(step.t), step.label);
            }
            this.notify();
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rec.start()));
        match result {
            Ok(Ok(())) => {
                *self.recorder.lock().unwrap_or_else(|e| e.into_inner()) = Some(rec);
                self.lock().started_at = Some(Instant::now());
                self.set_phase(Phase::Recording);
            }
            Ok(Err(e)) => {
                log::error!("start failed: {e:#}");
                self.set_phase(Phase::Failed(format!("start failed: {e:#}")));
            }
            Err(_) => self.set_phase(Phase::Failed("start failed: recorder panicked".into())),
        }
    }

    /// Stops and builds on a background thread.
    pub fn stop(&self) {
        if self.phase() != Phase::Recording {
            return;
        }
        self.set_phase(Phase::Building);
        let this = self.clone();
        std::thread::Builder::new()
            .name("agent-snap-stop".into())
            .spawn(move || this.stop_blocking())
            .ok();
    }

    fn stop_blocking(&self) {
        let Some(mut rec) = self.recorder.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            self.set_phase(Phase::Failed("not recording".into()));
            return;
        };
        let options = self.options();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> anyhow::Result<(PathBuf, usize)> {
            rec.stop()?;
            let mut b = Builder::new(rec.out_dir(), options, Some(platform::ocr()))?;
            let md = b.build()?;
            Ok((md, b.stats().tokens()))
        }));
        self.lock().started_at = None;
        match result {
            Ok(Ok((md, tokens))) => {
                self.refresh_sessions();
                self.set_phase(Phase::Done { path: md.to_string_lossy().into_owned(), tokens });
            }
            Ok(Err(e)) => {
                log::error!("build failed: {e:#}");
                self.set_phase(Phase::Failed(format!("build failed: {e:#}")));
            }
            Err(_) => self.set_phase(Phase::Failed("build failed: builder panicked".into())),
        }
    }
}
