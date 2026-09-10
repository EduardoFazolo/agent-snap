//! Hardware probe for the Windows backend. Run on a Windows box:
//!
//! ```text
//! cargo run -p agent-snap-windows --example probe -- [seconds] [out.png]
//! ```
//!
//! It captures the primary monitor for N seconds (default 5), counts delivered frames, dumps
//! the last frame as PNG, prints every input event seen meanwhile, then prints `front()`,
//! `hit_test` at the cursor, `focused()`, `tab_url()` and the OCR of the dumped frame.

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use agent_snap_core::platform::Frame;

    env_logger::init();
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = args.next().unwrap_or_else(|| "probe-frame.png".to_string());

    let mut backend = agent_snap_windows::backend();

    let frames = Arc::new(Mutex::new((0u64, None::<Frame>)));
    let sink = frames.clone();
    let info = backend.capture.start(
        30,
        Box::new(move |f| {
            let mut g = sink.lock().unwrap();
            g.0 += 1;
            g.1 = Some(f);
        }),
    )?;
    println!("capture: {}x{} px, scale {}", info.width, info.height, info.scale);

    backend.input.start(Box::new(|ev| println!("input: {:?} at ({:.1},{:.1}) x{}", ev.kind, ev.x, ev.y, ev.click_count)))?;

    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    backend.capture.stop();
    backend.input.stop();

    let (count, last) = {
        let g = frames.lock().unwrap();
        (g.0, g.1.clone())
    };
    println!("frames: {} in {:.1}s = {:.1} fps", count, t0.elapsed().as_secs_f64(), count as f64 / t0.elapsed().as_secs_f64());

    let mut ocr_input = None;
    if let Some(f) = last {
        let rgba = nv12_to_rgba(&f);
        rgba.save(&out)?;
        println!("wrote {out}");
        ocr_input = Some(rgba);
    }

    let front = backend.windows.front();
    println!("front: {front:?}");
    println!("tab_url: {:?}", backend.windows.tab_url(&front));
    let mut pt = windows::Win32::Foundation::POINT::default();
    // SAFETY: `pt` is a live out-pointer.
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt)? };
    let (x, y) = (pt.x as f64 / info.scale, pt.y as f64 / info.scale);
    let t = Instant::now();
    let hit = backend.windows.hit_test(x, y);
    println!("hit_test({x:.0},{y:.0}) in {:?}: {hit:?}", t.elapsed());
    println!("focused: {:?} secure={}", backend.windows.focused(), backend.windows.focused_is_secure());

    if let Some(img) = ocr_input {
        let t = Instant::now();
        let words = backend.ocr.recognize(&img);
        println!("ocr: {} lines in {:?}", words.len(), t.elapsed());
        for w in words.iter().take(20) {
            println!("  {:?} {:?}", w.text, w.bounds);
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn nv12_to_rgba(f: &agent_snap_core::platform::Frame) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(f.width, f.height);
    for y in 0..f.height as usize {
        for x in 0..f.width as usize {
            let yy = f.y[y * f.y_stride + x] as f64;
            let uv = (y / 2) * f.uv_stride + (x / 2) * 2;
            let u = f.uv[uv] as f64 - 128.0;
            let v = f.uv[uv + 1] as f64 - 128.0;
            let c = 1.164 * (yy - 16.0);
            let r = (c + 1.596 * v).clamp(0.0, 255.0) as u8;
            let g = (c - 0.392 * u - 0.813 * v).clamp(0.0, 255.0) as u8;
            let b = (c + 2.017 * u).clamp(0.0, 255.0) as u8;
            img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
        }
    }
    img
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("the probe only runs on Windows");
}
