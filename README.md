# agent-snap

Token-efficient screen recording for coding agents (macOS).

`agent-snap` records a normal screen video (source of truth) plus a timestamped
log of everything that happened: every frame's arrival and changed area, every
click, keystroke burst, scroll, drag, cursor movement, and window switch, with
app/window/URL and, when the app answers quickly, the accessibility name of the
element. The builder then reads frames straight out of the video, finds when the
screen settled after each input, crops to where the change happened, and writes
`flow.md`: one focused composite per window visit, with labels and arrows. Clicks
without a usable accessibility name are labeled by reading the text under the
cursor from the pixels (Vision OCR), so any app works.

Three full screenshots ≈ 4.8k tokens and unreadable after downscale. One composite
≈ 1.4k tokens and readable.

## Menu bar app

```sh
./scripts/make-app.sh && open dist/AgentSnap.app
```

Viewfinder icon in the menu bar. Popover has permission status with Grant buttons,
settings (output folder, typed text, browser URLs, no-input updates, panels per
image, panel width, settle time) and a REC/STOP button. Icon turns red while
recording; interactions with the popover itself are not recorded. When you stop,
it builds `flow.md` and offers Copy path / Open / Reveal.

Permissions are attributed to `AgentSnap.app`, so grant Screen Recording and
Accessibility to it once. macOS ties Accessibility to the code signature, so an ad-hoc signed build loses
the grant on every rebuild. `make-app.sh` signs with a self-signed "AgentSnap Dev"
certificate when one exists in the login keychain, which keeps the grant stable.
If permissions look granted but the app still says no, reset and re-grant:

```sh
tccutil reset Accessibility com.fazolo.agent-snap
tccutil reset ScreenCapture com.fazolo.agent-snap
```

## CLI

```sh
swift build -c release
.build/release/agent-snap record            # Ctrl+C to stop
.build/release/agent-snap record --duration 30 --out sessions/demo
.build/release/agent-snap build sessions/demo   # rebuild flow.md from frames
```

Output directory:

```
sessions/<stamp>/
  recording.mov       the video (HEVC, 30fps, cursor visible, 10s fragments)
  session.json        frame log, steps, window timeline, AX targets, pixel coords
  ax.log              accessibility diagnostics per click
  frames/*.png        keyframes extracted from the video (before/after each step)
  composites/*.png    focused panels with labels + arrows
  flow.md             what to paste to the agent
```

## CLI permissions

Run it from a real terminal app (Terminal, iTerm, Ghostty...). macOS attributes
permissions to that app. Grant in System Settings > Privacy & Security:

- **Screen Recording** → your terminal (ScreenCaptureKit)
- **Accessibility** → your terminal (global input tap + element names)
- **Automation** → prompt appears on first browser URL read (osascript)

## How it works

- `Capture.swift` ScreenCaptureKit stream, cursor visible. The OS delivers a frame
  when pixels change (with dirty rects); the recorder re-appends the last frame to
  keep a constant 30fps file.
- `InputTap.swift` CGEventTap for mouse/keyboard (listen-only).
- `Semantics.swift` coalesces raw events into gestures: click, double/right click,
  drag, typed text (1s gap), key chord, scroll.
- `WindowTracker.swift` frontmost app/window via AX, browser tab URL via
  AppleScript, AX hit-test on click, focused element on typing. Password fields
  are redacted.
- `Recorder.swift` writes every frame to `recording.mov` (`VideoWriter`) and logs
  its time + dirty rect. Gestures and window switches are logged with timestamps.
  Nothing waits on accessibility.
- `Builder.swift` reads frames out of the video (`FrameSource`), computes settle
  from the frame log (quiet gap, 2s max), synthesizes "Screen updated" steps from
  large repaints with no input, OCRs click targets when AX gave nothing useful.
- `AppUI.swift` NSStatusItem + NSPopover + SwiftUI, `Options.swift` settings in UserDefaults.
- `Builder.swift` focus rect = changed cells near the click ∪ target bounds,
  padded, min 700×450, max 1800×1200, clamped to the window. Rolling window keeps
  the rect stable across consecutive steps. Panels are never scaled below
  native pixels until they exceed 1000×780. Window visits under 1.5s with no
  input collapse to one line.

## Not yet

- Windows / Linux capture and input backends
- multi-display (main display only)
- hover detection, screen-update noise tuning
- mapping element names to source files (grep on AX name)
