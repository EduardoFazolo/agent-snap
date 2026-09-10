//! OS-neutral half of agent-snap: session model, gesture coalescing, recording orchestration,
//! ffmpeg-backed video writing/reading, and the flow.md builder.

pub mod builder;
pub mod encoder;
pub mod ffmpeg;
pub mod frames;
pub mod model;
pub mod options;
pub mod platform;
pub mod prompt;
pub mod recorder;
pub mod semantics;

pub use builder::{image_tokens, Builder, Stats};
pub use encoder::VideoWriter;
pub use frames::FrameSource;
pub use model::{fmt_t, CursorSample, FrameLog, Session, Step, StepKind, WindowSegment};
pub use options::Options;
pub use platform::*;
pub use prompt::{parse_flow_header, prompt_for, FlowHeader};
pub use recorder::Recorder;
pub use semantics::{CoalesceEvent, Coalescer, Gesture, GestureKind};
