# Rusty-Tuber

A high-performance **PNG-Tuber** written in Rust. Drive a PNG avatar from your
microphone volume and trigger emotions from a built-in web app, composited into
**OBS** as a transparent Browser Source.

- **Mic-driven mouth**: RMS loudness (with EMA smoothing) maps to four mouth
  states (`closed → slight → medium → open`).
- **Emotions that auto-revert**: trigger an emotion and it returns to your
  resting emotion after a configurable per-emotion timer (or stays until cleared).
- **Data-driven assets**: drop a folder of PNGs to add an emotion — no code.
  Partial frame sets are supported (a 2-frame emotion snaps to the nearest mouth).
- **Web control + REST API**: a built-in panel (`/`) for clicking emotions, plus
  a JSON WebSocket and REST endpoints for hotkeys / Stream Deck / automation.
- **Single self-contained binary**: the web UI is embedded; only the character
  PNGs live on disk.
- **Designed for OBS**: point a Browser Source at the *stage* URL for a
  transparent, zero-plugin overlay. Rust never decodes PNGs — OBS's browser
  engine does, so the hot path is trivially cheap.

---

## Quick start

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

---

## Asset layout

```
assets/characters/<character>/<emotion>/<mouth>.png
```

- `<emotion>` folders become emotion names (case-insensitive).
- Each emotion folder **must** contain `closed.png` and `open.png`.
- `slight.png` and `medium.png` are **optional**. If absent, the resolver snaps
  to the nearest available frame (e.g. an emotion with only `closed` + `open`
  uses `closed` for `slight` and `open` for `medium`).

```
assets/characters/default_macaw/
├── calm/
│   ├── closed.png
│   ├── slight.png
│   ├── medium.png
│   └── open.png
├── surprised/
│   ├── closed.png
│   └── open.png          (slight/medium optional → snaps to nearest)
└── angry/
    ├── closed.png
    └── open.png
```

PNGs should have an **alpha channel** (transparent background) so they composite
cleanly in OBS. Point `[engine].asset_root` at the character folder you want.

> Assets are loaded **once at startup** (per the chosen design). Restart the
> server to pick up newly added emotion folders.

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

[thresholds]               # RMS level that opens each mouth state. slight < medium < open.
slight = 0.02
medium = 0.08
open = 0.18

[engine]
default_emotion = "calm"   # Resting emotion.
asset_root = "./assets/characters/default_macaw"
bind = "127.0.0.1:8080"    # OBS Browser Source points at http://<bind>/stage.html

[timers]                   # Per-emotion auto-revert (seconds). Omit an emotion to make it stick.
surprised = 2.5
pleased = 3.0
laughing = 1.5
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
| `TriggerEmotion` | `{"emotion": "surprised"}` | Switch emotion; auto-revert on its timer. |
| `ClearOverride` | — | Return to the resting emotion now. |
| `SetDefault` | `{"emotion": "calm"}` | Change the resting emotion. |
| `SetMouthOverride` | `{"mouth": "open"}` | Force a mouth shape (ignores mic). |
| `ClearMouthOverride` | — | Resume mic-driven mouth. |
| `Hello` | — | Optional handshake (no-op). |

**Server → client:**

| type | payload |
|------|---------|
| `Welcome` | `{"catalog": {...}, "default_emotion": "calm"}` (on connect) |
| `StateUpdate` | `{"emotion", "mouth", "volume", "overridden", "frame", "default_emotion"}` |
| `Error` | `{"message": "..."}` |

`StateUpdate` is sent immediately on any visible change and throttled to ~20 Hz
for volume-only drift. `mouth` is one of `closed|slight|medium|open`; `frame` is
the resolved `/frames/...` URL to display.

### REST

| Method & path | Body | Effect |
|---|---|---|
| `GET  /api/catalog` | — | Emotions + frame map. |
| `GET  /api/state` | — | Latest `StateUpdate` snapshot. |
| `POST /api/emotion/:name` | — | Trigger an emotion. |
| `POST /api/clear` | — | Clear the override. |
| `POST /api/default/:name` | — | Set the resting emotion. |
| `POST /api/mouth/:mouth` | — | Force a mouth (`closed|slight|medium|open`). |
| `POST /api/mouth` | — | Release a forced mouth. |

Trigger an emotion from a hotkey / Stream Deck with plain HTTP:

```bash
curl -X POST http://127.0.0.1:8080/api/emotion/surprised
```

### Static routes

- `GET /` — control panel (embedded HTML/JS).
- `GET /stage.html` — OBS stage (transparent, bare avatar).
- `GET /frames/<emotion>/<file>.png` — character frames served from the asset root.

---

## How it works

```
mic/loopback ──cpal──▶ audio (RMS + EMA) ─┐
                                           ├─▶ state task (single owner)
web app ──WS/REST──▶ net ───────────────┘        │  effective (emotion, mouth, volume)
   ▲                                             ▼
   └── StateUpdate (debounced) ◀──────── net (broadcast)
                          │
                          ▼
   OBS Browser Source ◀─ HTTP (frames) + WS (state)
```

- **`config.rs`** — typed, validated `config.toml` parsing.
- **`assets.rs`** — catalog loader + nearest-frame fallback quantizer.
- **`audio.rs`** — cpal capture, lock-free RMS/EMA, `list-audio-devices`.
- **`state.rs`** — single async owner; token-race-safe revert timers (a stale
  timer can never clobber a newer emotion — the bug the original SDD design had).
- **`protocol.rs`** — serde message types + the shared `MouthState` enum.
- **`net.rs`** — axum router: embedded UI, `ServeDir` for frames, WS, REST,
  throttled broadcast, live snapshot for `/api/state`.

The hot path (audio callback) only computes an RMS and maybe sends one channel
message — no allocation, no locking, no image work. PNG decoding happens in
OBS's browser process.

---

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets        # 28 tests: unit + HTTP/WS integration
cargo machete                   # no unused deps
cargo audit                     # no known advisories
RUST_LOG=debug cargo run        # verbose logging
```

The integration test (`tests/integration.rs`) spins up the real router on an
ephemeral port against the bundled catalog and exercises the REST + WebSocket
contract, including the auto-revert timer.

## License

MIT OR Apache-2.0.
