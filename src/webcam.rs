//! Virtual webcam sink (Linux v4l2loopback).
//!
//! Consumes the compositor's RGBA frame, composites the avatar over a chroma
//! background (the webcam is opaque — there is no alpha over video devices), and
//! writes BGR4 (32-bit BGRA) frames to a `/dev/videoN` v4l2loopback device at a
//! steady fps. BGR4 is v4l2loopback's native format and is trivially produced
//! from RGBA (swap R/B), so the output works in OBS/Chrome/Zoom/Discord as a
//! normal camera. For OBS, add a **Chroma Key** filter on this source to key
//! out the background colour and recover transparency; the transparent OBS
//! Browser Source remains available alongside.

#![cfg(target_os = "linux")]

use crate::compositor::Frame;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

// v4l2 constants we need (avoid pulling a full v4l2 binding crate).
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SRGB: u32 = 8;
// 32-bit BGRX (bytes in memory: B, G, R, X). This is v4l2loopback's native
// format and is what v4l2-ctl reports as 'BGR4'. Trivial to produce from RGBA
// (swap R/B) and needs no even-width padding.
const V4L2_PIX_FMT_BGR32: u32 = u32::from_le_bytes(*b"BGR4");

// VIDIOC_S_FMT = _IOWR('V', 5, struct v4l2_format). struct v4l2_format is 208
// bytes on 64-bit Linux (verified against the installed UAPI headers), so this
// resolves to 0xC0D05605. Built via the _IOC macro so the size encoding is
// explicit; if a future kernel changes the struct size this must move with it.
const VIDIOC_S_FMT: libc::c_ulong = ioc(3, b'V', 5, 208);

/// Build a Linux ioctl number (mirrors the _IOC macro).
const fn ioc(dir: u32, type_: u8, nr: u32, size: u32) -> libc::c_ulong {
    (((dir & 0x3) as libc::c_ulong) << 30)
        | (((size & 0x3FFF) as libc::c_ulong) << 16)
        | ((type_ as libc::c_ulong) << 8)
        | ((nr & 0xFF) as libc::c_ulong)
}

