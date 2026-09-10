//! Coalesces raw input events into user-level gestures (click, double-click, right-click, drag,
//! typing run, key, scroll run, cursor movement without clicks).
//!
//! The Swift original used dispatch timers; here the owner calls [`Coalescer::poll`] whenever
//! [`Coalescer::next_deadline`] passes and drains [`CoalesceEvent`]s after every call.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::platform::{MouseButton, RawInput, RawKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureKind {
    Click,
    DoubleClick,
    RightClick,
    Drag,
    Type,
    Key,
    Scroll,
    Cursor,
}

/// A user-level gesture derived from raw input events. Coordinates in points.
#[derive(Clone, Debug)]
pub struct Gesture {
    pub kind: GestureKind,
    pub start_t: Instant,
    pub end_t: Instant,
    pub x: f64,
    pub y: f64,
    pub end_point: Option<(f64, f64)>,
    /// Typed text or key label.
    pub text: String,
    pub scroll_dy: f64,
    /// Cursor gestures: total distance travelled (points).
    pub path_length: f64,
    /// Cursor gestures: half the bounding box diagonal (points).
    pub radius: f64,
}

impl Gesture {
    fn new(kind: GestureKind, start_t: Instant, end_t: Instant, x: f64, y: f64) -> Self {
        Gesture { kind, start_t, end_t, x, y, end_point: None, text: String::new(), scroll_dy: 0.0, path_length: 0.0, radius: 0.0 }
    }
    pub fn duration(&self) -> f64 {
        self.end_t.saturating_duration_since(self.start_t).as_secs_f64()
    }
}

/// `Begin` fires when a new gesture starts (so the recorder can resolve the target and window
/// at that moment); `Gesture` when it completes.
#[derive(Clone, Debug)]
pub enum CoalesceEvent {
    Begin(RawInput),
    Gesture(Gesture),
}

struct Typing {
    start: RawInput,
    buf: String,
    last_t: Instant,
}

struct Scroll {
    start: RawInput,
    dy: f64,
    last_t: Instant,
}

#[derive(Default)]
pub struct Coalescer {
    down: Option<RawInput>,
    typing: Option<Typing>,
    scroll: Option<Scroll>,
    moves: Vec<(Instant, f64, f64)>,
    last_point: (f64, f64),
    out: VecDeque<CoalesceEvent>,
}

impl Coalescer {
    /// Cursor movement without clicks counts once it lasts this long and travels this far.
    pub const MOVE_GAP: f64 = 0.5;
    pub const CURSOR_MIN_DURATION: f64 = 0.7;
    pub const CURSOR_MIN_PATH: f64 = 400.0;

    pub const TYPING_GAP: f64 = 1.0;
    pub const SCROLL_GAP: f64 = 0.6;
    pub const DRAG_THRESHOLD: f64 = 6.0;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_point(&self) -> (f64, f64) {
        self.last_point
    }

    /// Pops the next pending event.
    pub fn next_event(&mut self) -> Option<CoalesceEvent> {
        self.out.pop_front()
    }

    /// Takes all pending events.
    pub fn drain(&mut self) -> Vec<CoalesceEvent> {
        self.out.drain(..).collect()
    }

