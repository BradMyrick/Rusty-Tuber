# Rusty-Tuber

A headless, high-performance **PNG-Tuber** written in Rust. Drive a layered PNG
avatar from your microphone volume (plus optional blinks and animations) and
composite every frame into a **virtual webcam** (`v4l2loopback`) that OBS,
Zoom, Discord, and browsers all read as a normal camera.

- **No web UI, no server.** Pure Rust: mic → avatar → webcam. A tiny stdin
  command interface is the control seam for hotkeys / scripts.
- **Single universal output.** The compositor renders the avatar (static body +
  eye layer + mouth layer + animation overlays) into one RGBA frame on visible
  change and writes it to a v4l2loopback device. Add a Chroma Key filter in the
  consumer to drop the background.
- **Low-latency audio.** An asymmetric envelope follower (fast attack, gentle
  release) plus a low-latency buffer preset make the mouth feel instant.
- **Camera-style webcam.** By default the device emits a constant `[webcam].fps`
  stream (like a real webcam) — required for OBS, which paces its V4L2 source by
  frame timestamps and otherwise stalls/backlogs. Output is `YUYV` by default
  (works in OBS, browsers, Cheese, Zoom, Meet, Discord), with `VIDIOC_S_PARM`
  advertising the frame rate so readers lock pacing.
- **Layered compositing.** The avatar is a static body plus independent eye and
  mouth layers (same canvas, stacked like a South-Park cutout).
- **Mic-driven mouth.** RMS loudness maps to four mouth levels
  (`closed → partial → medium → open`), tuned in `config.toml`.
- **Natural blinking.** A randomised, tunable blink scheduler; add
  `eyes/closed.png` to enable it.
- **Emotions as eye expressions.** An emotion is an optional eye-expression set
  under `eyes/<emotion>/`; triggering it swaps the eye layer while the mouth
  keeps reacting to the mic. Auto-reverts on a per-emotion timer.
- **Single self-contained binary.** Only the character PNGs live on disk.

---

## Quick start

**Linux prerequisite:** install the ALSA development headers (cpal needs them
to build its audio backend), plus `v4l2loopback` for the virtual webcam:

```bash
sudo apt-get install -y libasound2-dev pkg-config v4l2loopback-dkms v4l2loopback-utils
# Fedora: sudo dnf install -y alsa-lib-devel v4l2loopback
```

macOS and Windows need no extra system packages for audio, but the virtual
webcam output is **Linux-only** (`[webcam]` is ignored elsewhere).

```bash
# 1. Build
cargo build --release

# 2. (optional) see which audio device cpal exposes
./target/release/rusty-tuber list-audio-devices

# 3. Run (type `help` on stdin for commands; Ctrl-C to quit)
./target/release/rusty-tuber --config config.toml
```

On startup you'll see something like:

```
INFO loaded asset catalog emotions=[] base=["base/body.png"]
INFO loaded animation group group=worms instances=5 frames=2
INFO compositor ready width=2048 height=2048 layers=4 anim_instances=5
INFO auto-detected v4l2loopback device device=/dev/video2 name=Rusty-Tuber
INFO virtual webcam output started device=/dev/video2 width=512 height=512 format=Yuyv fps=30
INFO audio capture running
INFO control interface ready on stdin — type `help` for commands (Ctrl-C to quit)
INFO rusty-tuber running headless; type `help` on stdin for commands, Ctrl-C to quit
```

The repo ships two characters under `assets/characters/`: `wbc` (the configured
default — a white-blood-cell with squirming worms) and `default_macaw` (a simple
demo). Swap `asset_root` in `config.toml` to switch.

### Virtual webcam setup (Linux v4l2loopback)

The avatar is written to a v4l2loopback device as a normal camera, so any app
that uses a webcam can read it. One-time setup on Ubuntu:

```bash
sudo apt-get install v4l2loopback-dkms v4l2loopback-utils
sudo modprobe v4l2loopback exclusive_caps=1 card_label="Rusty-Tuber" video_nr=2
```

