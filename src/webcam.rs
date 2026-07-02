//! Virtual webcam sink (Linux v4l2loopback).
//!
//! Composites the compositor's RGBA frame over a chroma background (video
//! devices carry no alpha) and writes BGR4 frames to a `/dev/videoN`
//! v4l2loopback device, so the avatar appears as a normal camera in OBS,
//! browsers, and conferencing apps. Consumers apply a chroma-key filter to drop
//! the background.

#![cfg(target_os = "linux")]

use crate::compositor::Frame;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wide::u32x8;
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

/// Resolve the webcam device: an explicit `configured` path, else auto-detect.
///
/// Auto-detection scans `/sys/class/video4linux/` and picks the first device
/// whose `name` mentions v4l2loopback / "Dummy video", or — failing that — any
/// named device with no `device` symlink (virtual devices have no physical bus,
/// real webcams do). This recognises devices created with a custom `card_label`.
pub fn find_device(configured: &str) -> Option<PathBuf> {
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }

    let entries = fs::read_dir("/sys/class/video4linux")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut found: Option<(PathBuf, String)> = None;
    for dir in entries {
        let Some(node) = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.strip_prefix("video"))
        else {
            continue;
        };
        let name = fs::read_to_string(dir.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let lname = name.to_ascii_lowercase();
        let named_loopback =
            lname.contains("v4l2loopback") || lname.contains("dummy video");
        let has_bus = dir.join("device").exists();
        let candidate = named_loopback || (!has_bus && !name.is_empty());

        debug!(node = %format!("/dev/video{node}"), %name, has_bus, "video device");
        if candidate && found.is_none() {
            found = Some((PathBuf::from(format!("/dev/video{node}")), name));
        }
    }

    if let Some((dev, name)) = &found {
        info!(device = %dev.display(), name = %name, "auto-detected v4l2loopback device");
    }
    found.map(|(d, _)| d)
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

