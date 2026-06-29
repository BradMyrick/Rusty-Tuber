//! Strongly-typed configuration loaded from `config.toml`.
//!
//! The raw structs mirror the TOML schema; [`AppConfig::validate`] enforces
//! invariants (threshold ordering, non-empty default emotion, sensible bounds).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level application configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub audio: AudioSettings,
    pub thresholds: ThresholdSettings,
    pub engine: EngineSettings,
    /// Emotion name -> auto-revert duration (seconds). Emotions absent from
    /// this map stick until manually cleared.
    #[serde(default)]
    pub timers: HashMap<String, f32>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// Capture from a microphone / input device.
    #[default]
    Input,
    /// Capture from a system-output monitor (loopback) device.
    Loopback,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AudioSettings {
    pub sample_rate: u32,
    pub buffer_size: u32,
    #[serde(default = "default_smoothing")]
    pub smoothing_factor: f32,
    #[serde(default)]
    pub mode: AudioMode,
    /// Empty string selects the system default device.
    #[serde(default)]
    pub device: String,
}

fn default_smoothing() -> f32 {
    0.35
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ThresholdSettings {
    pub slight: f32,
    pub medium: f32,
    pub open: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineSettings {
    pub default_emotion: String,
    pub asset_root: PathBuf,
    pub bind: String,
}

impl AppConfig {
    /// Parse a TOML string into validated configuration.
    pub fn from_toml_str(raw: &str) -> Result<Self> {
        let cfg: AppConfig =
            toml::from_str(raw).context("parsing config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load and validate configuration from a file on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        Self::from_toml_str(&raw)
    }

    /// Enforce structural invariants. Does not check that the asset catalog
    /// exists on disk (that happens later, non-fatally).
    pub fn validate(&self) -> Result<()> {
        if self.engine.default_emotion.trim().is_empty() {
            bail!("[engine].default_emotion must not be empty");
        }
        if self.engine.bind.trim().is_empty() {
            bail!("[engine].bind must not be empty");
        }

        let t = &self.thresholds;
        if !(t.slight < t.medium && t.medium < t.open) {
            bail!(
                "[thresholds] must satisfy slight < medium < open (got slight={}, medium={}, open={})",
                t.slight,
                t.medium,
                t.open
            );
        }
        for v in [t.slight, t.medium, t.open] {
            if !v.is_finite() || v < 0.0 {
                bail!(
                    "[thresholds] values must be non-negative finite numbers"
                );
            }
        }

        if !self.audio.smoothing_factor.is_finite()
            || !(0.0..=1.0).contains(&self.audio.smoothing_factor)
        {
            bail!(
                "[audio].smoothing_factor must be in [0.0, 1.0] (got {})",
                self.audio.smoothing_factor
            );
        }

        if self.audio.sample_rate == 0 {
            bail!("[audio].sample_rate must be non-zero");
        }
        if self.audio.buffer_size == 0 {
            bail!("[audio].buffer_size must be non-zero");
        }

        for (name, secs) in &self.timers {
            if !secs.is_finite() || *secs <= 0.0 {
                bail!("[timers].{name} must be a positive finite number (got {secs})");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[audio]
sample_rate = 44100
buffer_size = 1024

[thresholds]
slight = 0.02
medium = 0.08
open = 0.18

[engine]
default_emotion = "calm"
asset_root = "./assets"
bind = "127.0.0.1:8080"
"#;

    #[test]
    fn parses_minimal_config() {
        let cfg = AppConfig::from_toml_str(MINIMAL).expect("valid config");
        assert_eq!(cfg.engine.default_emotion, "calm");
        assert_eq!(cfg.audio.mode, AudioMode::Input);
        assert!(cfg.audio.device.is_empty());
        assert!(cfg.timers.is_empty()); // default empty
    }

    #[test]
    fn rejects_non_monotonic_thresholds() {
        let raw = MINIMAL.replacen("open = 0.18", "open = 0.05", 1);
        let err = AppConfig::from_toml_str(&raw).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("slight < medium < open"), "{msg}");
    }

    #[test]
    fn rejects_bad_smoothing() {
        let raw =
            MINIMAL.replacen("[audio]", "[audio]\nsmoothing_factor = 3.0", 1);
        let err = AppConfig::from_toml_str(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("smoothing_factor"));
    }

    #[test]
    fn rejects_nonpositive_timer() {
        let raw = format!("{MINIMAL}\n[timers]\nsurprised = 0\n");
        let err = AppConfig::from_toml_str(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("surprised"));
    }

    #[test]
    fn parses_loopback_mode_and_timers() {
        let raw = r#"
[audio]
sample_rate = 48000
buffer_size = 512
smoothing_factor = 0.5
mode = "loopback"
device = "alsa_output.pci-0000_00_1b.0.analog-stereo.monitor"

[thresholds]
slight = 0.01
medium = 0.05
open = 0.12

[engine]
default_emotion = "calm"
asset_root = "./assets"
bind = "0.0.0.0:9000"

[timers]
surprised = 2.5
"#;
        let cfg = AppConfig::from_toml_str(raw).expect("valid");
        assert_eq!(cfg.audio.mode, AudioMode::Loopback);
        assert_eq!(cfg.audio.sample_rate, 48000);
        assert_eq!(cfg.engine.bind, "0.0.0.0:9000");
        assert_eq!(cfg.timers.get("surprised"), Some(&2.5_f32));
    }
}
