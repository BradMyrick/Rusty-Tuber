# Rusty-Tuber

A high-performance **PNG-Tuber** written in Rust. Drive a layered PNG avatar
from your microphone volume and trigger eye-expression emotions from a built-in
web app, composited into **OBS** as a transparent Browser Source.

- **Layered compositing**: the avatar is a static body plus independent eye and
  mouth layers (same canvas, stacked like a South-Park cutout). Only the mouth
  and eyes swap — the body never reloads, so there's no flicker.
- **Mic-driven mouth**: RMS loudness (with EMA smoothing) maps to four mouth
  levels (`closed → partial → medium → open`). Tune the pick-up thresholds and
  toggle individual levels live from the panel's **Mouth tuning** card — disable
  `closed` to make `partial` the resting mouth and A/B-test 3 vs 4 positions.
- **Natural blinking**: a randomised, fully tunable blink scheduler; add
  `eyes/closed.png` to enable it. Manual eye override too.
- **Emotions as eye expressions**: an emotion is an optional eye-expression set
  under `eyes/<emotion>/`; triggering it swaps the eye layer while the mouth
  keeps reacting to the mic. Auto-reverts on a per-emotion timer.
- **Data-driven assets**: drop in layered PNGs — no code. Partial mouth sets are
  supported (a 2-frame set snaps to the nearest level).
- **Web control + REST API**: a built-in panel (`/`) for triggering emotions,
  changing the resting face (★), and driving overrides, with keyboard shortcuts
  plus a JSON WebSocket and REST endpoints for hotkeys / Stream Deck / automation.
- **Single self-contained binary**: the web UI is embedded; only the character
  PNGs live on disk.
- **Designed for OBS**: point a Browser Source at the *stage* URL for a
  transparent, zero-plugin overlay. Rust never decodes PNGs — OBS's browser
  engine does, so the hot path is trivially cheap.

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
INFO loaded asset catalog emotions=["calm", "laughing", "pleased", "surprised"]
INFO server listening; add the stage URL as an OBS Browser Source
     bind=127.0.0.1:8080 panel=http://127.0.0.1:8080/ stage=http://127.0.0.1:8080/stage.html
INFO audio capture running  device=default sample_rate=44100
```

Open the **panel** (`http://127.0.0.1:8080/`) in your browser to click emotions
and watch the mouth react to your mic. The repo ships placeholder art under
`assets/characters/default_macaw/` so it works immediately — swap in your own.

### Add it to OBS

1. In OBS: **Sources → add → Browser**.
2. URL: `http://127.0.0.1:8080/stage.html`
3. Width/Height: match your PNG (the placeholder art is 512×512).
4. Tick **"Refresh browser when scene becomes active"** if you like.
5. The background is transparent — the avatar composites over your scene.

> Use `stage.html` (bare avatar, transparent) for OBS, and `/` (control panel)
> in your own browser. They share the same live state over WebSocket.

### Control panel

Open `http://<bind>/` for the control panel. Beyond clicking emotions, it can:

- **Change the resting face** — click the ★ on any emotion to make it the
  resting emotion (sends `SetDefault`). No need to edit `config.toml`.
