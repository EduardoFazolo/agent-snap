//! User-tunable settings, persisted as JSON in the OS config dir (shared by CLI + app).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    #[serde(rename = "outputDir")]
    pub output_dir: String,
    #[serde(rename = "captureTypedText")]
    pub capture_typed_text: bool,
    #[serde(rename = "captureURLs")]
    pub capture_urls: bool,
    #[serde(rename = "detectScreenUpdates")]
    pub detect_screen_updates: bool,
    #[serde(rename = "panelsPerImage")]
    pub panels_per_image: usize,
    #[serde(rename = "compositeWidth")]
    pub composite_width: u32,
    #[serde(rename = "quietMs")]
    pub quiet_ms: u32,
}

impl Default for Options {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Options {
            output_dir: home.join("agent-snap").to_string_lossy().into_owned(),
            capture_typed_text: true,
            capture_urls: true,
            detect_screen_updates: true,
            panels_per_image: 3,
            composite_width: 1568,
            quiet_ms: 350,
        }
    }
}

impl Options {
    /// `<config dir>/agent-snap/options.json`.
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("agent-snap").join("options.json")
    }

    pub fn load() -> Options {
        std::fs::read(Self::path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self)?)
    }

    /// `<output_dir>/sessions/yyyyMMdd-HHmmss` (local time).
    pub fn new_session_dir(&self) -> PathBuf {
        let name = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        PathBuf::from(&self.output_dir).join("sessions").join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_keys_match_swift() {
        let o = Options::default();
        let v: serde_json::Value = serde_json::to_value(&o).unwrap();
        for k in ["outputDir", "captureTypedText", "captureURLs", "detectScreenUpdates", "panelsPerImage", "compositeWidth", "quietMs"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        let partial: Options = serde_json::from_str(r#"{"quietMs": 500}"#).unwrap();
        assert_eq!(partial.quiet_ms, 500);
        assert_eq!(partial.panels_per_image, 3);
    }

    #[test]
    fn session_dir_shape() {
        let o = Options { output_dir: "/tmp/x".into(), ..Default::default() };
        let d = o.new_session_dir();
        let name = d.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 15);
        assert_eq!(&name[8..9], "-");
        assert_eq!(d.parent().unwrap(), std::path::Path::new("/tmp/x/sessions"));
    }
}
