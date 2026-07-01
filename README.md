# Rusty-Tuber

A high-performance **PNG-Tuber** written in Rust. Drive a layered PNG avatar
from your microphone volume and trigger eye-expression emotions from a built-in
web app. A Rust **compositor** renders every frame into a **virtual webcam**
that OBS, Zoom, Discord, and browsers all read as a normal camera.

- **Single universal output**: the server composites the avatar (static body +
  eye layer + mouth layer) into one RGBA frame on visible change and writes it
  to a v4l2loopback webcam. The panel previews that same camera; no duplicated
  rendering, no video streamed over the network.
- **Low-latency audio**: an asymmetric envelope follower (fast attack, gentle
  release) plus a low-latency buffer preset make the mouth feel instant. Attack
  and release are live-tunable from the panel.
- **Variable-FPS webcam**: 30 fps while talking, ~8 fps while idle, so silence
  doesn't burn CPU.
- **Layered compositing**: the avatar is a static body plus independent eye and
  mouth layers (same canvas, stacked like a South-Park cutout).
- **Mic-driven mouth**: RMS loudness maps to four mouth levels
  (`closed → partial → medium → open`). Tune the pick-up thresholds and toggle
  individual levels live from the panel — disable `closed` to make `partial` the
  resting mouth and A/B-test 3 vs 4 positions.
- **Natural blinking**: a randomised, tunable blink scheduler; add
  `eyes/closed.png` to enable it. Manual eye override too.
- **Emotions as eye expressions**: an emotion is an optional eye-expression set
  under `eyes/<emotion>/`; triggering it swaps the eye layer while the mouth
  keeps reacting to the mic. Auto-reverts on a per-emotion timer.
- **Web control + REST API**: a built-in panel (`/`) for triggering emotions,
  changing the resting face (★), driving overrides, and tuning the mouth, with
  keyboard shortcuts plus a JSON WebSocket and REST endpoints for hotkeys /
  Stream Deck / automation.
- **Single self-contained binary**: the web UI is embedded; only the character
  PNGs live on disk.

---

## Quick start

**Linux prerequisite:** install the ALSA development headers (cpal needs them
to build its audio backend):

```bash
sudo apt-get install -y libasound2-dev pkg-config   # Debian/Ubuntu
# Fedora: sudo dnf install -y alsa-lib-devel
```

macOS and Windows need no extra system packages (CoreAudio / WASAPI ship with the OS).

```bash
# 1. Build
cargo build --release

# 2. (optional) see which audio device cpal exposes
./target/release/rusty-tuber list-audio-devices

# 3. Run
./target/release/rusty-tuber --config config.toml
```

On startup you'll see something like:

```
INFO loaded asset catalog emotions=[] base=["base/body.png"]
INFO compositor ready width=921 height=921 layers=7
INFO auto-detected v4l2loopback device device=/dev/video2 name=Rusty-Tuber
INFO virtual webcam output started (BGR4) device=/dev/video2 fps=30 idle_fps=8
INFO server listening  bind=127.0.0.1:8080 panel=http://127.0.0.1:8080/
INFO starting audio capture  device=default latency=Low buffer=256 attack_ms=6 release_ms=110
```

Open the **panel** (`http://127.0.0.1:8080/`) in your browser, click **Enable
preview** to view the avatar, and watch the mouth react to your mic. The repo
ships placeholder art under `assets/characters/default_macaw/` — swap in your own.

### Virtual webcam setup (Linux v4l2loopback)

The avatar is written to a v4l2loopback device as a normal camera, so any app
that uses a webcam can read it. One-time setup on Ubuntu:

```bash
sudo apt-get install v4l2loopback-dkms v4l2loopback-utils
sudo modprobe v4l2loopback exclusive_caps=1 card_label="Rusty-Tuber"
```

The server auto-detects the device (or set `[webcam].device = "/dev/videoN"`).
Enable it in `config.toml` (`[webcam] enabled = true`) and (re)start.

### Add it to OBS / Zoom / Discord

1. In OBS: **Sources → add → Video Capture Device (V4L2)** → pick **Rusty-Tuber**.
2. Add a **Chroma Key** filter, key colour `#00ff00` (the `[webcam].background`).
3. Zoom/Discord/browsers: select **Rusty-Tuber** as the camera the same way.

(Webcams carry no alpha, so the avatar sits on the chroma background; the key
filter drops it. The panel preview shows the same camera via `getUserMedia`.)

### Control panel

Open `http://<bind>/` for the control panel. Beyond clicking emotions, it can:

