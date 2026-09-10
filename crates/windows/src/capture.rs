//! Screen capture through Windows.Graphics.Capture (WGC) on the primary monitor.
//!
//! Frames arrive on a free-threaded frame pool; each one is copied into a CPU-readable staging
//! texture, converted from BGRA to NV12 (BT.601, limited range, like the macOS backend's
//! `420YpCbCr8BiPlanarVideoRange`) and handed to the recorder. WGC gives no dirty regions, so
//! `Frame::dirty` is always empty (the core treats that as "whole frame").

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_snap_core::platform::{CaptureInfo, Frame, ScreenCapture};
use anyhow::{anyhow, Context, Result};
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::dpi;

const BUFFER_COUNT: i32 = 3;

/// WGC-based capture of the primary monitor.
pub struct WgcCapture {
    running: Option<Running>,
}

struct Running {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    token: i64,
    alive: Arc<AtomicBool>,
}

impl WgcCapture {
    pub fn new() -> Self {
        Self { running: None }
    }
}

impl Default for WgcCapture {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the FrameArrived handler needs. Serialized by a mutex because the free-threaded
/// pool may raise the event from any thread-pool thread and the D3D11 immediate context is not
/// thread safe.
struct HandlerState {
    d3d: D3d,
    pool_size: SizeInt32,
    min_interval: Duration,
    last_emit: Option<Instant>,
    y_buf: Vec<u8>,
    uv_buf: Vec<u8>,
    on_frame: Box<dyn FnMut(Frame) + Send>,
}

/// The Direct3D objects used by the frame handler.
///
/// `ID3D11Device` is free-threaded; the immediate context and the staging texture are not, but
/// they are only ever touched under the `HandlerState` mutex, from whichever thread-pool thread
/// raises FrameArrived. That serialization is what makes moving them across threads sound.
struct D3d {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

// SAFETY: see the type-level comment; all access is serialized by the mutex around HandlerState.
unsafe impl Send for D3d {}

impl ScreenCapture for WgcCapture {
    fn start(&mut self, max_fps: u32, on_frame: Box<dyn FnMut(Frame) + Send>) -> Result<CaptureInfo> {
        self.stop();
        dpi::ensure_dpi_aware();
        // SAFETY: plain call; S_FALSE / RPC_E_CHANGED_MODE just mean the thread is already initialized.
        if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            log::debug!("RoInitialize: {e} (ignored)");
        }

        let (device, context) = create_d3d_device()?;
        let dxgi: IDXGIDevice = device.cast().context("ID3D11Device -> IDXGIDevice")?;
        // SAFETY: `dxgi` is a valid DXGI device.
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .context("CreateDirect3D11DeviceFromDXGIDevice")?
            .cast()
            .context("IInspectable -> IDirect3DDevice")?;

        let monitor = dpi::primary_monitor();
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("GraphicsCaptureItem factory")?;
        // SAFETY: `monitor` is a valid HMONITOR from MonitorFromPoint.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")?;
        let size = item.Size().context("GraphicsCaptureItem.Size")?;
        if size.Width <= 0 || size.Height <= 0 {
            return Err(anyhow!("capture item has empty size {}x{}", size.Width, size.Height));
        }

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFER_COUNT,
            size,
        )
        .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;
        let session = pool.CreateCaptureSession(&item).context("CreateCaptureSession")?;
        if let Err(e) = session.SetIsCursorCaptureEnabled(true) {
            log::warn!("IsCursorCaptureEnabled unsupported: {e}");
        }
        if let Err(e) = session.SetIsBorderRequired(false) {
            // Windows 10 lacks IGraphicsCaptureSession3; the yellow border stays on there.
            log::debug!("IsBorderRequired unsupported: {e}");
        }

        let alive = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(HandlerState {
            d3d: D3d { device, context, winrt_device, staging: None },
            pool_size: size,
            min_interval: if max_fps == 0 { Duration::ZERO } else { Duration::from_secs_f64(1.0 / max_fps as f64) },
            last_emit: None,
            y_buf: Vec::new(),
            uv_buf: Vec::new(),
            on_frame,
        }));

