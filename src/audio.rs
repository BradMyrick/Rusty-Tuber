//! Audio capture and analysis.
//!
//! Opens a cpal input stream (microphone or a PipeWire loopback *monitor*
//! source), computes per-buffer RMS loudness, applies an exponential moving
//! average for stability, and forwards the smoothed level to the state task as
//! [`StateCommand::SetVolume`].
//!
//! The callback is lock-free: smoothing state lives in an `AtomicU32` storing
//! the bit pattern of an `f32`, so the realtime audio thread never blocks.
//!
//! Device selection:
//! - `audio.device` non-empty  -> exact device-name match.
//! - `audio.mode = "input"`    -> system default input (mic).
//! - `audio.mode = "loopback"` -> first device whose name contains "monitor"
//!   (PipeWire exposes sink monitors this way). Use `--list-audio-devices` to
//!   find the exact name.

use crate::config::{AudioMode, AudioSettings};
use crate::state::StateCommand;
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, SampleFormat, SampleRate, Stream, StreamConfig,
    SupportedStreamConfig,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// Lock-free envelope tuning shared between the state task (writer) and the
/// realtime audio callback (reader). Values are stored as `f32` bit patterns in
/// `AtomicU32`s; `Relaxed` ordering suffices — a torn read only applies one
/// stale coefficient for a single buffer.
#[derive(Clone)]
pub struct EnvelopeControl {
    attack_ms: Arc<AtomicU32>,
    release_ms: Arc<AtomicU32>,
}

impl EnvelopeControl {
    pub fn new(attack_ms: f32, release_ms: f32) -> Self {
        Self {
            attack_ms: Arc::new(AtomicU32::new(attack_ms.to_bits())),
            release_ms: Arc::new(AtomicU32::new(release_ms.to_bits())),
        }
    }

    pub fn get(&self) -> (f32, f32) {
        (
            f32::from_bits(self.attack_ms.load(Ordering::Relaxed)),
            f32::from_bits(self.release_ms.load(Ordering::Relaxed)),
        )
    }

    pub fn set(&self, attack_ms: f32, release_ms: f32) {
        self.attack_ms.store(attack_ms.to_bits(), Ordering::Relaxed);
        self.release_ms
            .store(release_ms.to_bits(), Ordering::Relaxed);
    }
}

/// Entry describing an input device, for the `--list-audio-devices` command.
pub struct DeviceInfo {
    pub name: String,
    pub is_monitor: bool,
    pub is_default: bool,
}

/// Enumerate input devices of the default host.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    for d in host.input_devices()? {
        let name = d.name().unwrap_or_else(|_| "<unknown>".into());
        let lname = name.to_ascii_lowercase();
        let is_monitor = lname.contains("monitor");
        let is_default = default_name.as_deref() == Some(&name);
        out.push(DeviceInfo {
            name,
            is_monitor,
            is_default,
        });
    }
    Ok(out)
}

/// Print every input device, marking the default and any loopback/monitor
/// sources. Used by the `list-audio-devices` subcommand.
pub fn run_list_devices() -> Result<()> {
    let devices = list_input_devices().context("enumerating audio devices")?;
    if devices.is_empty() {
        println!("No input devices found.");
        return Ok(());
    }
    println!("Input devices (mark the one you want with [audio].device):");
    for d in devices {
        let flags = match (d.is_default, d.is_monitor) {
            (true, true) => "  [default] [loopback]",
            (true, false) => "  [default]",
            (false, true) => "  [loopback]",
            (false, false) => "  ",
        };
        println!("{flags}{}", d.name);
    }
    Ok(())
}

fn pick_device(audio: &AudioSettings) -> Result<Device> {
    let host = cpal::default_host();

    if !audio.device.is_empty() {
        for d in host.input_devices()? {
            if d.name().map(|n| n == audio.device).unwrap_or(false) {
                return Ok(d);
            }
        }
        bail!(
            "audio device not found: {:?}. Run `rusty-tuber --list-audio-devices` to see options.",
            audio.device
        );
    }

    match audio.mode {
        AudioMode::Input => host
            .default_input_device()
            .context("no default input device available"),
        AudioMode::Loopback => {
            for d in host.input_devices()? {
                let is_mon = d
                    .name()
                    .map(|n| n.to_ascii_lowercase().contains("monitor"))
                    .unwrap_or(false);
                if is_mon {
                    return Ok(d);
                }
            }
            bail!(
                "no loopback (monitor) device found. \
                 Run `rusty-tuber --list-audio-devices`; PipeWire sink monitors appear as \
                 inputs ending in '.monitor'. If none is listed, create one with \
                 `pw-loopback -m '[Capture]' &` or pick a specific device name."
            );
        }
    }
}

