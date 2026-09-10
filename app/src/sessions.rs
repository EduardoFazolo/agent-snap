//! Past recordings under `<output_dir>/sessions`, newest first (mirrors the Swift `refreshSessions`).

use std::path::{Path, PathBuf};

use agent_snap_core::{image_tokens, parse_flow_header};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub dir: String,
    pub name: String,
    pub flow: String,
    pub flow_short: String,
    pub steps: Option<usize>,
    pub tokens: Option<usize>,
    pub duration: Option<String>,
}

pub fn list(output_dir: &str) -> Vec<SessionInfo> {
    let root = Path::new(output_dir).join("sessions");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    let mut out = Vec::new();
    for d in dirs {
        let flow = d.join("flow.md");
        if !flow.is_file() {
            continue;
        }
        let flow_s = flow.to_string_lossy().into_owned();
        let mut info = SessionInfo {
            dir: d.to_string_lossy().into_owned(),
            name: d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            flow_short: crate::platform::abbreviate(&flow_s),
            flow: flow_s,
            steps: None,
            tokens: None,
            duration: None,
        };
        if let Ok(text) = std::fs::read_to_string(&flow) {
            let header = parse_flow_header(&text).unwrap_or_default();
            info.steps = header.steps;
            info.duration = header.duration;
            info.tokens = Some(header.tokens.unwrap_or_else(|| estimate_tokens(&text, &d)));
        }
        out.push(info);
    }
    out
}

/// Older session without a cost line: text/4 plus every composite's image cost.
fn estimate_tokens(text: &str, dir: &Path) -> usize {
    let mut tokens = text.chars().count() / 4;
    if let Ok(rd) = std::fs::read_dir(dir.join("composites")) {
        for f in rd.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "png") {
                if let Some((w, h)) = png_dimensions(&p) {
                    tokens += image_tokens(w, h);
                }
            }
        }
    }
    tokens
}

/// Width/height from the PNG IHDR chunk (no decoding).
fn png_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut head = [0u8; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(head[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(head[20..24].try_into().ok()?);
    Some((w, h))
}
