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
    /// Virtual webcam output (Linux v4l2loopback). Ignored on non-Linux.
    #[serde(default)]
    pub webcam: WebcamSettings,
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

/// Audio capture latency preset. `low` uses small buffers (~256 frames, ~6 ms
/// at 44.1 kHz) for snappy mouth response; `stable` uses larger buffers
/// (~1024, ~23 ms) for setups that glitch at small buffer sizes.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LatencyMode {
    #[default]
    Low,
    Stable,
}

impl LatencyMode {
    /// Preferred frame count per buffer for this preset.
    pub fn buffer_size(self) -> u32 {
        match self {
            LatencyMode::Low => 256,
            LatencyMode::Stable => 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AudioSettings {
    pub sample_rate: u32,
    /// Optional explicit buffer size; if omitted, the `latency` preset decides.
    #[serde(default)]
    pub buffer_size: Option<u32>,
    #[serde(default)]
    pub latency: LatencyMode,
    /// Envelope attack time constant (ms) — how fast the mouth opens when you
    /// start talking. Smaller = snappier.
    #[serde(default = "default_attack_ms")]
    pub attack_ms: f32,
    /// Envelope release time constant (ms) — how gently the mouth closes when
    /// you stop. Larger = smoother, less flutter on quiet syllables.
    #[serde(default = "default_release_ms")]
    pub release_ms: f32,
    #[serde(default)]
    pub mode: AudioMode,
    /// Empty string selects the system default device.
    #[serde(default)]
    pub device: String,
}

impl AudioSettings {
    /// The buffer size actually requested: an explicit override if given,
    /// otherwise the latency preset's default.
    pub fn effective_buffer_size(&self) -> u32 {
        self.buffer_size
            .unwrap_or_else(|| self.latency.buffer_size())
    }
}

fn default_attack_ms() -> f32 {
    6.0
}
fn default_release_ms() -> f32 {
    110.0
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

/// Virtual webcam output (Linux v4l2loopback). The avatar is composited over an
/// opaque `background` colour (webcams carry no alpha) and written as BGR4. Use
/// a chroma colour (default green) so consumers can key it out for transparency.
/// The output rate is event-driven: a frame is written the instant the
/// compositor produces one (coalesced to ~33 fps) and the loop parks at idle, so
/// there is no separate frame-rate knob.
#[derive(Debug, Clone, Deserialize)]
pub struct WebcamSettings {
    #[serde(default)]
    pub enabled: bool,
    /// `/dev/videoN` path; empty = auto-detect the first v4l2loopback device.
    #[serde(default)]
    pub device: String,
    /// Hex colour (e.g. `#00ff00`) used to fill transparent areas. Default green
    /// for chroma keying.
    #[serde(default = "default_background")]
    pub background: String,
}

fn default_background() -> String {
    "#00ff00".into()
}

impl Default for WebcamSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            background: "#00ff00".into(),
        }
    }
}

impl WebcamSettings {
    /// Parse `background` (a CSS-style `#rrggbb` hex string) into RGB.
    pub fn background_rgb(&self) -> Result<[u8; 3]> {
        let hex = self.background.trim_start_matches('#');
        if hex.len() != 6 {
            anyhow::bail!(
                "[webcam].background must be #rrggbb (got {:?})",
                self.background
            );
        }
        let r = u8::from_str_radix(&hex[0..2], 16)?;
        let g = u8::from_str_radix(&hex[2..4], 16)?;
        let b = u8::from_str_radix(&hex[4..6], 16)?;
        Ok([r, g, b])
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

        for ms in [self.audio.attack_ms, self.audio.release_ms] {
            if !ms.is_finite() || !(0.1..=2000.0).contains(&ms) {
                bail!(
                    "[audio] attack_ms/release_ms must be finite and in [0.1, 2000] ms (got {ms})"
                );
            }
        }

        if self.audio.sample_rate == 0 {
            bail!("[audio].sample_rate must be non-zero");
        }
        if self.audio.effective_buffer_size() == 0 {
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
latency = "low"

[thresholds]
partial = 0.02
medium = 0.08
open = 0.18

[engine]
default_emotion = "calm"
asset_root = "./assets"
"#;

    #[test]
    fn parses_minimal_config() {
        let cfg = AppConfig::from_toml_str(MINIMAL).expect("valid config");
        assert_eq!(cfg.engine.default_emotion, "calm");
        assert_eq!(cfg.audio.mode, AudioMode::Input);
        assert_eq!(cfg.audio.latency, LatencyMode::Low);
        assert_eq!(cfg.audio.effective_buffer_size(), 256);
        assert!((cfg.audio.attack_ms - 6.0).abs() < 1e-6); // default applied
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
    fn rejects_bad_envelope() {
        let raw = MINIMAL.replacen("[audio]", "[audio]\nattack_ms = 0.0", 1);
        let err = AppConfig::from_toml_str(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("attack_ms"));
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
latency = "stable"
attack_ms = 4.0
release_ms = 90.0
mode = "loopback"
device = "alsa_output.pci-0000_00_1b.0.analog-stereo.monitor"

[thresholds]
partial = 0.01
medium = 0.05
open = 0.12

[engine]
default_emotion = ""
asset_root = "./assets"

[timers]
surprised = 2.5
"#;
        let cfg = AppConfig::from_toml_str(raw).expect("valid");
        assert_eq!(cfg.audio.mode, AudioMode::Loopback);
        assert_eq!(cfg.audio.sample_rate, 48000);
        assert_eq!(cfg.audio.latency, LatencyMode::Stable);
        assert_eq!(cfg.audio.effective_buffer_size(), 1024);
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