/// Locate a config compatible with the device, preferring `f32` samples and the
/// configured sample rate when the device advertises a matching range.
fn negotiate_config(
    device: &Device,
    audio: &AudioSettings,
) -> Result<(StreamConfig, SampleFormat)> {
    let want = SampleRate(audio.sample_rate);

    // Prefer an f32 config whose range includes the requested sample rate.
    let mut chosen: Option<SupportedStreamConfig> = None;
    if let Ok(ranges) = device.supported_input_configs() {
        for r in ranges {
            if r.sample_format() == SampleFormat::F32
                && r.min_sample_rate() <= want
                && want <= r.max_sample_rate()
            {
                chosen = Some(r.with_sample_rate(want));
                break;
            }
        }
    }

    // Fallback to the device default config (callback converts any format).
    let supported = chosen.map(Ok).unwrap_or_else(|| {
        device
            .default_input_config()
            .context("device has no default input config")
    })?;

    let fmt = supported.sample_format();
    let mut cfg: StreamConfig = supported.into();
    // Buffer size is decided at stream-build time (Fixed, then Default fallback).
    cfg.buffer_size = cpal::BufferSize::Default;
    Ok((cfg, fmt))
}

/// Start capturing and analysing audio. The returned `Stream` must be kept
/// alive for the lifetime of the application (dropping it stops capture).
pub fn start(
    audio: &AudioSettings,
    envelope: EnvelopeControl,
    cmd_tx: UnboundedSender<StateCommand>,
) -> Result<Stream> {
    let device = pick_device(audio).context("selecting audio device")?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    let (base_config, sample_format) = negotiate_config(&device, audio)?;

    // Try a fixed buffer size first; fall back to the negotiated default.
    let preferred = audio.effective_buffer_size().max(1);
    let mut fixed_config = base_config.clone();
    fixed_config.buffer_size = cpal::BufferSize::Fixed(preferred);
    fixed_config.sample_rate = SampleRate(audio.sample_rate);
    // Per-buffer interval for the envelope coefficients (seconds).
    let buffer_interval = preferred as f32 / audio.sample_rate as f32;

    let (att_ms, rel_ms) = envelope.get();
    info!(
        device = %device_name,
        sample_rate = fixed_config.sample_rate.0,
        channels = fixed_config.channels,
        buffer = preferred,
        latency = ?audio.latency,
        format = ?sample_format,
        attack_ms = att_ms,
        release_ms = rel_ms,
        mode = ?audio.mode,
        "starting audio capture"
    );

    let smoothed: Arc<AtomicU32> = Arc::new(AtomicU32::new(0.0f32.to_bits()));

    let err_cb = |err| warn!(error = %err, "audio stream error");

    let build = |cfg: StreamConfig| {
        let smoothed = smoothed.clone();
        let envelope = envelope.clone();
        let dt = buffer_interval;
        let cmd_tx = cmd_tx.clone();
        let fmt = sample_format;
        device.build_input_stream_raw(
            &cfg,
            fmt,
            move |data, _info| {
                // Guard against panics unwinding across the realtime/FFI
                // boundary, which is UB in cpal callbacks.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let raw = compute_rms(data.bytes(), fmt);
                    let prev = f32::from_bits(smoothed.load(Ordering::Relaxed));
                    // Asymmetric one-pole: fast attack so the mouth opens within
                    // ~1 buffer, slower release for a smooth close.
                    let (att_ms, rel_ms) = envelope.get();
                    let tau_ms = if raw > prev { att_ms } else { rel_ms };
                    let coef = (-dt / (tau_ms.max(0.1) / 1000.0)).exp();
                    let next = coef * prev + (1.0 - coef) * raw;
                    smoothed.store(next.to_bits(), Ordering::Relaxed);
                    let _ = cmd_tx.send(StateCommand::SetVolume(next));
                }));
            },
            err_cb,
            None,
        )
    };

    let stream = build(fixed_config).or_else(|e| {
        debug!(error = %e, "fixed-buffer build failed; retrying with Default buffer");
        let mut fallback = base_config;
        fallback.sample_rate = SampleRate(audio.sample_rate);
        build(fallback)
    })?;

    stream.play().context("starting audio stream")?;
    Ok(stream)
}