    /// Earliest time at which a pending run should be flushed if nothing else arrives.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut consider = |t: Instant| d = Some(d.map_or(t, |c| c.min(t)));
        if let Some(m) = self.moves.last() {
            consider(m.0 + secs(Self::MOVE_GAP));
        }
        if let Some(t) = &self.typing {
            consider(t.last_t + secs(Self::TYPING_GAP));
        }
        if let Some(s) = &self.scroll {
            consider(s.last_t + secs(Self::SCROLL_GAP));
        }
        d
    }

    /// Flushes runs whose gap has elapsed at `now`.
    pub fn poll(&mut self, now: Instant) {
        if let Some(m) = self.moves.last() {
            if elapsed(m.0, now) >= Self::MOVE_GAP {
                self.flush_moves();
            }
        }
        if let Some(t) = &self.typing {
            if elapsed(t.last_t, now) >= Self::TYPING_GAP {
                self.flush_typing();
            }
        }
        if let Some(s) = &self.scroll {
            if elapsed(s.last_t, now) >= Self::SCROLL_GAP {
                self.flush_scroll();
            }
        }
    }

    pub fn handle(&mut self, r: RawInput) {
        self.last_point = (r.x, r.y);
        match r.kind.clone() {
            RawKind::Drag(_) => {}
            RawKind::Move => {
                if let Some(last) = self.moves.last() {
                    if elapsed(last.0, r.t) > Self::MOVE_GAP {
                        self.flush_moves();
                    }
                }
                self.moves.push((r.t, r.x, r.y));
                if self.moves.len() > 20_000 {
                    self.moves.drain(..10_000);
                }
            }
            RawKind::Down(MouseButton::Left) | RawKind::Down(MouseButton::Right) => {
                self.flush_moves();
                self.flush_typing();
                self.flush_scroll();
                self.down = Some(r.clone());
                self.out.push_back(CoalesceEvent::Begin(r));
            }
            RawKind::Down(MouseButton::Other) | RawKind::Up(MouseButton::Other) => {}
            RawKind::Up(MouseButton::Left) => {
                let Some(d) = self.down.take() else { return };
                let dist = ((r.x - d.x).powi(2) + (r.y - d.y).powi(2)).sqrt();
                if dist > Self::DRAG_THRESHOLD {
                    let mut g = Gesture::new(GestureKind::Drag, d.t, r.t, d.x, d.y);
                    g.end_point = Some((r.x, r.y));
                    self.out.push_back(CoalesceEvent::Gesture(g));
                } else {
                    let kind = if r.click_count >= 2 { GestureKind::DoubleClick } else { GestureKind::Click };
                    self.out.push_back(CoalesceEvent::Gesture(Gesture::new(kind, d.t, r.t, d.x, d.y)));
                }
            }
            RawKind::Up(MouseButton::Right) => {
                let Some(d) = self.down.take() else { return };
                self.out.push_back(CoalesceEvent::Gesture(Gesture::new(GestureKind::RightClick, d.t, r.t, d.x, d.y)));
            }
            RawKind::Scroll { dy } => {
                self.flush_moves();
                self.flush_typing();
                if self.scroll.is_none() {
                    self.out.push_back(CoalesceEvent::Begin(r.clone()));
                    self.scroll = Some(Scroll { start: r.clone(), dy: 0.0, last_t: r.t });
                }
                let s = self.scroll.as_mut().unwrap();
                s.dy += dy;
                s.last_t = r.t;
            }
            RawKind::KeyDown { label, printable } => {
                self.flush_moves();
                self.flush_scroll();
                if let Some(c) = printable {
                    if self.typing.is_none() {
                        self.out.push_back(CoalesceEvent::Begin(r.clone()));
                        self.typing = Some(Typing { start: r.clone(), buf: String::new(), last_t: r.t });
                    }
                    let t = self.typing.as_mut().unwrap();
                    t.buf.push(c);
                    t.last_t = r.t;
                } else if label == "Backspace" && self.typing.as_ref().is_some_and(|t| !t.buf.is_empty()) {
                    let t = self.typing.as_mut().unwrap();
                    t.buf.pop();
                    t.last_t = r.t;
                } else {
                    self.flush_typing();
                    let mut g = Gesture::new(GestureKind::Key, r.t, r.t, r.x, r.y);
                    g.text = label;
                    self.out.push_back(CoalesceEvent::Begin(r));
                    self.out.push_back(CoalesceEvent::Gesture(g));
                }
            }
        }
    }

    pub fn flush_typing(&mut self) {
        let Some(ty) = self.typing.take() else { return };
        if ty.buf.is_empty() {
            return;
        }
        let mut g = Gesture::new(GestureKind::Type, ty.start.t, ty.last_t, ty.start.x, ty.start.y);
        g.text = ty.buf;
        self.out.push_back(CoalesceEvent::Gesture(g));
    }

    pub fn flush_scroll(&mut self) {
        let Some(sc) = self.scroll.take() else { return };
        let mut g = Gesture::new(GestureKind::Scroll, sc.start.t, sc.last_t, sc.start.x, sc.start.y);
        g.scroll_dy = sc.dy;
        self.out.push_back(CoalesceEvent::Gesture(g));
    }

    /// Emits a cursor gesture for sustained movement without clicks. Reported as-is, never interpreted.
    pub fn flush_moves(&mut self) {
        let m = std::mem::take(&mut self.moves);
        if m.len() < 3 {
            return;
        }
        let first = m[0];
        let last = m[m.len() - 1];
        let duration = elapsed(first.0, last.0);
        let mut path = 0.0;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.1, first.1, first.2, first.2);
        for i in 1..m.len() {
            path += ((m[i].1 - m[i - 1].1).powi(2) + (m[i].2 - m[i - 1].2).powi(2)).sqrt();
            min_x = min_x.min(m[i].1);
            max_x = max_x.max(m[i].1);
            min_y = min_y.min(m[i].2);
            max_y = max_y.max(m[i].2);
        }
        if duration < Self::CURSOR_MIN_DURATION || path < Self::CURSOR_MIN_PATH {
            return;
        }
        let mut g = Gesture::new(GestureKind::Cursor, first.0, last.0, (min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
        g.path_length = path;
        g.radius = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt() / 2.0;
        self.out.push_back(CoalesceEvent::Gesture(g));
    }

    pub fn flush_all(&mut self) {
        self.flush_moves();
        self.flush_typing();
        self.flush_scroll();
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

fn elapsed(from: Instant, to: Instant) -> f64 {
    to.saturating_duration_since(from).as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, s: f64) -> Instant {
        t0 + secs(s)
    }
    fn raw(t: Instant, kind: RawKind, x: f64, y: f64) -> RawInput {
        RawInput { t, kind, x, y, click_count: 1 }
    }
    fn gestures(c: &mut Coalescer) -> Vec<Gesture> {
        c.drain().into_iter().filter_map(|e| if let CoalesceEvent::Gesture(g) = e { Some(g) } else { None }).collect()
    }
    fn begins(c: &mut Coalescer) -> usize {
        c.drain().iter().filter(|e| matches!(e, CoalesceEvent::Begin(_))).count()
    }

    #[test]
    fn click() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        c.handle(raw(at(t0, 1.0), RawKind::Down(MouseButton::Left), 10.0, 10.0));
        assert_eq!(begins(&mut c), 1);
        c.handle(raw(at(t0, 1.1), RawKind::Up(MouseButton::Left), 12.0, 11.0));
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Click);
        assert_eq!((g[0].x, g[0].y), (10.0, 10.0));
        assert!((g[0].duration() - 0.1).abs() < 1e-6);
    }

    #[test]
    fn double_and_right_click() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        c.handle(raw(at(t0, 1.0), RawKind::Down(MouseButton::Left), 10.0, 10.0));
        let mut up = raw(at(t0, 1.05), RawKind::Up(MouseButton::Left), 10.0, 10.0);
        up.click_count = 2;
        c.handle(up);
        let g = gestures(&mut c);
        assert_eq!(g.last().unwrap().kind, GestureKind::DoubleClick);
        c.handle(raw(at(t0, 2.0), RawKind::Down(MouseButton::Right), 5.0, 5.0));
        c.handle(raw(at(t0, 2.1), RawKind::Up(MouseButton::Right), 5.0, 5.0));
        let g = gestures(&mut c);
        assert_eq!(g.last().unwrap().kind, GestureKind::RightClick);
    }

    #[test]
    fn drag() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        c.handle(raw(at(t0, 1.0), RawKind::Down(MouseButton::Left), 10.0, 10.0));
        c.handle(raw(at(t0, 1.2), RawKind::Drag(MouseButton::Left), 50.0, 10.0));
        c.handle(raw(at(t0, 1.5), RawKind::Up(MouseButton::Left), 100.0, 40.0));
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Drag);
        assert_eq!(g[0].end_point, Some((100.0, 40.0)));
    }

    #[test]
    fn typing_run_with_backspace_and_key() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        let key = |t, label: &str, p: Option<char>| raw(at(t0, t), RawKind::KeyDown { label: label.into(), printable: p }, 0.0, 0.0);
        c.handle(key(1.0, "h", Some('h')));
        assert_eq!(begins(&mut c), 1);
        c.handle(key(1.2, "e", Some('e')));
        c.handle(key(1.4, "x", Some('x')));
        c.handle(key(1.5, "Backspace", None));
        c.handle(key(1.6, "y", Some('y')));
        assert!(gestures(&mut c).is_empty());
        assert_eq!(c.next_deadline(), Some(at(t0, 2.6)));
        c.poll(at(t0, 2.0));
        assert!(gestures(&mut c).is_empty());
        c.poll(at(t0, 2.7));
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Type);
        assert_eq!(g[0].text, "hey");
        assert_eq!(g[0].start_t, at(t0, 1.0));
        assert_eq!(g[0].end_t, at(t0, 1.6));

        c.handle(key(3.0, "a", Some('a')));
        c.handle(key(3.1, "Cmd+S", None));
        let ev = c.drain();
        let kinds: Vec<_> = ev.iter().filter_map(|e| if let CoalesceEvent::Gesture(g) = e { Some(g.kind) } else { None }).collect();
        assert_eq!(kinds, vec![GestureKind::Type, GestureKind::Key]);
        let empty_bs = key(4.0, "Backspace", None);
        c.handle(empty_bs);
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Key);
        assert_eq!(g[0].text, "Backspace");
    }

    #[test]
    fn scroll_run() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        c.handle(raw(at(t0, 1.0), RawKind::Scroll { dy: 10.0 }, 5.0, 5.0));
        c.handle(raw(at(t0, 1.2), RawKind::Scroll { dy: 15.0 }, 6.0, 6.0));
        c.handle(raw(at(t0, 1.5), RawKind::Scroll { dy: -5.0 }, 6.0, 6.0));
        assert_eq!(begins(&mut c), 1);
        c.poll(at(t0, 2.0));
        assert!(gestures(&mut c).is_empty());
        c.poll(at(t0, 2.2));
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Scroll);
        assert_eq!(g[0].scroll_dy, 20.0);
        assert_eq!((g[0].x, g[0].y), (5.0, 5.0));
        assert_eq!(g[0].end_t, at(t0, 1.5));
    }

    #[test]
    fn cursor_gesture() {
        let t0 = Instant::now();
        let mut c = Coalescer::new();
        for i in 0..20 {
            c.handle(raw(at(t0, 1.0 + i as f64 * 0.05), RawKind::Move, 100.0 + i as f64 * 30.0, 200.0));
        }
        c.poll(at(t0, 2.4));
        assert!(gestures(&mut c).is_empty(), "flush only after MOVE_GAP");
        c.poll(at(t0, 2.5));
        let g = gestures(&mut c);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].kind, GestureKind::Cursor);
        assert!((g[0].path_length - 570.0).abs() < 1e-6);
        assert!((g[0].x - 385.0).abs() < 1e-6 && g[0].y == 200.0);
        assert!((g[0].radius - 285.0).abs() < 1e-6);

        let mut c = Coalescer::new();
        for i in 0..5 {
            c.handle(raw(at(t0, 1.0 + i as f64 * 0.05), RawKind::Move, 100.0 + i as f64 * 10.0, 200.0));
        }
        c.flush_all();
        assert!(gestures(&mut c).is_empty(), "short movement is not a gesture");
        let mut c = Coalescer::new();
        for i in 0..20 {
            c.handle(raw(at(t0, 1.0 + i as f64 * 0.05), RawKind::Move, 100.0 + i as f64 * 30.0, 200.0));
        }
        c.handle(raw(at(t0, 2.0), RawKind::Down(MouseButton::Left), 700.0, 200.0));
        c.handle(raw(at(t0, 2.1), RawKind::Up(MouseButton::Left), 700.0, 200.0));
        let kinds: Vec<_> = gestures(&mut c).iter().map(|g| g.kind).collect();
        assert_eq!(kinds, vec![GestureKind::Cursor, GestureKind::Click]);
    }
}
