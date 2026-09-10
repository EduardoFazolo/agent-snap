//! Pipes raw NV12 frames into an ffmpeg subprocess that writes a fragmented (crash-safe) `recording.mp4`.
//!
//! Input is `-f rawvideo -pix_fmt nv12 -framerate 30`, so frame `i` lands at `i/30` s. The recorder
//! feeds exactly one frame per tick (re-sending the last frame when nothing changed).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context, Result};

use crate::ffmpeg::{available_encoders, ffmpeg_path};
use crate::platform::Frame;

pub const FPS: u32 = 30;

/// Preferred encoders for this OS, best first. `libx264` is always the last resort.
pub fn encoder_candidates() -> Vec<&'static str> {
    let mut v: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["hevc_videotoolbox"]
    } else if cfg!(target_os = "windows") {
        vec!["hevc_nvenc", "hevc_amf", "hevc_qsv"]
    } else {
        vec![]
    };
    v.push("libx264");
    v
}

fn encoder_args(enc: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["-c:v".into(), enc.into()];
    match enc {
        "libx264" => a.extend(["-preset", "ultrafast", "-crf", "23", "-pix_fmt", "yuv420p"].map(String::from)),
        "hevc_videotoolbox" => a.extend(["-b:v", "24M", "-realtime", "1", "-tag:v", "hvc1"].map(String::from)),
        "hevc_nvenc" => a.extend(["-preset", "p1", "-tune", "ll", "-b:v", "24M", "-tag:v", "hvc1"].map(String::from)),
        _ => a.extend(["-b:v", "24M", "-tag:v", "hvc1"].map(String::from)),
    }
    // One keyframe per second, no frame reordering (matches the Swift writer).
    a.extend(["-g", "30", "-bf", "0"].map(String::from));
    a
}

pub struct VideoWriter {
    path: PathBuf,
    width: u32,
    height: u32,
    child: Child,
    stdin: Option<ChildStdin>,
    encoder: String,
    fallbacks: Vec<&'static str>,
    frames: u64,
    packed: Vec<u8>,
    last: Vec<u8>,
    failed: bool,
}

impl VideoWriter {
    /// Starts ffmpeg writing to `path`. Picks the first available encoder for this OS.
    pub fn new(path: &Path, width: u32, height: u32) -> Result<Self> {
        let _ = std::fs::remove_file(path);
        let avail = available_encoders().cloned().unwrap_or_default();
        let mut cands: Vec<&'static str> = encoder_candidates().into_iter().filter(|e| avail.iter().any(|a| a == e)).collect();
        if cands.is_empty() {
            cands.push("libx264");
        }
        let encoder = cands.remove(0);
        let (child, stdin) = spawn(path, width, height, encoder)?;
        log::info!("video encoder: {encoder} ({}x{} @{FPS}fps) -> {}", width, height, path.display());
        Ok(VideoWriter {
            path: path.to_path_buf(),
            width,
            height,
            child,
            stdin: Some(stdin),
            encoder: encoder.to_string(),
            fallbacks: cands,
            frames: 0,
            packed: Vec::new(),
            last: Vec::new(),
            failed: false,
        })
    }

    pub fn encoder(&self) -> &str {
        &self.encoder
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Appends one frame (one video tick). Returns false when the frame was dropped.
    pub fn write(&mut self, f: &Frame) -> bool {
        if self.failed {
            return false;
        }
        if f.width != self.width || f.height != self.height {
            log::warn!("frame size {}x{} does not match writer {}x{}; dropped", f.width, f.height, self.width, self.height);
            return false;
        }
        pack_nv12(f, &mut self.packed);
        std::mem::swap(&mut self.packed, &mut self.last);
        self.write_last()
    }

    /// Re-appends the previous frame (nothing changed on screen). Returns false if none yet.
    pub fn write_duplicate(&mut self) -> bool {
        if self.failed || self.last.is_empty() {
            return false;
        }
        self.write_last()
    }

    fn write_last(&mut self) -> bool {
        loop {
            let ok = match self.stdin.as_mut() {
                Some(s) => s.write_all(&self.last).is_ok(),
                None => false,
            };
            if ok {
                self.frames += 1;
                return true;
            }
            // ffmpeg died (typically: hardware encoder failed to initialise on the first frame).
            let err = self.drain_stderr();
            log::warn!("encoder {} failed after {} frames: {}", self.encoder, self.frames, err.trim());
            if self.fallbacks.is_empty() {
                self.failed = true;
                return false;
            }
            let next = self.fallbacks.remove(0);
            match spawn(&self.path, self.width, self.height, next) {
                Ok((child, stdin)) => {
                    self.child = child;
                    self.stdin = Some(stdin);
                    self.encoder = next.to_string();
                    log::info!("video encoder: falling back to {next}");
                    // Keep the timeline: refill the frames the dead process swallowed with the current frame.
                    let missing = self.frames;
                    self.frames = 0;
                    for _ in 0..missing {
                        if let Some(s) = self.stdin.as_mut() {
                            if s.write_all(&self.last).is_ok() {
                                self.frames += 1;
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("cannot start encoder {next}: {e:#}");
                    self.failed = true;
                    return false;
                }
            }
        }
    }

    fn drain_stderr(&mut self) -> String {
        self.stdin = None;
        let mut s = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut s);
        }
        let _ = self.child.wait();
        s
    }

    /// Closes the input and waits for ffmpeg to finalize the file.
    pub fn finish(mut self) -> Result<()> {
        self.stdin = None;
        let status = self.child.wait().context("waiting for ffmpeg")?;
        let mut err = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        log::info!("video finished: encoder={} frames={} status={status}", self.encoder, self.frames);
        if !status.success() && !self.failed {
            anyhow::bail!("ffmpeg exited with {status}: {}", err.trim());
        }
        Ok(())
    }
}

fn spawn(path: &Path, width: u32, height: u32, enc: &str) -> Result<(Child, ChildStdin)> {
    let mut cmd = Command::new(ffmpeg_path()?);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "nv12"])
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .args(["-framerate", &FPS.to_string(), "-i", "pipe:0", "-an"])
        .args(encoder_args(enc))
        .args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ffmpeg")?;
    let stdin = child.stdin.take().context("ffmpeg stdin")?;
    Ok((child, stdin))
}

/// Tightly packs the Y and UV planes (drops stride padding) into `out`.
pub fn pack_nv12(f: &Frame, out: &mut Vec<u8>) {
    let w = f.width as usize;
    let h = f.height as usize;
    let uv_h = h.div_ceil(2);
    out.clear();
    out.reserve(w * h + w * uv_h);
    for row in 0..h {
        let start = row * f.y_stride;
        out.extend_from_slice(&f.y[start..start + w]);
    }
    for row in 0..uv_h {
        let start = row * f.uv_stride;
        out.extend_from_slice(&f.uv[start..start + w]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn packs_strided_planes() {
        let f = Frame {
            t: Instant::now(),
            width: 4,
            height: 2,
            y_stride: 6,
            uv_stride: 6,
            y: Arc::from(vec![1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0]),
            uv: Arc::from(vec![9, 10, 11, 12, 0, 0]),
            dirty: vec![],
        };
        let mut out = Vec::new();
        pack_nv12(&f, &mut out);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn candidates_end_with_x264() {
        assert_eq!(encoder_candidates().last(), Some(&"libx264"));
    }
}
