//! agent-snap: one binary that is both the CLI (`record`, `build`) and the tray app (`app`).

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod app;
mod controller;
mod icons;
mod platform;
mod sessions;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_snap_core::platform::{PermState, Permission};
use agent_snap_core::{Builder, Options, Recorder};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-snap", version, about = "Token-efficient screen recording for coding agents", long_about = None)]
#[command(after_help = "record: captures a screen video plus every click/keystroke/scroll/window switch,\n        then builds DIR/flow.md with focused composites.\nbuild:  (re)generate flow.md + composites from an existing session DIR.\napp:    tray UI (also what AgentSnap.app runs).")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record the screen until Ctrl-C (or --duration), then build flow.md.
    Record {
        /// Session directory (default: <output dir>/sessions/<timestamp>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Stop automatically after this many seconds.
        #[arg(long)]
        duration: Option<f64>,
        /// Only record; skip building flow.md.
        #[arg(long)]
        no_build: bool,
    },
    /// (Re)generate flow.md + composites from an existing session directory.
    Build { dir: PathBuf },
    /// Tray app with the popover UI.
    App,
}

/// True when launched from an app bundle without arguments (macOS) / without a console (Windows).
fn launched_as_app() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::current_exe().map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/")).unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        !attach_parent_console()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn attach_parent_console() -> bool {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

fn init_logging(to_file: bool) {
    let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if to_file {
        let dir = PathBuf::from(&Options::load().output_dir);
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("agent-snap.log")) {
            b.target(env_logger::Target::Pipe(Box::new(f)));
        }
    }
    b.format(|buf, rec| writeln!(buf, "{} {:<5} {}: {}", chrono_like_now(), rec.level(), rec.target(), rec.args())).init();
}

/// Local time `HH:MM:SS` without pulling in chrono here.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

fn main() {
    // macOS passes `-psn_...` when launched from Finder; ignore it.
    let args: Vec<String> = std::env::args().filter(|a| !a.starts_with("-psn_")).collect();
    let as_app = args.len() == 1 && launched_as_app();
    #[cfg(target_os = "windows")]
    if !as_app {
        // Console attached above; nothing else to do.
    }

    if as_app {
        init_logging(true);
        run_app();
        return;
    }

    let cli = Cli::parse_from(args);
    match cli.cmd {
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            std::process::exit(1);
        }
        Some(Cmd::App) => {
            init_logging(true);
            run_app();
        }
        Some(Cmd::Build { dir }) => {
            init_logging(false);
            match build(&dir) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("build failed: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Some(Cmd::Record { out, duration, no_build }) => {
            init_logging(false);
            std::process::exit(record(out, duration, no_build));
        }
    }
}

fn run_app() {
    if let Err(e) = app::run() {
        log::error!("app failed: {e:#}");
        eprintln!("app failed: {e:#}");
        std::process::exit(1);
    }
}

/// Builds `dir/flow.md`; prints the path on stdout and the token line on stderr.
fn build(dir: &std::path::Path) -> anyhow::Result<()> {
    let mut b = Builder::new(dir, Options::load(), Some(platform::ocr()))?;
    let md = b.build()?;
    let s = b.stats();
    println!("{}", md.display());
    eprintln!("~{} tokens ({} images ~{}, text ~{})", s.tokens(), s.images, s.image_tokens, s.text_tokens);
    Ok(())
}

fn record(out: Option<PathBuf>, duration: Option<f64>, no_build: bool) -> i32 {
    let options = Options::load();
    let out_dir = out.unwrap_or_else(|| options.new_session_dir());

    let perms = platform::permissions();
    if perms.state(Permission::ScreenRecording) != PermState::Granted {
        perms.request(Permission::ScreenRecording);
        eprintln!("Screen Recording permission needed. Grant it to your terminal (or AgentSnap.app) in System Settings > Privacy & Security > Screen Recording, then rerun.");
        return 2;
    }
    if perms.state(Permission::Accessibility) != PermState::Granted {
        perms.request(Permission::Accessibility);
        eprintln!("Accessibility permission needed (for input events + element names). Grant it to your terminal (or AgentSnap.app) in System Settings > Privacy & Security > Accessibility, then rerun.");
        return 2;
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        if let Err(e) = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst)) {
            log::warn!("ctrl-c handler: {e}");
        }
    }

    let mut rec = Recorder::new(platform::backend(), &out_dir, options.clone());
    if let Err(e) = rec.start() {
        eprintln!("start failed: {e:#}");
        return 1;
    }
    eprintln!("recording to {} (Ctrl-C to stop)", out_dir.display());
    let started = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if let Some(d) = duration {
            if started.elapsed().as_secs_f64() >= d {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let session = match rec.stop() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stop failed: {e:#}");
            return 1;
        }
    };
    eprintln!("stopped. {} steps, {} window segments.", session.steps.len(), session.timeline.len());
    if no_build {
        println!("{}", out_dir.display());
        return 0;
    }
    match build(&out_dir) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("build failed: {e:#}");
            1
        }
    }
}