- **Force mouth / eyes** — override the mic or blink scheduler; `Auto` resumes.
- **Tune the mouth mapping** — the **Mouth tuning** card (toggle to show) sets
  which levels are active and the pick-up threshold for each. Sliders enforce
  strict order (two can't be equal); uncheck `closed` to make `partial` the
  resting mouth and compare 3 vs 4 positions in real time. Changes broadcast to
  every panel and the running avatar.
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
buffer_size = 1024         # Preferred frames per buffer.
smoothing_factor = 0.35    # EMA weight on the newest RMS sample (0..1).
mode = "input"             # "input" (mic) | "loopback" (system output)
device = ""                # "" = system default; else a name from --list-audio-devices.

[thresholds]               # RMS level that opens each mouth level. partial < medium < open.
partial = 0.02
medium = 0.08
open = 0.18
# Optional: active mouth levels (default = all four). The lowest enabled is the
# resting mouth. Disable one to A/B-test fewer positions, e.g. 3:
#   enabled = ["partial", "medium", "open"]
# Also adjustable at runtime from the panel's "Mouth tuning" card.

[engine]
default_emotion = ""       # Resting emotion (eye-set). Empty = base/default eyes.
                           # If set, must name a folder under eyes/<name>/.
asset_root = "./assets/characters/default_macaw"
bind = "127.0.0.1:8080"    # OBS Browser Source points at http://<bind>/stage.html

[timers]                   # Per-emotion auto-revert (seconds). Omit an emotion to make it stick.
                           # Empty by default — add entries when you add emotion eye-sets.

[blink]                    # Eye-blink scheduler. Optional — all keys have defaults.
enabled = true             # Set false to disable blinking.
min_interval = 2.5         # Seconds between blinks (randomised in [min, max]).
max_interval = 6.0
duration = 0.12            # Seconds the eyes stay closed per blink.
double_chance = 0.15       # Probability of a quick double-blink.
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

Messages use the envelope `{"type": "...", "payload": {...}}`.

**Client → server:**

| type | payload | effect |
|------|---------|--------|
| `TriggerEmotion` | `{"emotion": "happy"}` | Swap the eye layer to that expression; auto-revert on its timer. |
| `ClearOverride` | — | Return to the resting emotion now. |
| `SetDefault` | `{"emotion": "happy"}` | Change the resting emotion. |
| `SetMouthOverride` | `{"mouth": "open"}` | Force a mouth level (ignores mic). |
| `ClearMouthOverride` | — | Resume mic-driven mouth. |
| `SetMouthConfig` | `{"config": {enabled, partial, medium, open}}` | Set active levels + pick-up thresholds (validated). |
| `SetEyesOverride` | `{"eyes": "closed"}` | Force eyes open/closed (pauses blinking). |
| `ClearEyesOverride` | — | Resume blinking. |
| `Hello` | — | Optional handshake (the built-in panel sends it on connect). |

**Server → client:**

| type | payload |
|------|---------|
| `Welcome` | `{"catalog": {base, mouths, default_eyes, emotions}, "default_emotion": "", "mouth_config": {enabled, partial, medium, open}}` (on connect) |
| `StateUpdate` | `{"emotion", "mouth", "eyes", "volume", "overridden", "mouth_overridden", "eyes_overridden", "eyes_frame", "mouth_frame", "default_emotion"}` |
| `MouthConfigUpdate` | `{"config": {enabled, partial, medium, open}}` (broadcast when the tuning changes) |
| `Error` | `{"message": "..."}` |

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
- `GET /stage.html` — OBS stage (transparent; stacks base/eyes/mouth layers).
- `GET /frames/<layer>/<file>.png` — character layers served from the asset root.

---

## How it works

```
mic/loopback ──cpal──▶ audio (RMS + EMA) ─┐
                                           ├─▶ state task (single owner)
web app ──WS/REST──▶ net ───────────────┘        │  effective (emotion, mouth, eyes, volume)
   ▲                                             │  → resolves eye + mouth layer URLs
   └── StateUpdate (eyes_frame, mouth_frame) ◀── net (broadcast)
                          │
                          ▼
   OBS Browser Source ◀─ HTTP (layers) + WS (state)
                         base / eyes / mouth stacked in the browser
```

- **`config.rs`** — typed, validated `config.toml` parsing (incl. `[blink]`).
- **`assets.rs`** — layered catalog loader (base/mouths/eyes + emotion eye-sets)
  + nearest-level fallback quantizer for partial mouth sets.
- **`audio.rs`** — cpal capture, lock-free RMS/EMA, `list-audio-devices`.
- **`state.rs`** — single async owner; resolves the current eye/mouth layer
  URLs, token-race-safe revert timers (a stale timer can never clobber a newer
  emotion — the bug the original SDD design had), plus the randomised blink
  scheduler.
- **`protocol.rs`** — serde message types + the shared `MouthState`/`EyeState`
  and the layered `LayerCatalog`.
- **`net.rs`** — axum router: embedded UI, `ServeDir` for layers, WS, REST,
  throttled broadcast, live snapshot for `/api/state`.

The hot path (audio callback) only computes an RMS and maybe sends one channel
message — no allocation, no locking, no image work. The callback body is wrapped
in `catch_unwind` so a panic can't unwind across the realtime → FFI boundary, and
PNG decoding happens in OBS's browser process.

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
