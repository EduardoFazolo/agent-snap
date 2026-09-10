//! OCR through `Windows.Media.Ocr`, returning one entry per recognized line (words joined with a
//! space, bounds = union of the word rectangles), which mirrors what the macOS Vision backend
//! produces.

use agent_snap_core::platform::{Ocr, OcrText, RectI};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use image::RgbaImage;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::core::RuntimeType;
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation};

/// Upper bound on a single recognition; the engine normally answers in well under a second.
const OCR_TIMEOUT: Duration = Duration::from_secs(10);

pub struct WinOcr;

impl WinOcr {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WinOcr {
    fn default() -> Self {
        Self::new()
    }
}

impl Ocr for WinOcr {
    fn recognize(&self, rgba: &RgbaImage) -> Vec<OcrText> {
        match std::panic::catch_unwind(|| recognize(rgba)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::debug!("ocr failed: {e:#}");
                Vec::new()
            }
            Err(_) => {
                log::warn!("ocr panicked");
                Vec::new()
            }
        }
    }
}

fn recognize(rgba: &RgbaImage) -> Result<Vec<OcrText>> {
    if rgba.width() == 0 || rgba.height() == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: plain call; failure only means the thread is already initialized in another mode.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };

    let engine = match OcrEngine::TryCreateFromUserProfileLanguages() {
        Ok(e) => e,
        Err(e) => {
            // No OCR language pack installed: report nothing rather than fail.
            log::debug!("no OCR engine for the user's languages: {e}");
            return Ok(Vec::new());
        }
    };

    // The engine refuses images larger than MaxImageDimension on either side; downscale and map back.
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(2600).max(1);
    let longest = rgba.width().max(rgba.height());
    let (img, factor) = if longest > max_dim {
        let f = max_dim as f64 / longest as f64;
        let w = ((rgba.width() as f64 * f).floor() as u32).max(1);
        let h = ((rgba.height() as f64 * f).floor() as u32).max(1);
        (
            std::borrow::Cow::Owned(image::imageops::resize(rgba, w, h, image::imageops::FilterType::Triangle)),
            f,
        )
    } else {
        (std::borrow::Cow::Borrowed(rgba), 1.0)
    };

    let bgra = to_bgra_premultiplied(&img);
    let buffer = CryptographicBuffer::CreateFromByteArray(&bgra).context("CreateFromByteArray")?;
    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        img.width() as i32,
        img.height() as i32,
        BitmapAlphaMode::Premultiplied,
    )
    .context("SoftwareBitmap::CreateCopyWithAlphaFromBuffer")?;

    let result = block_on(engine.RecognizeAsync(&bitmap).context("RecognizeAsync")?, OCR_TIMEOUT).context("OCR result")?;
    let lines = result.Lines().context("Lines")?;
    let mut out = Vec::new();
    for line in &lines {
        let words = line.Words().context("Words")?;
        let mut text = String::new();
        let mut bounds: Option<(f64, f64, f64, f64)> = None; // x0, y0, x1, y1 in image pixels
        for word in &words {
            let w = word.Text().map(|t| t.to_string_lossy()).unwrap_or_default();
            if w.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&w);
            if let Ok(r) = word.BoundingRect() {
                let (x0, y0) = (r.X as f64 / factor, r.Y as f64 / factor);
                let (x1, y1) = (x0 + r.Width as f64 / factor, y0 + r.Height as f64 / factor);
                bounds = Some(match bounds {
                    Some(b) => (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1)),
                    None => (x0, y0, x1, y1),
                });
            }
        }
        if text.is_empty() {
            continue;
        }
        let (x0, y0, x1, y1) = bounds.ok_or_else(|| anyhow!("line without bounds"))?;
        out.push(OcrText {
            text,
            bounds: RectI {
                x: x0.floor() as i32,
                y: y0.floor() as i32,
                w: (x1 - x0).ceil().max(1.0) as i32,
                h: (y1 - y0).ceil().max(1.0) as i32,
            },
        });
    }
    Ok(out)
}

/// Block until a WinRT async operation completes (or `timeout` elapses) and return its result.
fn block_on<T: RuntimeType + 'static>(op: IAsyncOperation<T>, timeout: Duration) -> Result<T> {
    if op.Status().context("Status")? == AsyncStatus::Started {
        let (tx, rx) = mpsc::channel::<()>();
        op.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            let _ = tx.send(());
            Ok(())
        }))
        .context("SetCompleted")?;
        if rx.recv_timeout(timeout).is_err() {
            let _ = op.Cancel();
            return Err(anyhow!("async operation timed out after {timeout:?}"));
        }
    }
    op.GetResults().context("GetResults")
}

fn to_bgra_premultiplied(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.as_raw().len());
    for px in img.as_raw().chunks_exact(4) {
        let a = px[3] as u32;
        let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        out.extend_from_slice(&[pm(px[2]), pm(px[1]), pm(px[0]), px[3]]);
    }
    out
}
