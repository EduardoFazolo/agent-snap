//! Locates `ffmpeg`/`ffprobe`: on PATH first, else a copy downloaded once into the app data dir.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

/// `<data dir>/agent-snap/ffmpeg`, where an auto-downloaded ffmpeg lives.
pub fn sidecar_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("agent-snap").join("ffmpeg")
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn runs(path: &std::path::Path) -> bool {
    Command::new(path)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn locate(name: &str) -> Option<PathBuf> {
    let on_path = PathBuf::from(exe(name));
    if runs(&on_path) {
        return Some(on_path);
    }
    let local = sidecar_dir().join(exe(name));
    if runs(&local) {
        return Some(local);
    }
    None
}

/// Path to an `ffmpeg` binary: PATH, then the app data dir, else downloaded once into the app data dir.
pub fn ffmpeg_path() -> Result<PathBuf> {
    static CACHE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = guard.as_ref() {
        return Ok(p.clone());
    }
    if let Some(p) = locate("ffmpeg") {
        *guard = Some(p.clone());
        return Ok(p);
    }
    let dir = sidecar_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    log::info!("ffmpeg not found; downloading into {}", dir.display());
    let url = ffmpeg_sidecar::download::ffmpeg_download_url().context("no ffmpeg download for this platform")?;
    let archive = ffmpeg_sidecar::download::download_ffmpeg_package(url, &dir).context("downloading ffmpeg")?;
    ffmpeg_sidecar::download::unpack_ffmpeg(&archive, &dir).context("unpacking ffmpeg")?;
    let p = dir.join(exe("ffmpeg"));
    if !runs(&p) {
        anyhow::bail!("ffmpeg download did not produce a working binary at {}", p.display());
    }
    *guard = Some(p.clone());
    Ok(p)
}

/// Path to `ffprobe` if one is available (PATH or app data dir). Never downloads.
pub fn ffprobe_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(|| locate("ffprobe")).clone()
}

/// Names of encoders listed by `ffmpeg -encoders`, probed once.
pub fn available_encoders() -> Result<&'static Vec<String>> {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    if let Some(v) = CACHE.get() {
        return Ok(v);
    }
    let out = Command::new(ffmpeg_path()?)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running ffmpeg -encoders")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names = Vec::new();
    let mut started = false;
    for line in text.lines() {
        if !started {
            started = line.trim_start().starts_with("------");
            continue;
        }
        // " V....D hevc_videotoolbox    VideoToolbox H.265 Encoder"
        let mut parts = line.split_whitespace();
        if let (Some(flags), Some(name)) = (parts.next(), parts.next()) {
            if flags.starts_with('V') {
                names.push(name.to_string());
            }
        }
    }
    Ok(CACHE.get_or_init(|| names))
}
