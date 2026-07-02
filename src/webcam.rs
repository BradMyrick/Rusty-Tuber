//! Virtual webcam sink (Linux v4l2loopback).
//!
//! Composites the compositor's RGBA frame over a chroma background (video
//! devices carry no alpha) and writes frames to a `/dev/videoN` v4l2loopback
//! device, so the avatar appears as a normal camera in OBS, browsers, and
//! conferencing apps. Consumers apply a chroma-key filter to drop the
//! background. The output pixel format (default `YUYV`, the one format every
//! browser / Cheese / Zoom / Meet accepts) and an advertised frame interval
//! (`VIDIOC_S_PARM`) are configurable; the interval lets OBS / ffplay lock
//! pacing and stops them from free-running and accumulating latency.

#![cfg(target_os = "linux")]

use crate::compositor::Frame;
use crate::config::WebcamFormat;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wide::u32x8;
// v4l2 constants we need (avoid pulling a full v4l2 binding crate).
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SRGB: u32 = 8;
// 32-bit BGRX (bytes in memory: B, G, R, X). This is v4l2loopback's native
// format and is what v4l2-ctl reports as 'BGR4'. Trivial to produce from RGBA
// (swap R/B) and needs no even-width padding. Kept as an opt-in because some
// capture stacks (notably Chrome/Firefox) reject BGR4-only devices.
const V4L2_PIX_FMT_BGR32: u32 = u32::from_le_bytes(*b"BGR4");
// YUYV (YUY2): packed 4:2:2 YCbCr, 2 bytes/pixel, horizontally subsampled
// chroma. This is the universally-accepted webcam format (every browser,
// Cheese, Zoom, Meet, OBS, Discord) and is half the bandwidth of BGR4, so it
// is the default. The width must be even.
const V4L2_PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
// streamparm capability flag: "driver supports timeperframe".
const V4L2_CAP_TIMEPERFRAME: u32 = 0x1000;

// VIDIOC_S_FMT = _IOWR('V', 5, struct v4l2_format). struct v4l2_format is 208
// bytes on 64-bit Linux (verified against the installed UAPI headers), so this
// resolves to 0xC0D05605. Built via the _IOC macro so the size encoding is
// explicit; if a future kernel changes the struct size this must move with it.
const VIDIOC_S_FMT: libc::c_ulong = ioc(3, b'V', 5, 208);

// VIDIOC_S_PARM = _IOWR('V', 22, struct v4l2_streamparm). struct v4l2_streamparm
// is 204 bytes on 64-bit Linux (verified against the UAPI headers →
// 0xC0CC5616). Used to advertise the frame interval (timeperframe = 1/fps) so
// readers (OBS, ffplay, browsers via ENUM_FRAMEINTERVALS) can lock pacing
// instead of free-running and accumulating latency. Field offsets verified:
// type=0, parm.capability=4, timeperframe.numerator=12, .denominator=16.
const VIDIOC_S_PARM: libc::c_ulong = ioc(3, b'V', 22, 204);

/// Build a Linux ioctl number (mirrors the _IOC macro).
const fn ioc(dir: u32, type_: u8, nr: u32, size: u32) -> libc::c_ulong {
    (((dir & 0x3) as libc::c_ulong) << 30)
        | (((size & 0x3FFF) as libc::c_ulong) << 16)
        | ((type_ as libc::c_ulong) << 8)
        | ((nr & 0xFF) as libc::c_ulong)
}

/// What to composite the transparent avatar onto before writing to the device.
///
/// `Solid` is the chroma-key colour (the classic OBS workflow: key out the green
/// in the consumer). `Image` is a full RGBA frame (same dimensions as the
/// avatar) for apps that don't chroma key — Google Meet / Discord use ML
/// segmentation, so a flat green shows through; a real background image looks
/// right with no keying.
pub enum Background {
    Solid([u8; 3]),
    Image(Vec<u8>),
}