/// Find a v4l2loopback `/dev/videoN`. If `configured` is set, use it directly.
/// Otherwise scan `/sys/class/video4linux/`: a device is a candidate when its
/// `name` mentions v4l2loopback / "Dummy video device", OR it has no `device`
/// symlink (virtual devices have no physical bus, real webcams do). This catches
/// devices created with a custom `card_label` (e.g. `Rusty-Tuber`) too. Every
/// video device + its name is logged to help diagnose misses.
pub fn find_device(configured: &str) -> Option<PathBuf> {
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    let mut found: Option<PathBuf> = None;
    for n in 0..64 {
        let base = format!("/sys/class/video4linux/video{n}");
        let name_path = format!("{base}/name");
        let name = fs::read_to_string(&name_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let lname = name.to_ascii_lowercase();
        let is_loopback_name =
            lname.contains("v4l2loopback") || lname.contains("dummy video");
        // Real (hardware-backed) devices have a `device` symlink to their bus;
        // v4l2loopback devices don't.
        let has_bus = std::path::Path::new(&format!("{base}/device")).exists();
        let candidate = is_loopback_name || (!has_bus && !name.is_empty());
        info!(node = %format!("/dev/video{n}"), name = %name, has_bus, "video device");
        if candidate && found.is_none() {
            found = Some(PathBuf::from(format!("/dev/video{n}")));
        }
    }
    found
}

/// Open and configure the device for BGR4 output. The returned fd is kept for
/// the lifetime of the webcam loop.
fn open_device(
    path: &std::path::Path,
    width: u32,
    height: u32,
) -> Result<OwnedFd> {
    let path_str = path.to_str().unwrap_or("/dev/video0").to_owned();
    // SAFETY: opening a device path with O_RDWR; the fd is wrapped in OwnedFd.
    let fd = unsafe {
        libc::open(
            std::ffi::CString::new(path_str).unwrap().as_ptr(),
            libc::O_RDWR,
        )
    };
    if fd < 0 {
        anyhow::bail!(
            "opening webcam device {} (is v4l2loopback loaded? `sudo modprobe v4l2loopback`)",
            path.display()
        );
    }
    let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

    // Set the output format via VIDIOC_S_FMT. v4l2_format is a type + a union;
    // we zero a 208-byte buffer and write the v4l2_pix_format fields by offset
    // to avoid C-struct-layout pitfalls.
    let bytesperline = width * 4;
    let sizeimage = bytesperline * height;
    // v4l2_format = u32 type + 4 bytes padding (union is 8-byte aligned) + the
    // v4l2_pix_format union. Field offsets verified against the kernel headers.
    let mut fmt = [0u8; 208];
    write_u32(&mut fmt, 0, V4L2_BUF_TYPE_VIDEO_OUTPUT); // type
    write_u32(&mut fmt, 8, width); // pix.width
    write_u32(&mut fmt, 12, height); // pix.height
    write_u32(&mut fmt, 16, V4L2_PIX_FMT_BGR32); // pix.pixelformat
    write_u32(&mut fmt, 20, V4L2_FIELD_NONE); // pix.field
    write_u32(&mut fmt, 24, bytesperline); // pix.bytesperline
    write_u32(&mut fmt, 28, sizeimage); // pix.sizeimage
    write_u32(&mut fmt, 32, V4L2_COLORSPACE_SRGB); // pix.colorspace
                                                   // SAFETY: VIDIOC_S_FMT with a correctly-sized buffer is the standard
                                                   // driver ioctl contract; the pointer is valid and the size matches.
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            VIDIOC_S_FMT,
            fmt.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        anyhow::bail!(
            "VIDIOC_S_FMT failed on {} (errno {}) — device may not be a v4l2loopback output",
            path.display(),
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        );
    }
    Ok(fd)
}