- **Preview the avatar** — **Enable preview** opens the virtual camera in-browser.
- **Change the resting face** — click ★ on an emotion (sends `SetDefault`).
- **Force mouth / eyes** — override the mic or blink scheduler; `Auto` resumes.
- **Tune the mouth mapping** — the **Mouth tuning** card sets active levels and
  pick-up thresholds (strict-order sliders); uncheck `closed` to compare 3 vs 4
  mouth positions live.
- **Tune the audio response** — the **Audio response** card sets the envelope
  attack/release; lower attack = snappier open, higher release = smoother close.
- **Keyboard shortcuts** — `1`–`9` trigger the first nine emotions, `0` clears
  the override, `M` cycles the mouth, `E` cycles the eyes, `D` sets the current
  emotion as resting. A `<kbd>` badge on each button shows its shortcut.
- **Live volume meter** with a peak-hold marker, a connection status with
  reconnect countdown, and a Copy button for the stage URL.

The panel works on a phone or tablet (it stacks vertically on narrow screens),
so you can use it as a wireless Stream Deck on the same network. Errors and
disconnects surface as toasts rather than silently failing.

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

```
assets/characters/default_macaw/   (the bundled placeholder)
├── base/body.png                  (static macaw body)
├── mouths/closed.png
├── mouths/partial.png
├── mouths/medium.png
├── mouths/open.png
├── eyes/open.png                  (resting eyes)
└── eyes/closed.png                (blink)
```

> Assets are loaded **once at startup**. Restart the server to pick up newly
> added layers or emotions.

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
# Disable one to A/B-test fewer positions, e.g. 3:
#   enabled = ["partial", "medium", "open"]

[engine]
default_emotion = ""       # Resting emotion (eye-set). Empty = base/default eyes.
asset_root = "./assets/characters/default_macaw"
bind = "127.0.0.1:8080"    # Panel: http://<bind>/

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
fps = 30                   # Active rate while the avatar is moving.
idle_fps = 8              # Idle rate while static (saves CPU).
background = "#00ff00"     # #rrggbb chroma fill (webcams carry no alpha).
```

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

## Web API

### WebSocket — `GET /ws`

Text-only: control + the volume meter + config. The avatar video is **not** on
this socket — it comes from the virtual webcam (`getUserMedia` / Video Capture).
Messages use the envelope `{"type": "...", "payload": {...}}`.

**Client → server:**

| type | payload | effect |
|------|---------|--------|
| `TriggerEmotion` | `{"emotion": "happy"}` | Swap the eye layer to that expression; auto-revert on its timer. |
| `ClearOverride` | — | Return to the resting emotion now. |
| `SetDefault` | `{"emotion": "happy"}` | Change the resting emotion. |
| `SetMouthOverride` | `{"mouth": "open"}` | Force a mouth level (ignores mic). |
| `ClearMouthOverride` | — | Resume mic-driven mouth. |
| `SetMouthConfig` | `{"config": {enabled, partial, medium, open}}` | Set active levels + thresholds. |
| `SetEnvelope` | `{"config": {attack_ms, release_ms}}` | Set the audio envelope. |
| `SetEyesOverride` | `{"eyes": "closed"}` | Force eyes open/closed (pauses blinking). |
| `ClearEyesOverride` | — | Resume blinking. |
| `Hello` | — | Handshake (the panel sends it on connect). |

**Server → client:**

| type | payload |
|------|---------|
| `Welcome` | `{catalog, default_emotion, mouth_config, envelope, latency}` (on connect) |
| `StateUpdate` | `{emotion, mouth, eyes, volume, overridden, mouth_overridden, eyes_overridden, eyes_frame, mouth_frame, default_emotion}` |
| `MouthConfigUpdate` | `{config: {enabled, partial, medium, open}}` (broadcast when tuning changes) |
| `EnvelopeUpdate` | `{config: {attack_ms, release_ms}}` (broadcast when the envelope changes) |
| `Error` | `{message}` |

`StateUpdate` is sent immediately on any visible change and throttled to ~20 Hz
for volume-only drift. `mouth` is one of `closed|partial|medium|open`, `eyes` is
`open|closed`, and `eyes_frame` / `mouth_frame` are the resolved `/frames/...`
layer URLs to stack over the static `base` layer. `overridden` is true if any
override (emotion/mouth/eyes) is active; `mouth_overridden` / `eyes_overridden`
flag the per-channel overrides so every client (panel, OBS source, phone, Stream
Deck via REST) renders the same highlighted control without tracking local state.

Errors from the REST API are returned as a JSON envelope `{"error": "..."}`
with the appropriate status code (e.g. `404` for an unknown emotion, `400` for
an invalid mouth/eyes value).

### REST

| Method & path | Body | Effect |
|---|---|---|
| `GET  /api/health` | — | Liveness probe → `{"status":"ok"}`. |
| `GET  /api/catalog` | — | Layered asset catalog. |
| `GET  /api/state` | — | Latest `StateUpdate` snapshot. |
| `GET  /api/mouth-config` | — | Active mouth levels + thresholds. |
| `POST /api/mouth-config` | `{enabled, partial, medium, open}` | Update mouth levels + thresholds. |
| `GET  /api/envelope` | — | Audio envelope (attack/release). |
| `POST /api/envelope` | `{attack_ms, release_ms}` | Update the audio envelope. |
| `POST /api/emotion/:name` | — | Trigger an emotion. |
| `POST /api/clear` | — | Clear the override. |
| `POST /api/default/:name` | — | Set the resting emotion. |
| `POST /api/mouth/:mouth` | — | Force a mouth (`closed|partial|medium|open`). |
| `POST /api/mouth` | — | Release a forced mouth. |
| `POST /api/eyes/:state` | — | Force eyes (`open|closed`). |
| `POST /api/eyes` | — | Release a forced eye state (resume blinking). |

Trigger an emotion from a hotkey / Stream Deck with plain HTTP:

```bash
curl -X POST http://127.0.0.1:8080/api/emotion/surprised
```

### Static routes

- `GET /` — control panel (embedded HTML/JS).
- `GET /stage.html` — standalone browser viewer of the virtual camera.
- `GET /frames/<layer>/<file>.png` — character layers served from the asset root.

---

## How it works

```
mic/loopback ──cpal──▶ audio (RMS + asymmetric envelope) ─┐
                                                            ├─▶ state task (single owner)
