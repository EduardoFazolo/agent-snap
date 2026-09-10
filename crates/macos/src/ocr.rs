//! Vision text recognition on an RGBA image.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null;

use agent_snap_core::platform::{Ocr, OcrText, RectI};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_foundation::CFData;
use objc2_core_graphics::{CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo};
use objc2_foundation::{NSArray, NSDictionary};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation, VNRequest,
    VNRequestTextRecognitionLevel,
};

/// Vision-backed OCR.
#[derive(Default)]
pub struct VisionOcr;

impl VisionOcr {
    pub fn new() -> Self {
        Self
    }

    fn recognize_inner(rgba: &image::RgbaImage) -> Vec<OcrText> {
        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let data = CFData::from_bytes(rgba.as_raw());
        let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else { return Vec::new() };
        let Some(space) = CGColorSpace::new_device_rgb() else { return Vec::new() };
        let bitmap = CGBitmapInfo(CGImageAlphaInfo::Last.0);
        let Some(img) = (unsafe {
            CGImage::new(w, h, 8, 32, w * 4, Some(&space), bitmap, Some(&provider), null(), false, CGColorRenderingIntent::RenderingIntentDefault)
        }) else {
            return Vec::new();
        };

        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), &img, &NSDictionary::new())
        };
        let req_ref: &VNRequest = &request;
        let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[req_ref]);
        if let Err(e) = handler.performRequests_error(&requests) {
            log::debug!("vision: {}", e.localizedDescription());
            return Vec::new();
        }
        let Some(results) = request.results() else { return Vec::new() };

        let (wf, hf) = (w as f64, h as f64);
        let mut out = Vec::with_capacity(results.count());
        for obs in results.iter() {
            let Some(t) = obs.downcast_ref::<VNRecognizedTextObservation>() else { continue };
            let cands = t.topCandidates(1);
            let Some(best) = cands.firstObject() else { continue };
            let text = best.string().to_string();
            if text.trim().is_empty() {
                continue;
            }
            // Vision: normalized, origin bottom-left.
            let bb = unsafe { t.boundingBox() };
            let x = bb.origin.x * wf;
            let y = (1.0 - bb.origin.y - bb.size.height) * hf;
            let bounds = RectI {
                x: x.round() as i32,
                y: y.round() as i32,
                w: (bb.size.width * wf).round() as i32,
                h: (bb.size.height * hf).round() as i32,
            };
            out.push(OcrText { text, bounds });
        }
        out
    }
}

impl Ocr for VisionOcr {
    fn recognize(&self, rgba: &image::RgbaImage) -> Vec<OcrText> {
        catch_unwind(AssertUnwindSafe(|| Self::recognize_inner(rgba))).unwrap_or_default()
    }
}
