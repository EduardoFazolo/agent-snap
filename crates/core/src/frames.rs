//! Exact-time frame access into the recording via ffmpeg.
//!
//! Frame timestamps are indexed once with ffprobe (when available) so "the frame visible at t"
//! is the last frame whose pts <= t, for both the sparse Swift `.mov` files and our constant-rate `.mp4`.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};
use image::RgbaImage;
use lru::LruCache;

use crate::encoder::FPS;
use crate::ffmpeg::{ffmpeg_path, ffprobe_path};

pub struct FrameSource {
    video: PathBuf,
    offset: f64,
    /// Seconds; 0 when the file could not be read.
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    /// Sorted packet timestamps (seconds). Empty = assume constant `FPS`.
    pts: Vec<f64>,
    cache: LruCache<i64, Arc<RgbaImage>>,
}

impl FrameSource {
    /// `offset`: session time of the first video frame (`videoTime = t - offset`).
    pub fn open(video: &Path, offset: f64) -> Result<Self> {
        let mut src = FrameSource {
            video: video.to_path_buf(),
            offset,
            duration: 0.0,
            width: 0,
            height: 0,
            pts: Vec::new(),
            cache: LruCache::new(NonZeroUsize::new(64).unwrap()),
        };
        src.probe()?;
        Ok(src)
    }

    fn probe(&mut self) -> Result<()> {
        if let Some(ffprobe) = ffprobe_path() {
            let out = Command::new(&ffprobe)
                .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,duration:format=duration", "-of", "default=nw=1"])
                .arg(&self.video)
                .stdin(Stdio::null())
                .output()
                .context("running ffprobe")?;
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(v) = line.strip_prefix("width=") {
                    self.width = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("height=") {
                    self.height = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("duration=") {
                    if let Ok(d) = v.trim().parse::<f64>() {
                        self.duration = self.duration.max(d);
                    }
                }
            }
            let out = Command::new(&ffprobe)
                .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
                .arg(&self.video)
                .stdin(Stdio::null())
                .output()
                .context("running ffprobe")?;
            let mut pts: Vec<f64> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
                .collect();
            pts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if let Some(last) = pts.last() {
                self.duration = self.duration.max(last + 1.0 / FPS as f64);
            }
            self.pts = pts;
        }
        if self.width == 0 || self.duration <= 0.0 {
            // Fallback: parse `ffmpeg -i` banner ("Duration: 00:01:33.87", "3024x1964").
            let out = Command::new(ffmpeg_path()?).args(["-hide_banner", "-i"]).arg(&self.video).stdin(Stdio::null()).output().context("running ffmpeg -i")?;
            let text = String::from_utf8_lossy(&out.stderr);
            for line in text.lines() {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix("Duration: ") {
                    let d = rest.split(',').next().unwrap_or("").trim();
                    if let Some(secs) = parse_hms(d) {
                        self.duration = self.duration.max(secs);
                    }
                }
                if l.contains("Video:") && self.width == 0 {
                    for tok in l.split([' ', ',']) {
                        if let Some((w, h)) = tok.split_once('x') {
                            if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                                if w > 0 && h > 0 {
                                    self.width = w;
                                    self.height = h;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !self.duration.is_finite() {
            self.duration = 0.0;
        }
        Ok(())
    }

    /// Video timestamp of the frame displayed at video time `vt`.
    fn frame_pts(&self, vt: f64) -> f64 {
        if self.pts.is_empty() {
            return ((vt * FPS as f64) + 1e-6).floor() / FPS as f64;
        }
        let idx = self.pts.partition_point(|&p| p <= vt + 1e-4);
        if idx == 0 {
            self.pts[0]
        } else {
            self.pts[idx - 1]
        }
    }

    /// Frame visible at session time `t` (seconds since t0).
    pub fn frame(&mut self, t: f64) -> Option<Arc<RgbaImage>> {
        if self.duration <= 0.0 || !t.is_finite() || self.width == 0 {
            return None;
        }
        let vt = (t - self.offset).clamp(0.0, (self.duration - 0.001).max(0.0));
        let key = (vt * 1000.0).round() as i64;
        if let Some(img) = self.cache.get(&key) {
            return Some(img.clone());
        }
        let target = self.frame_pts(vt);
        // Exact frame first, then progressively earlier ones (mirrors the Swift tolerance ladder).
        for back in [0.0, 0.5, 3.0, 60.0] {
            let ss = (target - back - 0.0005).max(0.0);
            if let Some(img) = self.extract(ss) {
                let img = Arc::new(img);
                self.cache.put(key, img.clone());
                return Some(img);
            }
            if ss == 0.0 {
                break;
            }
        }
        None
    }

    fn extract(&self, ss: f64) -> Option<RgbaImage> {
        let out = Command::new(ffmpeg_path().ok()?)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .arg("-ss")
            .arg(format!("{ss:.6}"))
            .arg("-i")
            .arg(&self.video)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let need = self.width as usize * self.height as usize * 4;
        if out.stdout.len() < need {
            return None;
        }
        let mut data = out.stdout;
        data.truncate(need);
        RgbaImage::from_raw(self.width, self.height, data)
    }
}

fn parse_hms(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms() {
        assert_eq!(parse_hms("00:01:33.87"), Some(93.87));
    }

    #[test]
    fn pts_lookup() {
        let mut s = FrameSource {
            video: PathBuf::new(),
            offset: 0.0,
            duration: 1.0,
            width: 1,
            height: 1,
            pts: vec![0.0, 0.0333, 0.0667, 0.1],
            cache: LruCache::new(NonZeroUsize::new(2).unwrap()),
        };
        assert_eq!(s.frame_pts(0.05), 0.0333);
        assert_eq!(s.frame_pts(0.0), 0.0);
        assert_eq!(s.frame_pts(5.0), 0.1);
        s.pts.clear();
        assert!((s.frame_pts(0.05) - 1.0 / 30.0).abs() < 1e-9);
        assert!((s.frame_pts(0.1) - 3.0 / 30.0).abs() < 1e-9);
    }
}
