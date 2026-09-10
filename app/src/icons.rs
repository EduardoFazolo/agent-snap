//! Tray icons drawn in code (no asset files): a camera viewfinder when idle, a red dot while recording.

use tauri::image::Image;

const S: u32 = 44; // 22pt @2x

/// Coverage of a pixel by a shape, sampled 3x3.
fn coverage(x: u32, y: u32, inside: &dyn Fn(f64, f64) -> bool) -> f64 {
    let mut hits = 0;
    for sy in 0..3 {
        for sx in 0..3 {
            let px = x as f64 + (sx as f64 + 0.5) / 3.0;
            let py = y as f64 + (sy as f64 + 0.5) / 3.0;
            if inside(px, py) {
                hits += 1;
            }
        }
    }
    hits as f64 / 9.0
}

fn rgba(size: u32, color: [u8; 3], inside: impl Fn(f64, f64) -> bool) -> Image<'static> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let a = coverage(x, y, &inside);
            if a > 0.0 {
                let i = ((y * size + x) * 4) as usize;
                buf[i] = color[0];
                buf[i + 1] = color[1];
                buf[i + 2] = color[2];
                buf[i + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    Image::new_owned(buf, size, size)
}

/// Rounded-rect outline with a filled lens: reads as a camera at 22pt.
pub fn camera() -> Image<'static> {
    let n = S as f64;
    let (inset, radius, stroke) = (4.0, 9.0, 3.4);
    let cx = n / 2.0;
    rgba(S, [0, 0, 0], move |x, y| {
        // distance to the rounded rect border
        let (lo, hi) = (inset, n - inset);
        let qx = (lo + radius - x).max(x - (hi - radius)).max(0.0);
        let qy = (lo + radius - y).max(y - (hi - radius)).max(0.0);
        let d = (qx * qx + qy * qy).sqrt() - radius;
        let bx = (lo - x).max(x - hi);
        let by = (lo - y).max(y - hi);
        let outside = bx.max(by);
        let border = if qx > 0.0 || qy > 0.0 { d } else { outside };
        let on_border = border.abs() <= stroke / 2.0 && outside <= 0.0;
        let lens = ((x - cx).powi(2) + (y - cx).powi(2)).sqrt() <= 5.6;
        on_border || lens
    })
}

/// Solid red dot: unmistakable while recording.
pub fn record() -> Image<'static> {
    let c = S as f64 / 2.0;
    rgba(S, [255, 59, 48], move |x, y| ((x - c).powi(2) + (y - c).powi(2)).sqrt() <= 13.0)
}