panel/REST ──WS/HTTP──▶ net ──────────────────────────────┘     │  effective (emotion, mouth, eyes, volume)
                                                                 │  → COMPOSITOR renders one RGBA frame on visible change
   ┌─────────────────────────────────────────────────────────────┘  (watch channel)
   ▼
   webcam sink ──BGR4 + chroma bg──▶  /dev/videoN (v4l2loopback)
                                          │
   OBS / Zoom / Discord / panel ◀── read as a normal camera (+ Chroma Key)
```

- **`config.rs`** — typed, validated `config.toml` parsing (`[audio]`, `[blink]`,
  `[webcam]`, etc.).
- **`assets.rs`** — layered catalog loader (base/mouths/eyes + emotion eye-sets)
  + nearest-level fallback quantizer.
- **`audio.rs`** — cpal capture, asymmetric envelope follower, lock-free RMS,
  `list-audio-devices`.
- **`compositor.rs`** — decodes layers once, pre-composites the static base, and
  renders each frame via a skip-transparent alpha-over of the eye/mouth layers.
- **`state.rs`** — single async owner; resolves state, renders a frame on visible
  change, token-race-safe revert timers, the blink scheduler, and runtime
  mouth-config + envelope.
- **`webcam.rs`** *(Linux)* — v4l2loopback sink: composites over the chroma
  background, packs to BGR4, writes at a variable fps (30 active / ~8 idle);
  auto-detects the device.
- **`protocol.rs`** — serde message types, `MouthState`/`EyeState`, the layered
  `LayerCatalog`, `MouthConfig`, and `EnvelopeConfig`.
- **`net.rs`** — axum router: embedded panel, text-only control WS, REST,
  throttled broadcast, live snapshot for `/api/state`.

The hot path (audio callback) only computes an RMS and maybe sends one channel
message — no allocation, no locking, no image work. The callback body is wrapped
in `catch_unwind` so a panic can't unwind across the realtime → FFI boundary.
Avatar compositing/encoding happens off the audio thread, only on visible
change.

### Networking & security

The server is designed to bind to loopback for a single user on one machine.
The WebSocket upgrade rejects browser `Origin`s that aren't loopback or a
private-LAN address, so a random website can't open the live control channel and
drive your avatar; non-browser clients (OBS, curl, Stream Deck) send no `Origin`
and are allowed. Concurrent WebSocket clients are capped (default 16) and
inbound message size is bounded. Character frames are served with
`Cache-Control: public, max-age=3600` since they're immutable for the process
lifetime, keeping the first mouth/blink swap flicker-free.

---

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets        # unit + HTTP/WS integration
cargo machete                   # no unused deps
cargo audit                     # no known advisories
RUST_LOG=debug cargo run        # verbose logging
```

The integration test (`tests/integration.rs`) spins up the real router on an
ephemeral port against the bundled catalog and exercises the REST + WebSocket
contract, including the auto-revert timer and the blink/eyes override.

CI (`.github/workflows/rust.yml`) runs the same fmt + clippy + test gates on
every push and pull request.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion shall be dual-licensed as above.
