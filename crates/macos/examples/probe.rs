//! End-to-end probe of the macOS backend. Usage: probe <seconds> <out_dir>
//! Writes <out_dir>/probe.log (stdout is lost when launched via `open`) and <out_dir>/frame.png.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_snap_core::platform::{Frame, InputTap, Ocr, Permission, Permissions, RawKind, ScreenCapture, WindowTracker};
use agent_snap_macos::{MacCapture, MacInputTap, MacPermissions, MacWindowTracker, VisionOcr};

struct Log(Mutex<File>);
impl Log {
    fn line(&self, s: &str) {
        let mut f = self.0.lock().unwrap();
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
        println!("{s}");
    }
}

fn nv12_to_rgba(f: &Frame) -> image::RgbaImage {
    let (w, h) = (f.width as usize, f.height as usize);
    let mut img = image::RgbaImage::new(f.width, f.height);
    for y in 0..h {
        let yrow = &f.y[y * f.y_stride..y * f.y_stride + w];
        let uvrow = &f.uv[(y / 2) * f.uv_stride..(y / 2) * f.uv_stride + w];
        for x in 0..w {
            let yy = (yrow[x] as f32 - 16.0) * 1.164;
            let u = uvrow[(x / 2) * 2] as f32 - 128.0;
            let v = uvrow[(x / 2) * 2 + 1] as f32 - 128.0;
            let r = (yy + 1.793 * v).clamp(0.0, 255.0) as u8;
            let g = (yy - 0.213 * u - 0.533 * v).clamp(0.0, 255.0) as u8;
            let b = (yy + 2.112 * u).clamp(0.0, 255.0) as u8;
            img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
        }
    }
    img
}

fn kind_name(k: &RawKind) -> String {
    match k {
        RawKind::Move => "Move".into(),
        RawKind::Down(b) => format!("Down({b:?})"),
        RawKind::Up(b) => format!("Up({b:?})"),
        RawKind::Drag(b) => format!("Drag({b:?})"),
        RawKind::Scroll { .. } => "Scroll".into(),
        RawKind::KeyDown { .. } => "KeyDown".into(),
    }
}

