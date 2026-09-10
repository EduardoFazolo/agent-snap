# agent-snap

Show a coding agent what you just did on screen, without burning your whole context on screenshots.

Your agent can't see your screen. So you end up describing the bug in words, or pasting
screenshots. Words miss things. Screenshots are heavy and go blurry once they get downscaled,
and you need a bunch of them to tell a story.

agent-snap records what you do as a normal screen video, plus a log of every click, keystroke,
scroll and window switch. Then it turns that into `flow.md`: a short flowchart of what happened,
with cropped before/after images, labels and arrows. You paste that one file into any agent.

The video is the source of truth. Everything in `flow.md` is built from it, so nothing gets
made up. Works on any app, native or web, because it reads the screen, not the DOM.

Rough cost: three full screenshots are about 4.8k tokens and unreadable after downscale. One
agent-snap composite is about 1.4k tokens and you can actually read it.

macOS today. Windows backend is written but not tested on real hardware yet.

## Example

I filled in an expense form and switched a couple of tabs on a small demo page. 24 seconds.
agent-snap turned it into a `flow.md` of about **5,200 tokens**: four images and eleven step
lines. That whole thing is the prompt you paste into an agent.

![recording](docs/example/recording.gif)

*(compressed clip that ships in the repo. Full-res: [recording.mp4](docs/example/recording.mp4))*

Here is what the agent actually reads, [`docs/example/flow.md`](docs/example/flow.md) plus these
four images:

![step 1](docs/example/composites/run-01-a.png)
![step 2](docs/example/composites/run-01-b.png)
![step 3](docs/example/composites/run-01-c.png)
![step 4](docs/example/composites/run-01-d.png)

Every click and keystroke gets a label and an arrow pointing at it. When the app doesn't hand
over a name for what you clicked, agent-snap reads the text off the pixels instead (you'll see
"text read from screen"). So it names the Category dropdown opening, the value landing on
Travel, the Billable checkbox getting ticked, and both tab switches, same as it would in a
native app.

The whole example is reproducible. `smoke/run.sh` serves the page, opens a throwaway Chrome,
records with agent-snap, and drives the form with real mouse and keyboard. `smoke/` is
gitignored, only the finished result in `docs/example/` is committed.

## Use it

Menu bar app:

```sh
cargo build --release
scripts/make-app.sh && open dist/AgentSnap.app
```

You get a viewfinder icon in the menu bar. Click it for a small popover: permissions, a few
settings, arrows to look back at old recordings, and a REC button. Icon goes red while
recording. It ignores clicks on its own popover, so that never ends up in your recording.

Hit STOP and it builds `flow.md`, then gives you Copy prompt, Open and Reveal.

Copy prompt puts one line plus the file path on your clipboard. Paste it into any agent. It
copies the path, not the images, because images can't ride the clipboard. The agent opens the
file and follows the image links itself.

Or from the terminal:

```sh
cargo build --release
target/release/agent-snap record                   # Ctrl+C to stop
target/release/agent-snap record --duration 30 --out sessions/demo
target/release/agent-snap build sessions/demo      # rebuild flow.md from a recording
```

Each recording is a folder:

```
sessions/<stamp>/
  recording.mp4       the video, 30fps, cursor visible
  session.json        frame log, steps, window timeline, pixel coords
  frames/*.png        before/after frames pulled from the video
  composites/*.png    the cropped panels with labels and arrows
  flow.md             the thing you paste to the agent
```

It uses `ffmpeg` to encode and to pull frames back out. It'll use the one on your `PATH`, or
grab its own once if you don't have it.

## Permissions

**macOS.** Grant Screen Recording and Accessibility. The menu bar app asks for them as
`AgentSnap.app`. The CLI asks as whatever terminal you ran it from.

One annoying macOS thing: it ties the Accessibility grant to the app's signature, so an ad-hoc
build loses the grant every time you rebuild. `make-app.sh` signs with a self-signed
"AgentSnap Dev" cert if you have one in your login keychain, which keeps the grant stable
across rebuilds.

If it looks granted but the app still says no, reset and grant again:

```sh
tccutil reset Accessibility com.fazolo.agent-snap
tccutil reset ScreenCapture com.fazolo.agent-snap
```

First time it reads a browser tab URL, macOS asks for Automation once too.

**Windows.** Nothing to grant. Capture, input hooks and UI Automation all work without a
prompt.

## How it works

- **Capture.** The OS only hands over a frame when pixels actually change, and tells you which
  rectangles. agent-snap writes one frame every 1/30s anyway, repeating the last one when
  nothing moved, so the file is a steady 30fps and your cursor is always in it.
- **Input.** A listen-only tap on mouse and keyboard. It watches, it never swallows your input.
- **Gestures.** Raw events get grouped into things like click, double click, drag, a typed run
  of text, a key chord, a scroll. Repeated identical clicks are kept on purpose, not merged.
  That's how an agent can tell a page stopped responding and you clicked five times.
- **Window tracking.** Front app and window, browser tab URL, what you clicked, what's focused
  when you type. Password fields are redacted. Nothing waits on the accessibility API, and there
  are zero app-specific hacks.
- **Builder.** Pulls frames from the video, works out when the screen settled after each action,
  crops to where the change happened, and reads the click target off the pixels when the app
  gave nothing. Panels that barely changed collapse to a single text line instead of a near-
  identical image.

## Layout

```
crates/core      OS-neutral: session model, gestures, recording, ffmpeg, the flow.md builder
crates/macos     ScreenCaptureKit, CGEventTap, Accessibility, Vision OCR
crates/windows   Windows.Graphics.Capture, low-level hooks, UI Automation, Windows OCR
app              the binary: CLI plus the Tauri menu bar app
```

All the OS-specific stuff sits behind five traits in `crates/core/src/platform.rs`:
`ScreenCapture`, `InputTap`, `WindowTracker`, `Ocr`, `Permissions`. A new OS means writing those
and nothing else.

## Not yet

- Windows backend compiles but nobody has run it on a real Windows machine
- Linux
- more than one display (main display only for now)
- merging panels that show basically the same thing twice
