//! The prompt users paste into an agent, and the parser for the flow.md header stats.

use std::path::Path;

/// What gets pasted into an agent: the file to read and what it is.
pub fn prompt_for(flow_path: &Path) -> String {
    format!(
        "Read {}. It summarizes a screen recording of what I did. Look at the images shown inline; the full frames and video listed at the end are only for zooming in if a step is unclear.",
        flow_path.display()
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlowHeader {
    /// From `~N tokens`.
    pub tokens: Option<usize>,
    /// From `- N steps`.
    pub steps: Option<usize>,
    /// From `duration mm:ss.s`.
    pub duration: Option<String>,
}

/// Parses the first 1200 characters of a flow.md the way the Swift app does
/// (`~(\d+) tokens`, `- (\d+) steps`, `duration (\d\d:\d\d\.\d)`). `None` when nothing matched.
pub fn parse_flow_header(text: &str) -> Option<FlowHeader> {
    let head: String = text.chars().take(1200).collect();
    let h = FlowHeader {
        tokens: find_number(&head, "~", " tokens"),
        steps: find_number(&head, "- ", " steps"),
        duration: find_duration(&head),
    };
    if h == FlowHeader::default() {
        None
    } else {
        Some(h)
    }
}

/// First occurrence of `<prefix><digits><suffix>`.
fn find_number(s: &str, prefix: &str, suffix: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find(prefix) {
        let start = from + i + prefix.len();
        let digits: String = s[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && s[start + digits.len()..].starts_with(suffix) {
            return digits.parse().ok();
        }
        from = from + i + prefix.len();
    }
    None
}

fn find_duration(s: &str) -> Option<String> {
    let prefix = "duration ";
    let mut from = 0;
    while let Some(i) = s[from..].find(prefix) {
        let start = from + i + prefix.len();
        let cand: Vec<char> = s[start..].chars().take(7).collect();
        if cand.len() == 7 {
            let ok = cand[0].is_ascii_digit()
                && cand[1].is_ascii_digit()
                && cand[2] == ':'
                && cand[3].is_ascii_digit()
                && cand[4].is_ascii_digit()
                && cand[5] == '.'
                && cand[6].is_ascii_digit();
            if ok {
                return Some(cand.into_iter().collect());
            }
        }
        from = start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header() {
        let md = "# agent-snap session\n\n- recorded: 2026-09-09T18:33:48Z, duration 01:33.8\n- screen: 3024×1964 px @2.0x\n- 83 steps across 19 window visits\n- estimated prompt cost: ~31665 tokens (34 images ~28921, text ~2744)\n";
        let h = parse_flow_header(md).unwrap();
        assert_eq!(h.tokens, Some(31665));
        assert_eq!(h.steps, Some(83));
        assert_eq!(h.duration.as_deref(), Some("01:33.8"));
        assert_eq!(parse_flow_header("nothing here"), None);
        let h = parse_flow_header("- recorded: x, duration 00:07.5\n- 6 steps across 1 window visits\n").unwrap();
        assert_eq!(h.tokens, None);
        assert_eq!(h.steps, Some(6));
    }

    #[test]
    fn prompt_text() {
        let p = prompt_for(Path::new("/a/flow.md"));
        assert!(p.starts_with("Read /a/flow.md. It summarizes a screen recording of what I did."));
        assert!(p.ends_with("only for zooming in if a step is unclear."));
    }
}
