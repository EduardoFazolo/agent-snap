# agent-snap-windows

Windows backend for agent-snap. Implements every trait in `agent_snap_core::platform`
(`ScreenCapture`, `InputTap`, `WindowTracker`, `Ocr`, `Permissions`) and exposes them through
`agent_snap_windows::backend()`.

| Module           | OS API                                                                 |
|------------------|------------------------------------------------------------------------|
| `capture.rs`     | Windows.Graphics.Capture (primary monitor) + D3D11 staging copy, BGRA -> NV12 (BT.601 limited) |
| `input.rs`       | `WH_MOUSE_LL` / `WH_KEYBOARD_LL` on a dedicated message-loop thread    |
| `windows.rs`     | `GetForegroundWindow`, DWM extended frame bounds, UI Automation        |
| `ocr.rs`         | `Windows.Media.Ocr` (per line: words joined, union of word rects)      |
| `permissions.rs` | Always `Granted`; `request` / `open_settings` are no-ops               |
| `dpi.rs`         | Per-monitor-v2 DPI awareness, primary monitor scale (`dpi / 96`)       |

The crate body is gated with `#![cfg(target_os = "windows")]`, so it is an empty library on
macOS/Linux and the workspace builds everywhere.

## Status: written on a Mac, cross-compile-checked only

Everything below compiles (`cargo check` / `cargo clippy` for `x86_64-pc-windows-msvc`) but has
**never run on Windows hardware**. Treat these as the first things to verify:

- **Capture**
  - `IGraphicsCaptureItemInterop::CreateForMonitor` from a plain (non-packaged) process, and
    whether `RoInitialize(MULTITHREADED)` on the calling thread is enough for WinRT activation.
  - `IsBorderRequired(false)` needs Windows 11 / `IGraphicsCaptureSession3`; on Windows 10 the
    yellow capture border stays (failure is logged and ignored).
  - The FrameArrived handler runs on thread-pool threads; the D3D11 immediate context is used
    only under a mutex (`D3d` is `unsafe impl Send` for that reason). Verify no deadlock/stall
    when the callback outlives `stop()` (an `alive` flag short-circuits it).
  - Frame throttling to `max_fps` and pool `Recreate` on display-mode change are untested.
  - The NV12 converter is scalar (no SIMD); at 4K it may be the fps bottleneck. Unit-tested for
    colour primaries and odd sizes only.
  - `Frame::dirty` is always empty (WGC exposes no dirty rects).
- **Input**
  - Hook coordinates are assumed to be physical pixels because the process is made
    per-monitor-v2 DPI aware; if the host already set a different awareness the division by
    `scale` will be wrong. Multi-monitor with mixed DPI uses the primary monitor's scale.
  - Double-click counting reimplements `GetDoubleClickTime` + `SM_CXDOUBLECLK/2` rules.
  - Wheel: one notch (120) = 3 lines = 48 points (`LINE_POINTS = 16`), sign flipped so positive
    means the user scrolled down. The 3-lines default is hard-coded (not read from
    `SPI_GETWHEELSCROLLLINES`).
  - `ToUnicodeEx` with flag `0x4` (do not change kernel dead-key state) requires Windows 10
    1607+; the key state array is synthesised from `GetAsyncKeyState`, so AltGr layouts and
    dead keys need a real check. Labels: plain printable keys are labelled with the typed
    character; shortcuts as `Ctrl+Shift+S`; bare modifier presses are not reported.
  - Low-level hooks are silently removed by Windows if the callback exceeds
    `LowLevelHooksTimeout` (default 300 ms). The callback only forwards to the recorder's
    closure; make sure that closure never blocks.
- **Window tracker / UIA**
  - `hit_test` descends from `ElementFromPoint` with the control-view walker (depth <= 12,
    48 siblings per level, ~40 ms budget). UIA calls into another process can individually
    exceed that budget; check p99 latency on Chromium and Electron windows.
  - `tab_url` searches for an `Edit` named one of `ADDRESS_BAR_NAMES` (Chromium: "Address and
    search bar"; Firefox: "Search or enter address"/"Search with Google or enter address").
    Chromium only builds its UIA tree once a UIA client has connected; the first call may return
    `None`. Cached 2 s per foreground `HWND`.
  - The `IUIAutomation` instance is created lazily per thread (`thread_local`), with COM
    initialised MTA on that thread. If the recorder calls from an STA thread the
    `CoInitializeEx` error is ignored and the STA is used.
  - Bounds are converted from pixels to points with the primary monitor's scale.
- **OCR**
  - `OcrEngine::TryCreateFromUserProfileLanguages` returns an error when no OCR language pack is
    installed; the backend then returns an empty vec. Images larger than
    `OcrEngine::MaxImageDimension` are downscaled and the rects mapped back.
  - Blocking is done with `SetCompleted` + a channel and a 10 s timeout (the `windows-future`
    `Async::join` helper is private).

## Running the probe on a Windows machine

```text
cargo run -p agent-snap-windows --example probe -- [seconds] [out.png]
```

It captures the primary monitor for N seconds (default 5) at up to 30 fps, prints the achieved
fps, writes the last frame as `probe-frame.png` (NV12 -> RGB), prints every input event seen while
recording, then prints `front()`, `tab_url()`, `hit_test` at the cursor (with its latency),
`focused()` / `focused_is_secure()` and the OCR lines of the dumped frame. Use `RUST_LOG=debug`
to see why a best-effort lookup returned nothing.

Things to eyeball in the PNG: cursor present, colours not swapped (red stays red), no yellow
border on Windows 11, correct size for the display's scale factor.

## Cross-checking from macOS

```text
cargo check  -p agent-snap-windows --target x86_64-pc-windows-msvc
cargo clippy -p agent-snap-windows --target x86_64-pc-windows-msvc --all-targets
```

`agent-snap-core` currently pulls `ring` (via `ffmpeg-sidecar -> ureq -> rustls`), whose build
script compiles C for the MSVC target and wants `lib.exe`. On a Mac without a Windows SDK this
works with ring's sysroot-free mode plus a wrapper named `llvm-ar` around BSD `ar`:

```sh
mkdir -p /tmp/xar && printf '#!/bin/sh\nexec /usr/bin/ar "$@"\n' > /tmp/xar/llvm-ar && chmod +x /tmp/xar/llvm-ar
AR_x86_64_pc_windows_msvc=/tmp/xar/llvm-ar \
CFLAGS_x86_64_pc_windows_msvc="-nostdlibinc -DRING_CORE_NOSTDLIBINC=1 -fgnuc-version=4.2.1 -Wno-everything" \
cargo check -p agent-snap-windows --target x86_64-pc-windows-msvc
```

This only affects type-checking on a Mac; a real Windows build needs none of it.