/// Compute the RMS loudness of a cpal buffer in roughly `[0.0, 1.0]`, converting
/// any supported sample format on the fly.
fn compute_rms(bytes: &[u8], format: SampleFormat) -> f32 {
    match format {
        SampleFormat::F32 => rms(as_f32(bytes).iter().copied()),
        SampleFormat::F64 => rms(as_f64(bytes).iter().map(|&s| s as f32)),
        SampleFormat::I16 => {
            rms(as_i16(bytes).iter().map(|&s| s as f32 / 32_768.0))
        }
        SampleFormat::U16 => rms(as_u16(bytes)
            .iter()
            .map(|&s| (s as f32 - 32_768.0) / 32_768.0)),
        SampleFormat::I32 => {
            rms(as_i32(bytes).iter().map(|&s| s as f32 / 2_147_483_648.0))
        }
        SampleFormat::U32 => rms(as_u32(bytes)
            .iter()
            .map(|&s| (s as f32 - 2_147_483_648.0) / 2_147_483_648.0)),
        SampleFormat::I8 => rms(as_i8(bytes).iter().map(|&s| s as f32 / 128.0)),
        SampleFormat::U8 => {
            rms(as_u8(bytes).iter().map(|&s| (s as f32 - 128.0) / 128.0))
        }
        _ => {
            // Unknown newer format: treat as silence rather than panicking.
            0.0
        }
    }
}

fn rms<I>(samples: I) -> f32
where
    I: Iterator<Item = f32> + ExactSizeIterator,
{
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .map(|s| {
            let s = s.clamp(-1.0, 1.0);
            (s as f64) * (s as f64)
        })
        .sum();
    (sum_sq / n as f64).sqrt() as f32
}

fn as_f32(b: &[u8]) -> &[f32] {
    cast_slice(b)
}
fn as_f64(b: &[u8]) -> &[f64] {
    cast_slice(b)
}
fn as_i16(b: &[u8]) -> &[i16] {
    cast_slice(b)
}
fn as_u16(b: &[u8]) -> &[u16] {
    cast_slice(b)
}
fn as_i32(b: &[u8]) -> &[i32] {
    cast_slice(b)
}
fn as_u32(b: &[u8]) -> &[u32] {
    cast_slice(b)
}
fn as_i8(b: &[u8]) -> &[i8] {
    cast_slice(b)
}
fn as_u8(b: &[u8]) -> &[u8] {
    b
}

/// Reinterpret a byte slice as a typed sample slice via std `align_to`. cpal
/// guarantees the buffer is aligned and sized to whole samples for the active
/// format; if a backend ever hands us a malformed buffer we return an empty
/// slice (silence) rather than panicking inside the realtime callback.
fn cast_slice<T: Copy>(b: &[u8]) -> &[T] {
    // SAFETY: cpal guarantees the callback buffer is aligned and sized to whole
    // samples of the active format. We still guard the head/tail remainders:
    // a non-empty remainder means the buffer is malformed, so return silence.
    let (head, mid, tail) = unsafe { b.align_to::<T>() };
    if !head.is_empty() || !tail.is_empty() {
        return &[];
    }
    mid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(compute_rms(&[], SampleFormat::F32), 0.0);
    }

    #[test]
    fn rms_of_full_scale_f32_signal() {
        // alternating +/-1 => RMS == 1.0
        let samples: Vec<f32> = vec![1.0, -1.0, 1.0, -1.0];
        let bytes: Vec<u8> =
            samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let r = compute_rms(&bytes, SampleFormat::F32);
        assert!((r - 1.0).abs() < 1e-5, "got {r}");
    }

    #[test]
    fn rms_scales_with_amplitude() {
        let mk = |amp: f32| -> f32 {
            let samples: Vec<f32> = vec![amp, -amp, amp, -amp];
            let bytes: Vec<u8> =
                samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            compute_rms(&bytes, SampleFormat::F32)
        };
        assert!(mk(0.5) < mk(1.0));
        assert!((mk(0.5) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn rms_of_i16_normalizes() {
        // full-scale i16 +/-32767 ~ RMS 1.0
        let samples: Vec<i16> = vec![32_767, -32_767, 32_767, -32_767];
        let bytes: Vec<u8> =
            samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let r = compute_rms(&bytes, SampleFormat::I16);
        assert!(r > 0.99 && r <= 1.0, "got {r}");
    }
}