fn cursor_position() -> Option<(f64, f64)> {
    let ev = objc2_core_graphics::CGEvent::new(None)?;
    let p = objc2_core_graphics::CGEvent::location(Some(&ev));
    Some((p.x, p.y))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn")).collect();
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let out_dir = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "/tmp/agent-snap-probe".into()));
    let _ = fs::create_dir_all(&out_dir);
    let log = Arc::new(Log(Mutex::new(
        OpenOptions::new().create(true).append(true).open(out_dir.join("probe.log")).expect("log file"),
    )));
    {
        let l = log.clone();
        std::panic::set_hook(Box::new(move |info| l.line(&format!("PANIC: {info}"))));
    }
    log.line(&format!("=== probe start secs={secs} out={} pid={} ===", out_dir.display(), std::process::id()));

    let perms = MacPermissions::new();
    log.line(&format!(
        "permissions: screen={:?} accessibility={:?}",
        perms.state(Permission::ScreenRecording),
        perms.state(Permission::Accessibility)
    ));

    // Capture.
    let frames = Arc::new(Mutex::new((0u64, 0u64, 0u64, 0f64))); // count, with_dirty, dirty_total, luma_sum
    let dumped: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let t0 = Instant::now();
    let mut capture = MacCapture::new();
    let info = {
        let frames = frames.clone();
        let dumped = dumped.clone();
        capture.start(
            30,
            Box::new(move |f: Frame| {
                let mut g = frames.lock().unwrap();
                g.0 += 1;
                if !f.dirty.is_empty() {
                    g.1 += 1;
                    g.2 += f.dirty.len() as u64;
                }
                // cheap luma sample: every 64th byte of Y
                let n = (f.y.len() / 64).max(1);
                let sum: u64 = f.y.iter().step_by(64).map(|&b| b as u64).sum();
                g.3 = sum as f64 / n as f64;
                if t0.elapsed() >= Duration::from_secs(2) {
                    let mut d = dumped.lock().unwrap();
                    if d.is_none() {
                        *d = Some(f);
                    }
                }
            }),
        )
    };
    let info = match info {
        Ok(i) => {
            log.line(&format!("capture: {}x{} scale={}", i.width, i.height, i.scale));
            Some(i)
        }
        Err(e) => {
            log.line(&format!("capture FAILED: {e:#}"));
            None
        }
    };

    // Input.
    let counts: Arc<Mutex<BTreeMap<String, u64>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let samples: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tap = MacInputTap::new();
    {
        let counts = counts.clone();
        let samples = samples.clone();
        match tap.start(Box::new(move |r| {
            *counts.lock().unwrap().entry(kind_name(&r.kind)).or_default() += 1;
            let mut s = samples.lock().unwrap();
            if s.len() < 12 && !matches!(r.kind, RawKind::Move) {
                s.push(format!("{:?} at ({:.0},{:.0}) clicks={}", r.kind, r.x, r.y, r.click_count));
            }
        })) {
            Ok(()) => log.line("input tap: started"),
            Err(e) => log.line(&format!("input tap FAILED: {e:#}")),
        }
    }

    let mut tracker = MacWindowTracker::new();
    let mut png_written = false;
    let mut ax_done = false;
    let deadline = t0 + Duration::from_secs(secs);
    let mut next_tick = t0;
    while Instant::now() < deadline {
        if Instant::now() >= next_tick {
            let ts = Instant::now();
            let w = tracker.front();
            log.line(&format!(
                "[{:.1}s] front: app={:?} bundle={:?} title={:?} bounds={:?} ({}ms)",
                t0.elapsed().as_secs_f64(),
                w.app,
                w.bundle_id,
                w.title,
                w.bounds,
                ts.elapsed().as_millis()
            ));
            if !ax_done && t0.elapsed() >= Duration::from_secs(3) {
                ax_done = true;
                let ts = Instant::now();
                let url = tracker.tab_url(&w);
                log.line(&format!("tab_url: {url:?} ({}ms)", ts.elapsed().as_millis()));
                if let Some((x, y)) = cursor_position() {
                    let ts = Instant::now();
                    let h = tracker.hit_test(x, y);
                    log.line(&format!("hit_test at ({x:.0},{y:.0}): {h:?} ({}ms)", ts.elapsed().as_millis()));
                }
                let ts = Instant::now();
                let f = tracker.focused();
                log.line(&format!("focused: {f:?} secure={} ({}ms)", tracker.focused_is_secure(), ts.elapsed().as_millis()));
            }
            next_tick += Duration::from_secs(1);
        }
        if !png_written {
            let f = dumped.lock().unwrap().clone();
            if let Some(f) = f {
                png_written = true;
                let ts = Instant::now();
                let img = nv12_to_rgba(&f);
                let path = out_dir.join("frame.png");
                match img.save(&path) {
                    Ok(()) => log.line(&format!(
                        "dumped {} ({}x{}, dirty={:?}) in {}ms",
                        path.display(),
                        f.width,
                        f.height,
                        &f.dirty[..f.dirty.len().min(4)],
                        ts.elapsed().as_millis()
                    )),
                    Err(e) => log.line(&format!("png save failed: {e}")),
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let elapsed = t0.elapsed().as_secs_f64();
    capture.stop();
    tap.stop();

    let (count, with_dirty, dirty_total, luma) = *frames.lock().unwrap();
    log.line(&format!(
        "frames: {count} in {elapsed:.1}s = {:.1} fps; frames with dirty rects: {with_dirty}; avg dirty/frame: {:.2}; last mean luma: {luma:.1} ({})",
        count as f64 / elapsed,
        if count > 0 { dirty_total as f64 / count as f64 } else { 0.0 },
        if luma > 8.0 { "non-black" } else { "BLACK?" }
    ));
    log.line(&format!("input counts: {:?}", counts.lock().unwrap()));
    for s in samples.lock().unwrap().iter() {
        log.line(&format!("  input sample: {s}"));
    }

    if let Some(f) = dumped.lock().unwrap().as_ref() {
        let img = nv12_to_rgba(f);
        let ts = Instant::now();
        let ocr = VisionOcr::new();
        let texts = ocr.recognize(&img);
        log.line(&format!("ocr: {} strings in {}ms", texts.len(), ts.elapsed().as_millis()));
        for t in texts.iter().take(10) {
            log.line(&format!("  ocr: {:?} @ {:?}", t.text, t.bounds));
        }
    } else {
        log.line("ocr: skipped (no frame)");
    }
    let _ = info;
    log.line("=== probe end ===");
}
