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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

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
    cmd_tx: UnboundedSender<StateCommand>,
) -> Result<Stream> {
    let device = pick_device(audio).context("selecting audio device")?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    let (base_config, sample_format) = negotiate_config(&device, audio)?;

    // Try a fixed buffer size first; fall back to the negotiated default.
    let preferred = audio.buffer_size.max(1);
    let mut fixed_config = base_config.clone();
    fixed_config.buffer_size = cpal::BufferSize::Fixed(preferred);
    fixed_config.sample_rate = SampleRate(audio.sample_rate);

    info!(
        device = %device_name,
        sample_rate = fixed_config.sample_rate.0,
        channels = fixed_config.channels,
        buffer = preferred,
        format = ?sample_format,
        smoothing = audio.smoothing_factor,
        mode = ?audio.mode,
        "starting audio capture"
    );

    let smoothing_init = audio.smoothing_factor.clamp(0.0, 1.0);
    let alpha = Arc::new(AtomicU32::new(smoothing_init.to_bits()));
    let smoothed: Arc<AtomicU32> = Arc::new(AtomicU32::new(0.0f32.to_bits()));

    let err_cb = |err| warn!(error = %err, "audio stream error");

    let build = |cfg: StreamConfig| {
        let smoothed = smoothed.clone();
        let alpha_v = f32::from_bits(alpha.load(Ordering::Relaxed));
        let cmd_tx = cmd_tx.clone();
        let fmt = sample_format;
        device.build_input_stream_raw(
            &cfg,
            fmt,
            move |data, _info| {
                let raw = compute_rms(data.bytes(), fmt);
                let prev = f32::from_bits(smoothed.load(Ordering::Relaxed));
                let next = alpha_v * raw + (1.0 - alpha_v) * prev;
                smoothed.store(next.to_bits(), Ordering::Relaxed);
                // UnboundedSender::send is synchronous and non-blocking.
                let _ = cmd_tx.send(StateCommand::SetVolume(next));
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
/// format, so the head/tail byte remainders are empty.
fn cast_slice<T: Copy>(b: &[u8]) -> &[T] {
    // SAFETY: cpal guarantees the callback buffer is aligned and sized to whole
    // samples of the active format. We assert the head/tail remainders are empty.
    let (head, mid, tail) = unsafe { b.align_to::<T>() };
    debug_assert!(head.is_empty() && tail.is_empty(), "unaligned cpal buffer");
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