/// Spawn the webcam loop on a dedicated thread.
///
/// **Event-driven:** the loop blocks on the compositor's `watch` channel and
/// writes a frame the instant one is produced — no polling, so there is no
/// detection latency and a static avatar burns ~zero CPU (the thread parks
/// until the next mouth/blink/animation change). v4l2loopback keeps serving the
/// last written frame to every reader, so silence costs nothing. The output
/// rate is whatever the compositor produces (it coalesces to ~33 fps), so there
/// is no separate frame-rate knob.
pub fn spawn_webcam(
    frame_rx: watch::Receiver<Arc<Frame>>,
    device: PathBuf,
    bg: [u8; 3],
) -> Result<()> {
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
        "virtual webcam output started (BGR4)"
    );

    std::thread::Builder::new()
        .name("webcam".into())
        .spawn(move || {
            // A current-thread runtime drives the `watch` future so the loop can
            // `await` new frames. The blocking `write()` to the device lives on
            // this dedicated thread, so it never stalls the async runtime (which
            // has no other tasks anyway).
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("webcam runtime");
            rt.block_on(async move {
                let mut bgra: Vec<u8> =
                    Vec::with_capacity((out_w as usize) * (out_h as usize) * 4);
                let mut last: Option<Arc<Frame>> = None;
                let mut raw = FileFd(fd.as_raw_fd()); // borrows the fd; `fd` keeps it open
                let mut frame_rx = frame_rx;

                // Mark the current (initial) value as seen so `changed()` only
                // fires for *future* posts, then fall through to write it so the
                // device has something to serve before the first change.
                frame_rx.borrow_and_update();
                loop {
                    let frame = frame_rx.borrow().clone(); // Arc clone — cheap
                    let changed =
                        last.as_ref().map_or(true, |l| !Arc::ptr_eq(l, &frame));
                    if changed {
                        last = Some(frame.clone());
                        rgba_to_bgra(&frame, bg, &mut bgra);
                        if raw.write_all(&bgra).is_err() {
                            warn!("webcam write failed; continuing");
                        }
                    }
                    // Block until the compositor posts a new frame. At idle this
                    // parks with zero wakeups; the sender dropping = shutdown.
                    if frame_rx.changed().await.is_err() {
                        break;
                    }
                }
            });
            // `fd` (OwnedFd) drops here at thread exit, closing the device.
        })
        .expect("spawn webcam thread");

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
///
/// The 8-pixel-wide SIMD path uses an exact identity for division by 255
/// (`x/255 == (x + (x>>8) + 1) >> 8`, verified across `[0, 65025]`), so the
/// alpha-over blend is bit-exact while avoiding integer divides. Opaque and
/// transparent pixels collapse to src/bg under the same formula, so there are
/// no branches to mispredict.
fn rgba_to_bgra(frame: &Frame, bg: [u8; 3], out: &mut Vec<u8>) {
    let n = (frame.width as usize) * (frame.height as usize);
    // Size the buffer once; reuse across frames (every byte is overwritten by
    // the blend below, so no per-frame memset).
    if out.len() != n * 4 {
        out.resize(n * 4, 0);
    }
    let rgba = &frame.rgba;

    // Process 8 pixels per iteration as packed little-endian u32 lanes:
    //   pixel bytes [r, g, b, a] -> u32 = r | g<<8 | b<<16 | a<<24
    let bg_r = bg[0] as u32;
    let bg_g = bg[1] as u32;
    let bg_b = bg[2] as u32;
    let one = u32x8::splat(1);
    let ff = u32x8::splat(255);
    let bg_rv = u32x8::splat(bg_r);
    let bg_gv = u32x8::splat(bg_g);
    let bg_bv = u32x8::splat(bg_b);
    let chunks = n / 8;
    for c in 0..chunks {
        let base = c * 8;
        let mut px = [0u32; 8];
        for (k, slot) in px.iter_mut().enumerate() {
            let j = (base + k) * 4;
            *slot = u32::from_le_bytes([
                rgba[j],
                rgba[j + 1],
                rgba[j + 2],
                rgba[j + 3],
            ]);
        }
        let v = u32x8::from(px);
        let a = v >> 24;
        let b = (v >> 16) & ff;
        let g = (v >> 8) & ff;
        let r = v & ff;
        let inv = ff - a;
        // Exact /255 via (sum + (sum>>8) + 1) >> 8.
        let blend = |s: u32x8, bgv: u32x8| -> u32x8 {
            let sum = a * s + inv * bgv;
            (sum + (sum >> 8) + one) >> 8
        };
        let or = blend(r, bg_rv);
        let og = blend(g, bg_gv);
        let ob = blend(b, bg_bv);
        // Pack to BGRX: bytes [ob, og, or, 0xFF] -> ob | og<<8 | or<<16 | X<<24.
        let out_v: u32x8 = ob | (og << 8) | (or << 16) | (ff << 24);
        let res: [u32; 8] = out_v.to_array();
        for (k, val) in res.iter().enumerate() {
            let j = (base + k) * 4;
            out[j..j + 4].copy_from_slice(&val.to_le_bytes());
        }
    }

    // Scalar tail for the remaining pixels.
    for i in (chunks * 8)..n {
        let j = i * 4;
        let r = rgba[j] as u32;
        let g = rgba[j + 1] as u32;
        let b = rgba[j + 2] as u32;
        let a = rgba[j + 3] as u32;
        let inv = 255 - a;
        let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;
        let or = div(a * r + inv * bg_r);
        let og = div(a * g + inv * bg_g);
        let ob = div(a * b + inv * bg_b);
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

    #[test]
    fn simd_blend_matches_scalar_for_semi_transparent() {
        // 16x1 frame (>8 pixels so the SIMD body + scalar tail both run) with
        // a mix of alpha values. The SIMD path must bit-match the reference
        // alpha-over formula `(a*src + inv*bg) / 255` (floor) for every pixel.
        let w = 16u32;
        let h = 1u32;
        let n = (w * h) as usize;
        let mut rgba = vec![0u8; n * 4];
        // Pixel 0: fully opaque red. Pixel 1: fully transparent. Pixel 2:
        // half-alpha white over green. Pixel 3: alpha 128 blue over black.
        // Remaining pixels cycle through varied alpha to exercise the SIMD body.
        let patterns = [
            [255, 0, 0, 255],
            [0, 0, 0, 0],
            [255, 255, 255, 128],
            [0, 0, 255, 128],
            [123, 200, 50, 200],
            [10, 20, 30, 1],
            [250, 240, 230, 64],
            [0, 0, 0, 254],
        ];
        for i in 0..n {
            let p = patterns[i % patterns.len()];
            rgba[i * 4..i * 4 + 4].copy_from_slice(&p);
        }
        let frame = Frame {
            width: w,
            height: h,
            rgba,
        };
        let bg = [10, 20, 30];
        let mut out = Vec::new();
        rgba_to_bgra(&frame, bg, &mut out);

        for i in 0..n {
            let j = i * 4;
            let (r, g, b, a) = (
                out[j + 2] as u32, // BGRX: out[2]=or
                out[j + 1] as u32, // out[1]=og
                out[j] as u32,     // out[0]=ob
                frame.rgba[j + 3] as u32,
            );
            let sr = frame.rgba[j] as u32;
            let sg = frame.rgba[j + 1] as u32;
            let sb = frame.rgba[j + 2] as u32;
            let inv = 255 - a;
            let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;
            let er = div(a * sr + inv * bg[0] as u32);
            let eg = div(a * sg + inv * bg[1] as u32);
            let eb = div(a * sb + inv * bg[2] as u32);
            assert_eq!((r, g, b), (er, eg, eb), "pixel {i} alpha {a}");
            assert_eq!(out[j + 3], 0xFF, "X channel must be saturated");
        }
    }
}