        let handler = {
            let alive = alive.clone();
            let state = state.clone();
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |sender, _| {
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                let Some(pool) = sender.as_ref() else { return Ok(()) };
                let Ok(mut st) = state.lock() else { return Ok(()) };
                if let Err(e) = on_frame_arrived(pool, &mut st) {
                    log::debug!("frame dropped: {e:#}");
                }
                Ok(())
            })
        };
        let token = pool.FrameArrived(&handler).context("FrameArrived")?;
        session.StartCapture().context("StartCapture")?;

        self.running = Some(Running { session, pool, token, alive });
        Ok(CaptureInfo { width: size.Width as u32, height: size.Height as u32, scale: dpi::primary_scale() })
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.alive.store(false, Ordering::Release);
            let _ = r.pool.RemoveFrameArrived(r.token);
            let _ = r.session.Close();
            let _ = r.pool.Close();
        }
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        // SAFETY: out-pointers reference live locals; no adapter, no software module.
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match (r, device, context) {
            (Ok(()), Some(d), Some(c)) => return Ok((d, c)),
            (Err(e), _, _) => log::warn!("D3D11CreateDevice({driver:?}) failed: {e}"),
            _ => log::warn!("D3D11CreateDevice({driver:?}) returned no device"),
        }
    }
    Err(anyhow!("no Direct3D 11 device available"))
}

fn on_frame_arrived(pool: &Direct3D11CaptureFramePool, st: &mut HandlerState) -> Result<()> {
    let t = Instant::now();
    // Always take the frame so the pool buffer is released, even when we throttle it away.
    let frame = pool.TryGetNextFrame().context("TryGetNextFrame")?;
    let result = process_frame(pool, st, &frame, t);
    let _ = frame.Close();
    result
}

