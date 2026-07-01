//! Strongly-typed configuration loaded from `config.toml`.
//!
//! The raw structs mirror the TOML schema; [`AppConfig::validate`] enforces
//! invariants (threshold ordering, non-empty default emotion, sensible bounds).

use crate::protocol::{MouthConfig, MouthState};
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
    /// Eye-blink scheduler tuning.
    #[serde(default)]
    pub blink: BlinkSettings,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdSettings {
    pub partial: f32,
    pub medium: f32,
    pub open: f32,
    /// Optional: which mouth levels are active, by name. Defaults to all four
    /// (`["closed", "partial", "medium", "open"]`). Disable a level (e.g.
    /// `["partial", "medium", "open"]` to drop `closed`) to make the next-lowest
    /// the resting mouth — handy for A/B-testing 3 vs 4 mouth positions.
    #[serde(default)]
    pub enabled: Vec<String>,
}

impl ThresholdSettings {
    /// Build the runtime [`MouthConfig`], validating enablement + ordering.
    pub fn to_mouth_config(&self) -> Result<MouthConfig> {
        let mut enabled = [true; 4];
        if !self.enabled.is_empty() {
            enabled = [false; 4];
            for name in &self.enabled {
                let lvl = MouthState::from_str_ci(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown mouth level in [thresholds].enabled: {name:?} \
                         (expected closed|partial|medium|open)"
                    )
                })?;
                enabled[lvl.level() as usize] = true;
            }
        }
        let cfg = MouthConfig {
            enabled,
            partial: self.partial,
            medium: self.medium,
            open: self.open,
        };
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("[thresholds] {e}"))?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineSettings {
    /// Resting emotion (eye-expression set). Empty = base/default eyes. If it
    /// names an emotion not present in the catalog, the loader falls back to the
    /// default eyes and logs a warning.
    #[serde(default)]
    pub default_emotion: String,
    pub asset_root: PathBuf,
    pub bind: String,
}

fn default_true() -> bool {
    true
}
fn default_blink_min() -> f32 {
    2.5
}
fn default_blink_max() -> f32 {
    6.0
}
fn default_blink_duration() -> f32 {
    0.12
}
fn default_double_chance() -> f32 {
    0.15
}

/// Eye-blink scheduler settings. The interval is randomised per cycle between
/// `min_interval` and `max_interval` so blinks feel natural rather than
/// metronomic. All fields have defaults, so a `[blink]` section (or individual
/// keys) may be omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct BlinkSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_blink_min")]
    pub min_interval: f32,
    #[serde(default = "default_blink_max")]
    pub max_interval: f32,
    #[serde(default = "default_blink_duration")]
    pub duration: f32,
    #[serde(default = "default_double_chance")]
    pub double_chance: f32,
}

impl Default for BlinkSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_interval: 2.5,
            max_interval: 6.0,
            duration: 0.12,
            double_chance: 0.15,
        }
    }
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
        if self.engine.bind.trim().is_empty() {
            bail!("[engine].bind must not be empty");
        }

        let t = &self.thresholds;
        if !(t.partial < t.medium && t.medium < t.open) {
            bail!(
                "[thresholds] must satisfy partial < medium < open (got partial={}, medium={}, open={})",
                t.partial,
                t.medium,
                t.open
            );
        }
        for v in [t.partial, t.medium, t.open] {
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

        let b = &self.blink;
        if b.min_interval <= 0.0 || !b.min_interval.is_finite() {
            bail!("[blink].min_interval must be positive and finite");
        }
        if b.max_interval < b.min_interval {
            bail!(
                "[blink].max_interval ({}) must be >= min_interval ({})",
                b.max_interval,
                b.min_interval
            );
        }
        if b.duration <= 0.0 || !b.duration.is_finite() {
            bail!("[blink].duration must be positive and finite");
        }
        if !(0.0..=1.0).contains(&b.double_chance) {
            bail!("[blink].double_chance must be in [0.0, 1.0]");
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
partial = 0.02
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
        assert!(msg.contains("partial < medium < open"), "{msg}");
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
partial = 0.01
medium = 0.05
open = 0.12

[engine]
default_emotion = ""
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

    #[test]
    fn blink_defaults_when_section_absent() {
        let cfg = AppConfig::from_toml_str(MINIMAL).unwrap();
        assert!(cfg.blink.enabled);
        assert_eq!(cfg.blink.min_interval, 2.5);
        assert_eq!(cfg.blink.max_interval, 6.0);
        assert!((cfg.blink.duration - 0.12).abs() < 1e-6);
    }

    #[test]
    fn blink_partial_section_uses_field_defaults() {
        let raw = format!(
            "{MINIMAL}\n[blink]\nenabled = false\nmin_interval = 3.0\n"
        );
        let cfg = AppConfig::from_toml_str(&raw).unwrap();
        assert!(!cfg.blink.enabled);
        assert_eq!(cfg.blink.min_interval, 3.0);
        // omitted fields keep defaults
        assert_eq!(cfg.blink.max_interval, 6.0);
        assert!((cfg.blink.duration - 0.12).abs() < 1e-6);
    }

    #[test]
    fn blink_rejects_bad_ranges() {
        let mk = |toml: &str| {
            AppConfig::from_toml_str(&format!("{MINIMAL}\n{toml}"))
        };
        assert!(
            mk("[blink]\nmin_interval = 5.0\nmax_interval = 2.0\n").is_err()
        );
        assert!(mk("[blink]\nduration = 0.0\n").is_err());
        assert!(mk("[blink]\ndouble_chance = 2.0\n").is_err());
    }
}