fn write_u32(buf: &mut [u8], offset: usize, val: u32) {
    buf[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

/// Spawn the webcam render loop on a dedicated OS thread (keeps the per-frame
/// How long after the last visible change we keep writing at the active (high)
/// frame rate before dropping to the idle rate. Covers natural pauses between
/// words/sentences so the camera doesn't visibly stutter down to idle mid-talk.
const ACTIVE_WINDOW: Duration = Duration::from_millis(350);

/// Spawn the webcam render loop on a dedicated OS thread (keeps the per-frame
/// CPU + blocking write() off the async runtime). Exits when the frame channel
/// closes (server shutdown).
///
/// Runs at `fps` while the avatar is actively changing (and for `ACTIVE_WINDOW`
/// after the last change), then drops to `idle_fps` while static — keeping the
/// feed alive for consumers without burning CPU at full rate during silence.
pub fn spawn_webcam(
    frame_rx: watch::Receiver<Arc<Frame>>,
    device: PathBuf,
    fps: u32,
    idle_fps: u32,
    bg: [u8; 3],
) -> Result<()> {
    // Use the compositor frame's native dimensions (BGR4 is 32-bit/pixel, so no
    // even-width constraint).
    let initial = frame_rx.borrow().clone();
    let out_w = initial.width;
    let out_h = initial.height;
    let fd = open_device(&device, out_w, out_h).with_context(|| {
        format!("setting up webcam device {}", device.display())
    })?;
    info!(
        device = %device.display(),
        width = out_w,
        height = out_h,
        fps,
        idle_fps,
        "virtual webcam output started (BGR4); add a Chroma Key filter in OBS to key out the background"
    );

    std::thread::spawn(move || {
        let mut bgra: Vec<u8> =
            Vec::with_capacity((out_w as usize) * (out_h as usize) * 4);
        let active_int = Duration::from_secs_f32(1.0 / (fps.max(1) as f32));
        let idle_int = Duration::from_secs_f32(1.0 / (idle_fps.max(1) as f32));
        let mut last: Option<Arc<Frame>> = None;
        let mut active_until = Instant::now();
        let mut raw = FileFd(fd.as_raw_fd()); // borrows the fd; `fd` keeps it open
        loop {
            let now = Instant::now();
            let frame = frame_rx.borrow().clone(); // Arc clone — cheap
            let changed =
                last.as_ref().map_or(true, |l| !Arc::ptr_eq(l, &frame));
            if changed {
                last = Some(frame.clone());
                active_until = now + ACTIVE_WINDOW;
            }
            rgba_to_bgra(&frame, bg, &mut bgra);
            if raw.write_all(&bgra).is_err() {
                warn!("webcam write failed; continuing");
            }
            // Variable FPS: active rate while talking (and briefly after), idle
            // rate when static.
            let target = if now < active_until {
                active_int
            } else {
                idle_int
            };
            std::thread::sleep(target);
        }
        // `fd` (OwnedFd) drops here at thread exit, closing the device.
    });

    Ok(())
}

/// A thin wrapper to write to a raw fd (the v4l2loopback write() interface).
struct FileFd(RawFd);
impl Write for FileFd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: write() on an open fd; the buffer is valid for `buf.len()`.
        let n = unsafe {
            libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len())
        };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Composite the avatar over `bg` and pack to 32-bit BGRX (V4L2_PIX_FMT_BGR32,
/// memory layout B,G,R,X) — v4l2loopback's native format. Transparent avatar
/// areas become the chroma background so a consumer can key them out.
fn rgba_to_bgra(frame: &Frame, bg: [u8; 3], out: &mut Vec<u8>) {
    let n = (frame.width as usize) * (frame.height as usize);
    out.clear();
    out.resize(n * 4, 0);
    let rgba = &frame.rgba;
    let inv_bg = bg;
    for i in 0..n {
        let j = i * 4;
        let r = rgba[j] as u32;
        let g = rgba[j + 1] as u32;
        let b = rgba[j + 2] as u32;
        let a = rgba[j + 3] as u32;
        // alpha-over the chroma background.
        let (or, og, ob) = if a == 0 {
            (inv_bg[0] as u32, inv_bg[1] as u32, inv_bg[2] as u32)
        } else if a == 255 {
            (r, g, b)
        } else {
            let inv = 255 - a;
            (
                (a * r + inv * inv_bg[0] as u32 + 127) / 255,
                (a * g + inv * inv_bg[1] as u32 + 127) / 255,
                (a * b + inv * inv_bg[2] as u32 + 127) / 255,
            )
        };
        // BGRX byte order.
        out[j] = ob as u8;
        out[j + 1] = og as u8;
        out[j + 2] = or as u8;
        out[j + 3] = 0xFF;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_size_matches_and_packs_bgrx() {
        // 2x1 avatar: two opaque pixels → 2*4 bytes BGRX.
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![10, 20, 30, 255, 40, 50, 60, 255],
        };
        let mut out = Vec::new();
        rgba_to_bgra(&frame, [0, 0, 0], &mut out);
        assert_eq!(out.len(), 8);
        // First pixel RGB(10,20,30) → BGRX = [30,20,10,0xFF].
        assert_eq!(&out[0..4], &[30, 20, 10, 0xFF]);
    }

    #[test]
    fn chroma_bg_fills_transparent_areas() {
        // 1x1 transparent avatar over green bg → BGRX of green = [0,255,0,0xFF].
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
        };
        let mut out = Vec::new();
        rgba_to_bgra(&frame, [0, 255, 0], &mut out);
        assert_eq!(&out[..], &[0, 255, 0, 0xFF]);
    }

    #[test]
    fn ioc_ioctl_constant_matches_known_value() {
        // VIDIOC_S_FMT for a 208-byte struct v4l2_format on 64-bit Linux.
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
    }
}