impl Background {
    /// Resolve the configured background. An empty `image_path` → the solid
    /// chroma colour; otherwise load, decode, and Lanczos3-resample the image to
    /// `size`×`size` (matching the avatar frame). Errors if the image is missing
    /// or unreadable — the caller treats that as fatal (better than silently
    /// falling back to chroma and surprising the user).
    pub fn load(image_path: &str, solid: [u8; 3], size: u32) -> Result<Self> {
        if image_path.is_empty() {
            return Ok(Background::Solid(solid));
        }
        let path = std::path::Path::new(image_path);
        let img = image::open(path)
            .with_context(|| {
                format!("loading background image {}", path.display())
            })?
            .to_rgba8();
        if img.width() == 0 || img.height() == 0 {
            anyhow::bail!("background image {} is empty", path.display());
        }
        let orig = (img.width(), img.height());
        let scaled = if img.width() == size && img.height() == size {
            img
        } else {
            image::imageops::resize(
                &img,
                size,
                size,
                image::imageops::FilterType::Lanczos3,
            )
        };
        info!(
            path = %path.display(),
            ?orig,
            size,
            "background image loaded (avatar composited over it; no chroma key needed)"
        );
        Ok(Background::Image(scaled.into_raw()))
    }
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

/// Open and configure the device for output. The returned fd is kept for the
/// lifetime of the webcam loop. `format` selects the pixel format (YUYV default,
/// BGR4 opt-in); `fps` is advertised via `VIDIOC_S_PARM` so readers can lock
/// pacing.
fn open_device(
    path: &std::path::Path,
    width: u32,
    height: u32,
    format: WebcamFormat,
    fps: u32,
) -> Result<OwnedFd> {
    let path_str = path.to_str().unwrap_or("/dev/video0").to_owned();
    // O_NONBLOCK: a `write()` that can't be sunk right now (device/OBS not
    // ready) returns EAGAIN instead of blocking, so device flow-control shows
    // up as a dropped frame (smooth) rather than accumulating latency (laggy).
    // The fd is wrapped in OwnedFd.
    let fd = unsafe {
        libc::open(
            std::ffi::CString::new(path_str).unwrap().as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK,
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
    let (fourcc, bytesperline, sizeimage) = match format {
        WebcamFormat::Yuyv => {
            // YUYV is 2 bytes/pixel; width must be even (validated in config).
            (V4L2_PIX_FMT_YUYV, width * 2, width * height * 2)
        }
        WebcamFormat::Bgr4 => {
            (V4L2_PIX_FMT_BGR32, width * 4, width * height * 4)
        }
    };
    // v4l2_format = u32 type + 4 bytes padding (union is 8-byte aligned) + the
    // v4l2_pix_format union. Field offsets verified against the kernel headers.
    let mut fmt = [0u8; 208];
    write_u32(&mut fmt, 0, V4L2_BUF_TYPE_VIDEO_OUTPUT); // type
    write_u32(&mut fmt, 8, width); // pix.width
    write_u32(&mut fmt, 12, height); // pix.height
    write_u32(&mut fmt, 16, fourcc); // pix.pixelformat
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

    // Advertise the frame interval. Without this v4l2loopback reports
    // timeperframe = 1/-1 (0.000 fps), which makes OBS / ffplay free-run their
    // capture cadence and accumulate latency (the observed 200–700 ms), and
    // leaves VIDIOC_ENUM_FRAMEINTERVALS empty so browsers / Cheese skip the
    // device. We set the OUTPUT side's timeperframe = 1/fps; v4l2loopback
    // mirrors it to the CAPTURE side. Failure is non-fatal (the device still
    // works at the old latency) — we warn so a regression is visible.
    let mut parm = [0u8; 204];
    write_u32(&mut parm, 0, V4L2_BUF_TYPE_VIDEO_OUTPUT); // type
    write_u32(&mut parm, 4, V4L2_CAP_TIMEPERFRAME); // parm.capability
    write_u32(&mut parm, 12, 1); // timeperframe.numerator (1 frame)
    write_u32(&mut parm, 16, fps.max(1)); // timeperframe.denominator (per second)
                                          // SAFETY: VIDIOC_S_PARM with a 204-byte buffer (verified sizeof) is the
                                          // standard driver ioctl contract.
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            VIDIOC_S_PARM,
            parm.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        warn!(
            errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            "VIDIOC_S_PARM failed; device will advertise 0 fps — expect reader-side lag"
        );
    }

    Ok(fd)
}

fn write_u32(buf: &mut [u8], offset: usize, val: u32) {
    buf[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

/// Spawn the webcam loop on a dedicated thread.
///
/// Two write cadences, selected by `steady`:
///
/// - **`steady = true` (default, real-camera behaviour):** a frame is written at
///   a constant `fps` cadence regardless of avatar activity. This is **required
///   for OBS**: OBS paces its V4L2 source by frame timestamps, so an
///   event-driven writer that parks during silence makes OBS stall, then
///   backlog up to ~1 s when speech resumes (intermittent lag that spikes on
///   start/stop). A steady stream keeps OBS's queue shallow. Cost at 512² is
///   ~1–2 % CPU at idle.
/// - **`steady = false` (event-driven):** writes only when the compositor posts
///   a changed frame and parks at idle for ~zero CPU. Fine for WebRTC consumers
///   (Discord / Meet) that grab the latest frame, but triggers the OBS backlog.
///
/// The avatar is composited over `bg` (a chroma colour or a background image)
/// and written in `format` (default YUYV); the device advertises `fps` via
/// `VIDIOC_S_PARM`, and the compositor coalesces renders to the same rate.
pub fn spawn_webcam(
    frame_rx: watch::Receiver<Arc<Frame>>,
    device: PathBuf,
    bg: Background,
    format: WebcamFormat,
    fps: u32,
    steady: bool,
) -> Result<()> {
    let initial = frame_rx.borrow().clone();
    let out_w = initial.width;
    let out_h = initial.height;
    let fd =
        open_device(&device, out_w, out_h, format, fps).with_context(|| {
            format!("setting up webcam device {}", device.display())
        })?;
    info!(
        device = %device.display(),
        width = out_w,
        height = out_h,
        ?format,
        fps,
        steady,
        bg = match &bg {
            Background::Solid(c) => format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
            Background::Image(_) => "<image>".to_string(),
        },
        "virtual webcam output started"
    );

    std::thread::Builder::new()
        .name("webcam".into())
        .spawn(move || {
            // A current-thread runtime drives the `watch`/`interval` futures so
            // the loop can `await`. The blocking `write()` lives on this
            // dedicated thread, so it never stalls the async runtime.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("webcam runtime");
            rt.block_on(async move {
                // Worst-case capacity is the 4-byte/pixel BGR4 path; YUYV (2
                // bytes/pixel) reuses the same allocation at half-fill.
                let mut out: Vec<u8> =
                    Vec::with_capacity((out_w as usize) * (out_h as usize) * 4);
                let mut raw = FileFd(fd.as_raw_fd()); // borrows the fd; `fd` keeps it open
                let mut frame_rx = frame_rx;

                // Instrumentation: track write behaviour so we can see (via the
                // periodic log line) whether the device is keeping up. High skip
                // counts mean OBS/the device can't sink our rate; large write
                // times would mean the driver is blocking despite O_NONBLOCK.
                let mut written: u64 = 0;
                let mut skipped: u64 = 0;
                let mut max_write_us: u128 = 0;
                let mut since_summary: u64 = 0;
                let summary_every: u64 = 60;

                // Steady mode drives writes on a fixed `fps` interval (and uses
                // `has_changed` only to notice shutdown). Event mode blocks on
                // `changed()` and writes only on a new frame.
                let mut interval = if steady {
                    let mut iv = tokio::time::interval(std::time::Duration::from_secs_f32(
                        1.0 / fps.max(1) as f32,
                    ));
                    // Don't bunch after a stall: push the next tick out by the
                    // overrun instead of firing catch-up bursts.
                    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    Some(iv)
                } else {
                    None
                };

                // Mark the current (initial) value as seen so `changed()` only
                // fires for *future* posts, then fall through to write it so the
                // device has something to serve before the first change/tick.
                frame_rx.borrow_and_update();
                loop {
                    if let Some(iv) = interval.as_mut() {
                        iv.tick().await;
                        // Non-blocking shutdown probe: the frame_tx sender
                        // dropped (run() tearing down) → exit cleanly.
                        if frame_rx.has_changed().is_err() {
                            break;
                        }
                    } else if frame_rx.changed().await.is_err() {
                        break; // sender dropped -> shutdown
                    }

                    let frame = frame_rx.borrow().clone(); // Arc clone — cheap
                    match format {
                        WebcamFormat::Yuyv => rgba_to_yuyv(&frame, &bg, &mut out),
                        WebcamFormat::Bgr4 => rgba_to_bgra(&frame, &bg, &mut out),
                    }
                    let t0 = Instant::now();
                    // Single non-blocking write of one whole frame. On EAGAIN
                    // the device keeps serving the last good frame — skip
                    // rather than block, so latency can never accumulate at
                    // this boundary.
                    match raw.write(&out) {
                        Ok(n) => {
                            if n != out.len() {
                                // v4l2loopback writes whole frames; a partial
                                // write would split the boundary, so drop it.
                                warn!(
                                    wrote = n,
                                    expected = out.len(),
                                    "unexpected partial webcam write; dropping"
                                );
                                skipped += 1;
                            } else {
                                written += 1;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            skipped += 1;
                        }
                        Err(e) => {
                            warn!(error = %e, "webcam write failed; continuing");
                            skipped += 1;
                        }
                    }
                    let us = t0.elapsed().as_micros();
                    if us > max_write_us {
                        max_write_us = us;
                    }
                    since_summary += 1;
                    if since_summary >= summary_every {
                        debug!(
                            written,
                            skipped,
                            max_write_us,
                            "webcam write stats (last {} frames)",
                            since_summary
                        );
                        since_summary = 0;
                        max_write_us = 0;
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
/// memory layout B,G,R,X) — v4l2loopback's native format.
fn rgba_to_bgra(frame: &Frame, bg: &Background, out: &mut Vec<u8>) {
    match bg {
        Background::Solid(c) => rgba_to_bgra_solid(frame, *c, out),
        Background::Image(b) => rgba_to_bgra_image(frame, b, out),
    }
}

/// Solid-colour background path. Transparent avatar areas become the chroma
/// background so a consumer can key them out.
///
/// The 8-pixel-wide SIMD path uses an exact identity for division by 255
/// (`x/255 == (x + (x>>8) + 1) >> 8`, verified across `[0, 65025]`), so the
/// alpha-over blend is bit-exact while avoiding integer divides. Opaque and
/// transparent pixels collapse to src/bg under the same formula, so there are
/// no branches to mispredict.
fn rgba_to_bgra_solid(frame: &Frame, bg: [u8; 3], out: &mut Vec<u8>) {
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

/// Background-image path: alpha-over the avatar onto the corresponding pixel of
/// `bg` (a full RGBA frame, same dimensions, treated as opaque) and pack to
/// BGRX. Scalar (BGR4 is the opt-in format; the image workflow targets YUYV).
fn rgba_to_bgra_image(frame: &Frame, bg: &[u8], out: &mut Vec<u8>) {
    let n = (frame.width as usize) * (frame.height as usize);
    debug_assert!(bg.len() >= n * 4, "bg image must match frame dimensions");
    if out.len() != n * 4 {
        out.resize(n * 4, 0);
    }
    let rgba = &frame.rgba;
    let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;
    for i in 0..n {
        let j = i * 4;
        let r = rgba[j] as u32;
        let g = rgba[j + 1] as u32;
        let b = rgba[j + 2] as u32;
        let a = rgba[j + 3] as u32;
        let inv = 255 - a;
        // Avatar (with alpha) over the background image pixel (opaque).
        let or = div(a * r + inv * (bg[j] as u32));
        let og = div(a * g + inv * (bg[j + 1] as u32));
        let ob = div(a * b + inv * (bg[j + 2] as u32));
        out[j] = ob as u8;
        out[j + 1] = og as u8;
        out[j + 2] = or as u8;
        out[j + 3] = 0xFF;
    }
}

/// Composite the avatar over `bg`, convert to packed **YUYV (YUY2, 4:2:2)**, and
/// store in `out`. YUYV is the universally-accepted webcam format (every
/// browser, Cheese, Zoom, Meet, OBS, Discord) and is half the bandwidth of
/// BGR4.
fn rgba_to_yuyv(frame: &Frame, bg: &Background, out: &mut Vec<u8>) {
    match bg {
        Background::Solid(c) => rgba_to_yuyv_solid(frame, *c, out),
        Background::Image(b) => rgba_to_yuyv_image(frame, b, out),
    }
}

/// Solid-colour background path. Two horizontally-adjacent pixels share one
/// (Cb,Cr) pair (averaged), so `frame.width` must be even — enforced by config
/// validation.
///
/// Colour conversion is BT.601 full-range with integer fixed-point: the luma
/// coefficients (77,150,29) sum to 256 so pure white maps to Y=255 and black to
/// Y=0. The alpha-over blend reuses the exact /255 identity.
fn rgba_to_yuyv_solid(frame: &Frame, bg: [u8; 3], out: &mut Vec<u8>) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    debug_assert!(w % 2 == 0, "YUYV requires even width (config must enforce)");
    let size = w * h * 2;
    if out.len() != size {
        out.resize(size, 0);
    }
    let rgba = &frame.rgba;
    let (bg_r, bg_g, bg_b) = (bg[0] as u32, bg[1] as u32, bg[2] as u32);
    let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;

    // One macropixel (4 bytes: Y0,Cb,Y1,Cr) per pair of horizontal pixels.
    let mut o = 0usize;
    for y in 0..h {
        let row = y * w;
        for x in (0..w).step_by(2) {
            let mut lum = [0u8; 2];
            let mut cb = [0i32; 2];
            let mut cr = [0i32; 2];
            for p in 0..2 {
                let j = (row + x + p) * 4;
                let (r8, g8, b8) =
                    (rgba[j] as u32, rgba[j + 1] as u32, rgba[j + 2] as u32);
                let a = rgba[j + 3] as u32;
                let inv = 255 - a;
                let r = div(a * r8 + inv * bg_r) as i32;
                let g = div(a * g8 + inv * bg_g) as i32;
                let b = div(a * b8 + inv * bg_b) as i32;
                // BT.601 full-range; coeffs sum to 256.
                lum[p] = ((77 * r + 150 * g + 29 * b + 128) >> 8) as u8;
                cb[p] = (((-43 * r - 84 * g + 127 * b + 128) >> 8) + 128)
                    .clamp(0, 255);
                cr[p] = (((127 * r - 106 * g - 21 * b + 128) >> 8) + 128)
                    .clamp(0, 255);
            }
            let cb_avg = ((cb[0] + cb[1] + 1) >> 1) as u8;
            let cr_avg = ((cr[0] + cr[1] + 1) >> 1) as u8;
            out[o] = lum[0];
            out[o + 1] = cb_avg;
            out[o + 2] = lum[1];
            out[o + 3] = cr_avg;
            o += 4;
        }
    }
}

/// Background-image path: same YUYV packing as the solid path, but each pixel's
/// backdrop comes from `bg` (a full RGBA frame, same dimensions, treated as
/// opaque) instead of a flat colour. Used for background images in Meet /
/// Discord / browsers where chroma keying isn't available.
fn rgba_to_yuyv_image(frame: &Frame, bg: &[u8], out: &mut Vec<u8>) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    debug_assert!(w % 2 == 0, "YUYV requires even width");
    debug_assert!(
        bg.len() >= w * h * 4,
        "bg image must match frame dimensions"
    );
    let size = w * h * 2;
    if out.len() != size {
        out.resize(size, 0);
    }
    let rgba = &frame.rgba;
    let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;

    let mut o = 0usize;
    for y in 0..h {
        let row = y * w;
        for x in (0..w).step_by(2) {
            let mut lum = [0u8; 2];
            let mut cb = [0i32; 2];
            let mut cr = [0i32; 2];
            for p in 0..2 {
                let j = (row + x + p) * 4;
                let (r8, g8, b8) =
                    (rgba[j] as u32, rgba[j + 1] as u32, rgba[j + 2] as u32);
                let a = rgba[j + 3] as u32;
                let inv = 255 - a;
                // Avatar (with alpha) over the background image pixel (opaque).
                let r = div(a * r8 + inv * (bg[j] as u32)) as i32;
                let g = div(a * g8 + inv * (bg[j + 1] as u32)) as i32;
                let b = div(a * b8 + inv * (bg[j + 2] as u32)) as i32;
                lum[p] = ((77 * r + 150 * g + 29 * b + 128) >> 8) as u8;
                cb[p] = (((-43 * r - 84 * g + 127 * b + 128) >> 8) + 128)
                    .clamp(0, 255);
                cr[p] = (((127 * r - 106 * g - 21 * b + 128) >> 8) + 128)
                    .clamp(0, 255);
            }
            let cb_avg = ((cb[0] + cb[1] + 1) >> 1) as u8;
            let cr_avg = ((cr[0] + cr[1] + 1) >> 1) as u8;
            out[o] = lum[0];
            out[o + 1] = cb_avg;
            out[o + 2] = lum[1];
            out[o + 3] = cr_avg;
            o += 4;
        }
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
        rgba_to_bgra(&frame, &Background::Solid([0, 0, 0]), &mut out);
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
        rgba_to_bgra(&frame, &Background::Solid([0, 255, 0]), &mut out);
        assert_eq!(&out[..], &[0, 255, 0, 0xFF]);
    }

    #[test]
    fn ioc_ioctl_constant_matches_known_value() {
        // VIDIOC_S_FMT for a 208-byte struct v4l2_format on 64-bit Linux.
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
        // VIDIOC_S_PARM for a 204-byte struct v4l2_streamparm on 64-bit Linux
        // (_IOWR('V', 22, ...)). Verified against the installed UAPI header.
        assert_eq!(VIDIOC_S_PARM, 0xC0CC_5616);
    }

    #[test]
    fn yuyv_size_is_width_height_times_two() {
        // 4x2 avatar → 4*2*2 = 16 bytes of YUYV (2 bytes/pixel).
        let frame = Frame {
            width: 4,
            height: 2,
            rgba: vec![0; 4 * 2 * 4],
        };
        let mut out = Vec::new();
        rgba_to_yuyv(&frame, &Background::Solid([0, 0, 0]), &mut out);
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn yuyv_black_and_white_endpoints() {
        // One 2x1 macropixel: opaque black then opaque white over black bg.
        // White must map to Y=255, black to Y=0; shared chroma is neutral-ish.
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
        };
        let mut out = Vec::new();
        rgba_to_yuyv(&frame, &Background::Solid([0, 0, 0]), &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 0, "Y0 black");
        assert_eq!(out[2], 255, "Y1 white");
        // Pure grey/neutral chroma for black+white averages to 128.
        assert_eq!(out[1], 128, "Cb neutral");
        assert_eq!(out[3], 128, "Cr neutral");
    }

    #[test]
    fn yuyv_transparent_pixel_falls_back_to_chroma_bg() {
        // 2x1 fully transparent avatar over green bg → both pixels are green
        // (0,255,0). Green's Y ≈ 150, so both luma bytes should match and the
        // shared chroma must be consistent with green.
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 0, 0, 0, 0, 0],
        };
        let mut out = Vec::new();
        rgba_to_yuyv(&frame, &Background::Solid([0, 255, 0]), &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], out[2], "identical pixels → identical luma");
        let y_green = ((150 * 255 + 128) >> 8) as u8;
        assert_eq!(out[0], y_green);
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
        rgba_to_bgra(&frame, &Background::Solid(bg), &mut out);

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

    /// Helper: build a solid-colour RGBA background image of the given size.
    fn solid_image(w: u32, h: u32, c: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for _ in 0..(w * h) {
            buf.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
        buf
    }

    #[test]
    fn yuyv_image_matches_solid_when_image_is_uniform_colour() {
        // A background IMAGE filled with colour C must produce byte-identical
        // output to the Solid(C) path — the image path is the generalisation.
        let w = 16u32;
        let h = 2u32;
        let n = (w * h) as usize;
        let mut rgba = vec![0u8; n * 4];
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
        let c = [10, 20, 30];

        let mut out_solid = Vec::new();
        rgba_to_yuyv(&frame, &Background::Solid(c), &mut out_solid);
        let mut out_image = Vec::new();
        rgba_to_yuyv(
            &frame,
            &Background::Image(solid_image(w, h, c)),
            &mut out_image,
        );
        assert_eq!(out_solid, out_image);
    }

    #[test]
    fn bgra_image_matches_solid_when_image_is_uniform_colour() {
        // Same equivalence property for the BGR4 path (image variant vs SIMD
        // solid variant must agree).
        let w = 16u32;
        let h = 2u32;
        let n = (w * h) as usize;
        let mut rgba = vec![0u8; n * 4];
        let patterns = [
            [255, 0, 0, 255],
            [0, 0, 0, 0],
            [255, 255, 255, 128],
            [0, 0, 255, 128],
            [123, 200, 50, 200],
            [10, 20, 30, 1],
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
        let c = [200, 100, 50];

        let mut out_solid = Vec::new();
        rgba_to_bgra(&frame, &Background::Solid(c), &mut out_solid);
        let mut out_image = Vec::new();
        rgba_to_bgra(
            &frame,
            &Background::Image(solid_image(w, h, c)),
            &mut out_image,
        );
        assert_eq!(out_solid, out_image);
    }

    #[test]
    fn background_load_empty_path_is_solid() {
        let bg = Background::load("", [1, 2, 3], 512).unwrap();
        assert!(matches!(bg, Background::Solid([1, 2, 3])));
    }
}