> **Use `exclusive_caps=1`.** With it, the device advertises CAPTURE-only caps
> while a writer is active, which is what OBS / desktop Zoom / Meet expect from
> the `write()` output path Rusty-Tuber uses.
>
> **Browsers (Chrome/Firefox) and Cheese/Guvcview need the device actively
> producing when they enumerate.** With `exclusive_caps=1` the device only shows
> up once Rusty-Tuber is running and has set the format — so start Rusty-Tuber
> *first*, then (re)start/refresh the browser or camera app. Rusty-Tuber also
> advertises a frame rate via `VIDIOC_S_PARM` (`[webcam].fps`) and outputs `YUYV`
> by default, both of which are required for browsers/Cheese to accept the
> device; setting `[webcam].format = "bgr4"` opts out (faster, but Chrome/Firefox
> won't see it).

Rusty-Tuber auto-detects the device (or set `[webcam].device = "/dev/videoN"`).
Enable it in `config.toml` (`[webcam] enabled = true`) and (re)start.

#### Make the module load on every boot

`modprobe` only lasts until reboot. To make the `v4l2loopback` device come back
automatically with your label/options, drop two small files in (the installer
can create them for you — see `scripts/`):

```bash
# Load the module at boot:
echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf

# Set the options (exclusive_caps, label, fixed /dev/videoN):
echo 'options v4l2loopback exclusive_caps=1 card_label="Rusty-Tuber" video_nr=2' \
  | sudo tee /etc/modprobe.d/v4l2loopback.conf
```

After the next reboot, `/dev/video2` will exist with the right name/caps with no
manual `modprobe`. (If `video_nr=2` collides with another device, pick a free
number and update `[webcam].device` to match.)

### Add it to OBS / Zoom / Discord

1. In OBS: **Sources → add → Video Capture Device (V4L2)** → pick **Rusty-Tuber**.
2. Set **Frame rate** to match `[webcam].fps` (30) and **Buffering → None** for
   the lowest-latency preview (Buffering = Auto can add a few frames of delay).
   Keep `[webcam].steady = true` (the default) — OBS paces its V4L2 source by
   frame timestamps, so an idle-parking writer makes OBS stall, then backlog up
   to ~1 s when you resume talking. The steady stream keeps the queue shallow.
3. Pick a background strategy:
   - **Chroma key (OBS):** leave `background_image` empty, keep
     `background = "#00ff00"`, and add a **Chroma Key** filter (key `#00ff00`).
   - **Background image (Meet/Discord/Zoom):** set
     `background_image = "assets/backgrounds/<name>.png"` — the avatar is
     composited over the image, no keying needed. (Meet/Discord blur/replacement
     use ML segmentation, not a colour key, so a flat green shows through.)
4. Zoom/Discord/browsers: select **Rusty-Tuber** as the camera the same way.

---

## Control interface (stdin)

Rusty-Tuber runs **headless** — there is no web UI. Drive emotions and overrides
by typing commands on stdin (or piping them in from a hotkey daemon / script).
Commands are case-insensitive; blank lines are ignored.

| command | effect |
|---------|--------|
| `emotion <name>` | Trigger an eye-expression set; auto-reverts on its `[timers]` timer. |
| `clear` | Drop the emotion override; return to the resting face. |
| `default <name>` | Change the resting emotion. |
| `mouth <closed\|partial\|medium\|open>` | Force a mouth level (ignores mic). |
| `mouth auto` | Resume mic-driven mouth. |
| `eyes <open\|closed>` | Force eyes (pauses blinking). |
| `eyes auto` | Resume blinking. |
| `help` / `?` | Show the command list. |
| `quit` / `exit` | Shut down. |

Examples:

```bash
# interactive: type into the running terminal
emotion surprised
mouth open
eyes auto

# scripted / hotkey daemon: pipe commands in
echo "emotion happy" | ./rusty-tuber --config config.toml
```

### Embedding / scripting

The stdin reader is just one frontend over a single `mpsc` channel of
[`state::StateCommand`](src/state.rs) values. A Stream Deck / hotkey daemon /
embedding program can feed the same channel directly, and subscribe to the
broadcast channel of [`protocol::ServerMessage`](src/protocol.rs) to observe
avatar state. See [`src/control.rs`](src/control.rs) and
[`src/lib.rs`](src/lib.rs).

---

## Asset layout (layered)

The avatar is a stack of independent transparent PNG **layers**, all sharing the
same canvas size, composited bottom-up like a South-Park cutout. The body stays
static; only the mouth and eye layers swap.

```
assets/characters/<character>/
├── base/
│   └── *.png            one or more static body images (stacked bottom-up, in
│                         filename order). At least one is required.
├── mouths/
│   ├── closed.png       mouth level 0 — resting          (required)
│   ├── partial.png      level 1                           (optional)
│   ├── medium.png       level 2                           (optional)
│   └── open.png         level 3 — fully open              (required)
└── eyes/
    ├── open.png         resting eyes                      (required)
    ├── closed.png       eyes-closed (blink)               (optional)
    └── <emotion>/       optional eye-expression sets (see below)
        ├── open.png     this emotion's resting eyes       (required)
        └── closed.png   this emotion's blink eyes         (optional)
```

- **Mouths** are driven by mic volume (`closed → partial → medium → open`).
  `closed` and `open` are required; `partial`/`medium` are optional and the
  resolver snaps to the nearest available level (e.g. a 2-frame set uses
  `closed` for `partial` and `open` for `medium`).
- **Eyes** blink between `open.png` and `closed.png`. If `closed.png` is absent
  that expression simply doesn't blink (the `open` frame is reused).
- **Emotions** are optional named eye-expression sets under `eyes/<emotion>/`.
  Triggering an emotion swaps the **eye layer** to that expression; the mouth
  keeps reacting to the mic. With no emotion folders, the avatar is just base +
  blink + mic mouth. Add an emotion later by dropping in
  `eyes/happy/open.png` (+ optional `closed.png`).

PNGs must have an **alpha channel** so the layers composite cleanly. Point
`[engine].asset_root` at the character folder.

#### Custom animations (`character.toml`)

A character can define independent animated overlay channels beyond the
eye/mouth layers. Each `[[anim]]` group cycles frame PNGs on a per-instance,
randomised timer (e.g. the `wbc` worms). Drop a `character.toml` in the
character root:

```toml
# One [[anim]] block per independent animation group.
[[anim]]
name = "worms"                 # folder under anim/<name>/
driver = "random_cycle"        # only "random_cycle" (independent per-instance)
instances = 5                  # how many independent copies run at once
frames = 2                     # frame count per instance
file_pattern = "worm{n}-{f}.png"  # {n} = instance index (1..instances),
                                  # {f} = frame index (1..frames)
min_interval = 0.08            # seconds between frame advances (randomised)
max_interval = 0.35
```

with frame PNGs at `anim/worms/worm1-1.png`, `worm1-2.png`, `worm2-1.png`, …
The compositor precomputes each frame's opaque bounding box at load, so a small
sprite on a large canvas costs only its occupied pixels per render. Animation
overlays are hidden while the mouth is open (talking) and return at rest.

```
assets/characters/wbc/        (the configured default)
├── base/body.png
├── mouths/{closed,medium,open}.png   (no partial, no eyes — worms carry the motion)
├── character.toml
└── anim/worms/worm{1..5}-{1,2}.png
```

> Assets are loaded **once at startup**. Restart to pick up newly added layers
> or emotions.

### Regenerating the placeholder art

The bundled placeholder PNGs were generated with
`scripts/gen_placeholder_assets.py`:

```bash
python3 scripts/gen_placeholder_assets.py   # needs Pillow
```

---

## Configuration (`config.toml`)

```toml
[audio]
sample_rate = 44100        # Target rate (cpal negotiates; not guaranteed).
latency = "low"            # "low" (~256-frame buffers, ~6ms) | "stable" (~1024, ~23ms).
# buffer_size = 256        # Optional explicit override; omit to use the latency preset.
attack_ms = 6              # Envelope attack — mouth-open responsiveness. Smaller = snappier.
release_ms = 110           # Envelope release — mouth-close smoothness. Larger = less flutter.
mode = "input"             # "input" (mic) | "loopback" (system output)
device = ""                # "" = system default; else a name from --list-audio-devices.

[thresholds]               # RMS level that opens each mouth level. partial < medium < open.
partial = 0.02
medium = 0.08
open = 0.18
# Optional: active mouth levels (default = all four). Lowest enabled = resting.
# Disable one to use fewer positions, e.g. 3:
#   enabled = ["partial", "medium", "open"]

[engine]
default_emotion = ""       # Resting emotion (eye-set). Empty = base/default eyes.
asset_root = "./assets/characters/wbc"

[timers]                   # Per-emotion auto-revert (seconds). Empty by default.

[blink]                    # Eye-blink scheduler. Optional — all keys have defaults.
enabled = true
min_interval = 2.5         # Seconds between blinks (randomised in [min, max]).
max_interval = 6.0
duration = 0.12            # Seconds the eyes stay closed per blink.
double_chance = 0.15       # Probability of a quick double-blink.

[webcam]                   # Virtual webcam (Linux v4l2loopback).
enabled = true
device = ""                # "/dev/videoN", or "" to auto-detect.
output_size = 512          # Square output edge (px). Layers scale to this at load
                           # and the whole pipeline runs at this size — cost scales
                           # with pixels, so this is the biggest perf lever. 512 is
                           # plenty for a webcam source OBS scales anyway. Must be
                           # even when format = "yuyv".
background = "#00ff00"     # #rrggbb chroma fill (webcams carry no alpha).
format = "yuyv"            # "yuyv" (default; works in OBS, browsers, Cheese, Zoom,
                           # Meet, Discord — half the bandwidth of BGR4) | "bgr4"
                           # (faster to produce but Chrome/Firefox reject it).
fps = 30                   # Advertised device frame rate (VIDIOC_S_PARM) and the
                           # compositor's render-rate cap. Lets OBS / ffplay / web
                           # clients lock pacing instead of free-running (which at
                           # 0 fps causes 200–700 ms reader-side lag). 24 = film
                           # look, 60 = smoother motion at higher CPU.
steady = true              # Constant-fps output even when idle (default on).
                           # REQUIRED for OBS: an event-driven writer that parks
                           # during silence makes OBS stall then backlog up to ~1 s
                           # when you resume talking (intermittent lag, worst on
                           # start/stop). ~1–2 % CPU at 512². Set false only for
                           # WebRTC-only (Discord/Meet) where zero idle CPU matters.
background_image = ""      # Optional background image composited behind the avatar
                           # (overrides the flat chroma `background`). A path
                           # relative to the working dir, e.g.
                           # "assets/backgrounds/background1.png". For apps that
                           # can't chroma key (Google Meet / Discord blur use ML
                           # segmentation, not a colour key, so flat green shows
                           # through). "" = solid `background` (the OBS chroma path).
```

> **Why `fps` matters.** v4l2loopback reports `0.000 fps` until the writer calls
> `VIDIOC_S_PARM`. With no frame interval, OBS/ffplay can't lock pacing and
> accumulate latency (smooth but ~half a second behind), while browsers/Cheese
> reject the device entirely. Rusty-Tuber advertises `[webcam].fps` and renders at
> the same rate, so producer and consumer stay in lockstep.
>
> **Backgrounds vs chroma key.** A flat `background = "#00ff00"` + a Chroma Key
> filter (OBS) is the classic transparent-background workflow. But Google Meet,
> Discord, and Zoom blur/replacement use **ML segmentation**, not a colour key, so
> a flat green shows through as green — set `background_image` to composite a real
> image behind the avatar instead (no keying needed). Drop your images in
> `assets/backgrounds/`.

If `[audio].device` is empty, `mode = "input"` uses the system default mic and
`mode = "loopback"` uses the first device whose name contains `monitor`.

### Audio notes (Linux / PipeWire)

`rusty-tuber` uses `cpal`, whose Linux backend is ALSA. On a PipeWire system it
sees a few generic devices (`pipewire`, `pulse`, `default`):

- **Mic**: `mode = "input"` works out of the box (the default device).
- **Loopback** (react to system/game audio): PipeWire sink monitors must be
  exposed as capture devices. If none appears in `list-audio-devices`, create one
  and route it, e.g.:
  ```bash
  pw-loopback -m '[Capture]' &
  ```
  then set `mode = "loopback"` (or name the device explicitly with `device =`).

Run `rusty-tuber list-audio-devices` any time to see the current options.

---

## How it works

```
mic/loopback ──cpal──▶ audio (RMS + asymmetric envelope) ─┐
                                                            ├─▶ state task (single owner)
stdin ──text commands──▶ control ──────────────────────────┘     │  effective (emotion, mouth, eyes, volume)
                                                                  │  → COMPOSITOR renders one RGBA frame on visible change
   ┌──────────────────────────────────────────────────────────────┘  (watch channel)
   ▼
   webcam sink ──YUYV/BGR4 + chroma bg──▶  /dev/videoN (v4l2loopback)
                                          │
   OBS / Zoom / Discord ◀── read as a normal camera (+ Chroma Key)
```

- **`config.rs`** — typed, validated `config.toml` parsing (`[audio]`, `[blink]`,
  `[webcam]`, etc.).
- **`assets.rs`** — layered catalog loader (base/mouths/eyes + emotion eye-sets)
  + nearest-level fallback quantizer.
- **`audio.rs`** — cpal capture, asymmetric envelope follower, lock-free RMS,
  `list-audio-devices`.
- **`compositor.rs`** — decodes layers once (caching each one's opaque bounding
  box), scales them to `[webcam].output_size` (Lanczos3, one-time), pre-composites
  the static base, and renders each frame via a bounding-box-cropped alpha-over
  of the eye/mouth/anim layers (sparse layers touch only their occupied pixels).
- **`state.rs`** — single async owner; resolves state and posts a lightweight
  `RenderRequest` to a dedicated render thread (coalesced to `[webcam].fps`,
  since the webcam can't consume more). Token-race-safe revert timers, the blink
  scheduler, and runtime mouth-config + envelope.
- **`webcam.rs`** *(Linux)* — v4l2loopback sink: alpha-overs the avatar onto a
  chroma colour **or** a background image with an RGBA→YUYV (or BGR4) convert,
  advertises the frame interval via `VIDIOC_S_PARM`, and writes a steady
  `fps` stream (default) so OBS can't stall/backlog — opening the device
  `O_NONBLOCK` so a busy reader surfaces as a *dropped* frame (smooth) instead
  of *blocking* (laggy). `steady = false` restores event-driven, zero-idle-CPU
  behaviour for WebRTC-only setups. Auto-detects the device.
- **`control.rs`** — dependency-free stdin command reader; the seam for hotkeys
  / scripts.
- **`protocol.rs`** — avatar domain types (`MouthState`/`EyeState`, the layered
  `LayerCatalog`, `MouthConfig`, `EnvelopeConfig`) plus `ServerMessage`, the
  broadcast-channel observation type for embedding code.

The hot path (audio callback) only computes an RMS and maybe sends one channel
message — no allocation, no locking, no image work. The callback body is wrapped
in `catch_unwind` so a panic can't unwind across the realtime → FFI boundary.
Avatar compositing happens off the audio thread on a dedicated render thread,
coalesced to the webcam's consumption rate.

### Performance & profiling

The pipeline cost scales with `[webcam].output_size`² (composite, alpha-over
blend, and device write all touch every output pixel), so this is the dominant
perf lever: 512² is ~16× cheaper than 2048² native art and is plenty for a
webcam source that OBS scales anyway. At 512² a frame is render ~0.35 ms +
write ~0.17 ms on a modest box — ~1% of the 30 ms budget.

Built-in stage timings are logged at `debug` level every 60 frames — run with
`RUST_LOG=debug` to see them:

```
DEBUG render stats (last 60 frames) renders=300 max_render_us=339 budget_us=30000
DEBUG webcam write stats (last 60 frames) written=300 skipped=0 max_write_us=162
```

If you ever see `max_render_us` approach `budget_us`, or `skipped` climbing
(reader can't keep up), that's the smoking gun. Otherwise the pipeline is
healthy and any perceived lag is downstream of `/dev/videoN`. Confirm the device
advertises a real frame rate — `v4l2-ctl --device=/dev/video2 --get-parm` should
show `Frames per second: 30.000`, not `0.000` — then isolate with
`ffplay /dev/video2` (or `mpv av://v4l2:/dev/video2`). If ffplay is snappy but
OBS lags, it's OBS-side: set the V4L2 source **Buffering → None** and **Frame
rate** to match `[webcam].fps`. (Before the `VIDIOC_S_PARM` fix the device
reported `0.000 fps`, which made *both* OBS and ffplay free-run and accumulate
200–700 ms — if that ever recurs, check `[webcam].fps` and the startup log.)

For a CPU flamegraph, the repo ships a `[profile.profiling]` (release opts +
symbols + frame pointers):

```bash
sudo sysctl -w kernel.perf_event_paranoid=1   # one-time: let perf sample
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile=profiling
perf record -F 999 -g -p $(pidof rusty-tuber) -- sleep 10
perf report --no-children      # or: perf script | inferno-flamegraph > out.svg
```

---

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets        # unit tests (config/assets/state/compositor/webcam/protocol/audio/control)
cargo deny check                # advisories + licenses + bans (CI runs this)
RUST_LOG=debug cargo run        # verbose logging
```

CI (`.github/workflows/rust.yml`) runs fmt + clippy + test on every push and
pull request, plus a weekly `cargo-deny` supply-chain audit (advisories,
licenses, bans, sources).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion shall be dual-licensed as above.