fn process_frame(
    pool: &Direct3D11CaptureFramePool,
    st: &mut HandlerState,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    t: Instant,
) -> Result<()> {
    let content = frame.ContentSize().context("ContentSize")?;
    if content.Width != st.pool_size.Width || content.Height != st.pool_size.Height {
        // Resolution changed (display mode switch): resize the pool; this frame still has the old size.
        log::info!("capture size changed to {}x{}", content.Width, content.Height);
        pool.Recreate(&st.d3d.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, BUFFER_COUNT, content)
            .context("FramePool.Recreate")?;
        st.pool_size = content;
    }
    if let Some(last) = st.last_emit {
        if t.duration_since(last) < st.min_interval {
            return Ok(());
        }
    }

    let surface = frame.Surface().context("Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().context("IDirect3DDxgiInterfaceAccess")?;
    // SAFETY: the surface wraps a D3D11 texture created on our device.
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.context("GetInterface<ID3D11Texture2D>")?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `desc` is a live out-pointer.
    unsafe { texture.GetDesc(&mut desc) };
    let width = (content.Width.max(1) as u32).min(desc.Width);
    let height = (content.Height.max(1) as u32).min(desc.Height);

    let staging = match &st.d3d.staging {
        Some((tex, w, h)) if *w == desc.Width && *h == desc.Height => tex.clone(),
        _ => {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: valid descriptor, out-pointer references a live local.
            unsafe { st.d3d.device.CreateTexture2D(&staging_desc, None, Some(&mut tex)) }.context("CreateTexture2D(staging)")?;
            let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))?;
            st.d3d.staging = Some((tex.clone(), desc.Width, desc.Height));
            tex
        }
    };

    // SAFETY: both textures belong to `st.device` and have identical descriptors apart from usage.
    unsafe { st.d3d.context.CopyResource(&staging, &texture) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: staging texture was created with CPU read access.
    unsafe { st.d3d.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.context("Map(staging)")?;
    let convert = {
        let pitch = mapped.RowPitch as usize;
        let len = pitch.checked_mul(height as usize).ok_or_else(|| anyhow!("row pitch overflow"))?;
        if mapped.pData.is_null() || pitch < width as usize * 4 {
            Err(anyhow!("unexpected mapped layout (pitch {pitch}, width {width})"))
        } else {
            // SAFETY: the mapped region spans `height` rows of `RowPitch` bytes each and stays valid until Unmap.
            let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
            bgra_to_nv12(src, pitch, width, height, &mut st.y_buf, &mut st.uv_buf);
            Ok(())
        }
    };
    // SAFETY: matches the Map above.
    unsafe { st.d3d.context.Unmap(&staging, 0) };
    convert?;

    let uv_stride = width.div_ceil(2) as usize * 2;
    let frame = Frame {
        t,
        width,
        height,
        y_stride: width as usize,
        uv_stride,
        y: Arc::from(st.y_buf.as_slice()),
        uv: Arc::from(st.uv_buf.as_slice()),
        dirty: Vec::new(),
    };
    st.last_emit = Some(t);
    (st.on_frame)(frame);
    Ok(())
}

/// BGRA (8-bit, row pitch `pitch`) -> NV12, BT.601 limited range (Y in 16..235, Cb/Cr in 16..240).
/// Chroma is the average of each 2x2 block; odd edges reuse the last column/row.
fn bgra_to_nv12(src: &[u8], pitch: usize, width: u32, height: u32, y_out: &mut Vec<u8>, uv_out: &mut Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    y_out.clear();
    y_out.resize(w * h, 16);
    uv_out.clear();
    uv_out.resize(cw * 2 * ch, 128);

    for y in 0..h {
        let row = &src[y * pitch..y * pitch + w * 4];
        let dst = &mut y_out[y * w..(y + 1) * w];
        for (px, d) in row.chunks_exact(4).zip(dst.iter_mut()) {
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *d = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235) as u8;
        }
    }

    for cy in 0..ch {
        let y0 = cy * 2;
        let y1 = (y0 + 1).min(h - 1);
        let row0 = &src[y0 * pitch..y0 * pitch + w * 4];
        let row1 = &src[y1 * pitch..y1 * pitch + w * 4];
        let dst = &mut uv_out[cy * cw * 2..(cy + 1) * cw * 2];
        for cx in 0..cw {
            let x0 = cx * 2 * 4;
            let x1 = ((cx * 2 + 1).min(w - 1)) * 4;
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for (row, x) in [(row0, x0), (row0, x1), (row1, x0), (row1, x1)] {
                b += row[x] as i32;
                g += row[x + 1] as i32;
                r += row[x + 2] as i32;
            }
            // Average of four samples, rounded.
            let (r, g, b) = ((r + 2) >> 2, (g + 2) >> 2, (b + 2) >> 2);
            let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240);
            let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(16, 240);
            dst[cx * 2] = u as u8;
            dst[cx * 2 + 1] = v as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bgra_to_nv12;

    #[test]
    fn converts_primaries_to_bt601_limited() {
        // 2x2 image: white, black, red, blue (BGRA). Pitch has 4 bytes of padding.
        let pitch = 12;
        let mut src = vec![0u8; pitch * 2];
        src[0..4].copy_from_slice(&[255, 255, 255, 255]);
        src[4..8].copy_from_slice(&[0, 0, 0, 255]);
        src[pitch..pitch + 4].copy_from_slice(&[0, 0, 255, 255]);
        src[pitch + 4..pitch + 8].copy_from_slice(&[255, 0, 0, 255]);
        let (mut y, mut uv) = (Vec::new(), Vec::new());
        bgra_to_nv12(&src, pitch, 2, 2, &mut y, &mut uv);
        assert_eq!(y, vec![235, 16, 81, 41]);
        assert_eq!(uv.len(), 2);
        // Average colour is (128,64,128): a purplish grey, so Cb and Cr both sit above neutral.
        assert!(uv[0] > 128 && uv[1] > 128, "uv = {uv:?}");
    }

    #[test]
    fn handles_odd_dimensions() {
        let src = vec![128u8; 3 * 4 * 3];
        let (mut y, mut uv) = (Vec::new(), Vec::new());
        bgra_to_nv12(&src, 12, 3, 3, &mut y, &mut uv);
        assert_eq!(y.len(), 9);
        assert_eq!(uv.len(), 2 * 2 * 2);
        assert!(y.iter().all(|&v| v == 126), "y = {y:?}");
        assert!(uv.iter().all(|&v| v == 128), "uv = {uv:?}");
    }
}
