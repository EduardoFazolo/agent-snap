//! Picks the platform backend at compile time.

use std::sync::Arc;

use agent_snap_core::platform::{Backend, Ocr, Permissions};

#[cfg(target_os = "macos")]
pub fn backend() -> Backend {
    agent_snap_macos::backend()
}

#[cfg(target_os = "macos")]
pub fn permissions() -> Box<dyn Permissions> {
    Box::new(agent_snap_macos::MacPermissions::new())
}

#[cfg(target_os = "macos")]
pub fn ocr() -> Arc<dyn Ocr> {
    Arc::new(agent_snap_macos::VisionOcr::new())
}

#[cfg(target_os = "windows")]
pub fn backend() -> Backend {
    agent_snap_windows::backend()
}

#[cfg(target_os = "windows")]
pub fn permissions() -> Box<dyn Permissions> {
    Box::new(agent_snap_windows::WinPermissions)
}

#[cfg(target_os = "windows")]
pub fn ocr() -> Arc<dyn Ocr> {
    Arc::new(agent_snap_windows::WinOcr::new())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
compile_error!("agent-snap needs a platform backend (macOS or Windows)");

/// Identifier the recorder ignores so our own tray UI never becomes a step.
pub const BUNDLE_ID: &str = "com.fazolo.agent-snap";

/// `/Users/me/agent-snap/x` -> `~/agent-snap/x` (keeps the popover narrow).
pub fn abbreviate(path: &str) -> String {
    match dirs::home_dir() {
        Some(h) => {
            let h = h.to_string_lossy();
            match path.strip_prefix(h.as_ref()) {
                Some(rest) => format!("~{rest}"),
                None => path.to_string(),
            }
        }
        None => path.to_string(),
    }
}

/// Opens a file or folder with the OS handler.
pub fn open_path(path: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", path]);
        c
    };
    let status = cmd.status()?;
    anyhow::ensure!(status.success(), "open failed: {status}");
    Ok(())
}

/// Shows a file selected in Finder / Explorer.
pub fn reveal_path(path: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.args(["-R", path]);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(format!("/select,{path}"));
        c
    };
    let _ = cmd.status()?;
    Ok(())
}
