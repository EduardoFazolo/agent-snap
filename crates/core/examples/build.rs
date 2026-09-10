//! Builds flow.md for a recorded session without OCR:
//! `cargo run -p agent-snap-core --example build -- <session dir>`

use std::path::PathBuf;
use std::time::Instant;

use agent_snap_core::{Builder, Options};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let dir = std::env::args().nth(1).map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("usage: build <session dir>"))?;
    let started = Instant::now();
    let mut b = Builder::new(dir, Options::load(), None)?;
    let out = b.build()?;
    let md = std::fs::read_to_string(&out)?;
    let cost = md.lines().find(|l| l.contains("estimated prompt cost")).unwrap_or("").to_string();
    println!("{}", out.display());
    println!("{cost}");
    eprintln!("built in {:.1}s", started.elapsed().as_secs_f64());
    Ok(())
}
