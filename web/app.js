// Rusty-Tuber web client (control panel). The Rust server composites the avatar
// and writes it to a virtual webcam (v4l2loopback) — the SINGLE video output.
// This panel reads that webcam back like any camera app (getUserMedia) for a
// preview, and uses a text WebSocket only for control + the volume meter +
// config (mouth levels, audio envelope). All avatar logic lives in Rust.

const mode = document.body.dataset.mode;
const isPanel = mode === "panel";

const previewVideo = document.getElementById("preview-video");
const previewBtn = document.getElementById("preview-btn");

const els = isPanel
  ? {
      dot: document.getElementById("dot"),
      connText: document.getElementById("conn-text"),
      meterFill: document.getElementById("meter-fill"),
      meterPeak: document.getElementById("meter-peak"),
      emotions: document.getElementById("emotions"),
      mouth: document.getElementById("mouth"),
      eyes: document.getElementById("eyes"),
      clearBtn: document.getElementById("clear"),
      toast: document.getElementById("toast"),
      tuningToggle: document.getElementById("tuning-toggle"),
      tuning: document.getElementById("tuning"),
      mouthLevels: document.getElementById("mouth-levels"),
      thresholds: document.getElementById("thresholds"),
      envelope: document.getElementById("envelope"),
      latencyMode: document.getElementById("latency-mode"),
    }
  : null;

let state = {
  catalog: null, // { base, mouths, default_eyes, emotions }
  defaultEmotion: "",
  emotion: "",
  mouth: "closed",
  eyes: "open",
  overridden: false,
  mouthOverridden: false,
  eyesOverridden: false,
  // Mouth-level config (active levels + thresholds) + audio envelope, mirrored
  // from the server and pushed back on change.
  mouthConfig: { enabled: [true, true, true, true], partial: 0.02, medium: 0.08, open: 0.18 },
  envelope: { attack_ms: 6, release_ms: 110 },
};

let toastTimer = null;
const MOUTH_KEYS = ["closed", "partial", "medium", "open"];

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}

// --- Webcam preview: read the virtual camera back via getUserMedia ---------
// Read the virtual camera back like any webcam. Camera access needs a user
// gesture + permission, hence the button. We open the default camera first so
// the preview always works, then try to switch to the Rusty-Tuber device.
async function enablePreview() {
  if (!navigator.mediaDevices?.getUserMedia) {
    showToast("Camera API unavailable (needs HTTPS or localhost).");
    return;
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: false });
  } catch (e) {
    showToast("Camera access denied or unavailable: " + (e?.message || e));
    return;
  }
  previewVideo.srcObject = stream;
  previewBtn.hidden = true;

  // Prefer the Rusty-Tuber device if the default wasn't it. Don't drop the
  // working stream until the switch succeeds.
  try {
    const cams = await navigator.mediaDevices.enumerateDevices();
    const rt = cams.find((d) => d.kind === "videoinput" && (d.label || "").includes("Rusty-Tuber"));
    const current = stream.getVideoTracks()[0]?.label;
    if (rt && rt.label !== current) {
      const better = await navigator.mediaDevices.getUserMedia({
        video: { deviceId: { exact: rt.deviceId } },
        audio: false,
      });
      stream.getTracks().forEach((t) => t.stop());
      previewVideo.srcObject = better;
    }
  } catch {
    // Keep the default camera; just note the switch didn't take.
    showToast("Showing default camera (couldn't switch to Rusty-Tuber).");
  }
}

function send(msg) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(msg));
    return true;
  }
  showToast("Disconnected — command ignored.");
  return false;
}

const triggerEmotion = (e) => send({ type: "TriggerEmotion", payload: { emotion: e } });
const clearOverride = () => send({ type: "ClearOverride" });
const setDefault = (e) => send({ type: "SetDefault", payload: { emotion: e } });
const setMouth = (m) => send({ type: "SetMouthOverride", payload: { mouth: m } });
const clearMouth = () => send({ type: "ClearMouthOverride" });
const setEyes = (e) => send({ type: "SetEyesOverride", payload: { eyes: e } });
const clearEyes = () => send({ type: "ClearEyesOverride" });

// --- Mouth tuning (levels + thresholds) -----------------------------------
// All three sliders share a 0–100 scale; dragging one carries its neighbours so
// partial<medium<open always holds (two can never be equal) and thumbs never
// rescale away from the pointer.
let mouthConfigTimer = null;

function scheduleSetMouthConfig() {
  if (mouthConfigTimer) clearTimeout(mouthConfigTimer);
  mouthConfigTimer = setTimeout(() => {
    mouthConfigTimer = null;
    send({ type: "SetMouthConfig", payload: { config: state.mouthConfig } });
  }, 120);
}

// --- Audio envelope (attack/release) sliders -------------------------------
let envelopeTimer = null;
function scheduleSetEnvelope() {
  if (envelopeTimer) clearTimeout(envelopeTimer);
  envelopeTimer = setTimeout(() => {
    envelopeTimer = null;
    send({ type: "SetEnvelope", payload: { config: state.envelope } });
  }, 120);
}

function renderEnvelope() {
  if (!isPanel) return;
  els.envelope.innerHTML = "";
  const add = (key, label, min, max, step) => {
    const row = document.createElement("div");
    row.className = "threshold";
    const lab = document.createElement("span");
    lab.textContent = label;
    const range = document.createElement("input");
    range.type = "range";
    range.min = String(min);
    range.max = String(max);
    range.step = String(step);
    const val = document.createElement("span");
    val.className = "tv";
    row.appendChild(lab);
    row.appendChild(range);
    row.appendChild(val);
    els.envelope.appendChild(row);
    const update = () => {
      const v = Number(range.value);
      state.envelope[key] = v;
      val.textContent = v + " ms";
    };
    range.addEventListener("input", () => { update(); scheduleSetEnvelope(); });
    range.value = String(Math.round(state.envelope[key] || (key === "attack_ms" ? 6 : 110)));
    update();
  };
  add("attack_ms", "attack", 1, 40, 1);
  add("release_ms", "release", 20, 300, 5);
}

function currentPct() {
  const c = state.mouthConfig;
  return [Math.round(c.partial * 100), Math.round(c.medium * 100), Math.round(c.open * 100)];
}

function writePct(pp, mm, oo) {
  const c = state.mouthConfig;
  c.partial = pp / 100;
  c.medium = mm / 100;
  c.open = oo / 100;
}

function renderTuning() {
  if (!isPanel) return;
  const cfg = state.mouthConfig;
  els.mouthLevels.innerHTML = "";
  const lowestOn = cfg.enabled.findIndex((e) => e);
  MOUTH_KEYS.forEach((lvl, i) => {
    const chip = document.createElement("label");
    chip.className = "level-chip" + (i === lowestOn ? " is-resting" : "");
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = !!cfg.enabled[i];
    cb.onchange = () => onToggleLevel(i, cb.checked);
    chip.appendChild(cb);
    chip.appendChild(document.createTextNode(lvl + (i === lowestOn ? " · resting" : "")));
    els.mouthLevels.appendChild(chip);
  });

  els.thresholds.innerHTML = "";
  for (const lvl of ["partial", "medium", "open"]) {
    const row = document.createElement("div");
    row.className = "threshold";
    const label = document.createElement("span");
    label.textContent = lvl;
    const range = document.createElement("input");
    range.type = "range";
    range.min = "0";
    range.max = "100";
    range.step = "1";
    const val = document.createElement("span");
    val.className = "tv";
    row.appendChild(label);
    row.appendChild(range);
    row.appendChild(val);
    els.thresholds.appendChild(row);
    range.addEventListener("input", () => onThreshold(lvl, Number(range.value)));
  }
  updateThresholdDOM();
}

function updateThresholdDOM() {
  const [pp, mm, oo] = currentPct();
  const rows = els.thresholds.children;
  const set = (row, value) => {
    row.querySelector("input").value = String(value);
    row.querySelector(".tv").textContent = value + "%";
  };
  set(rows[0], pp);
  set(rows[1], mm);
  set(rows[2], oo);
}

function onThreshold(level, pct) {
  let [pp, mm, oo] = currentPct();
  if (level === "partial") {
    pp = clamp(pct, 0, 98);
    if (pp >= mm) mm = pp + 1; // push medium up
    if (mm >= oo) oo = mm + 1; // cascade to open
  } else if (level === "medium") {
    mm = clamp(pct, 1, 99);
    if (mm <= pp) pp = mm - 1; // push partial down
    if (mm >= oo) oo = mm + 1; // push open up
  } else {
    oo = clamp(pct, 2, 100);
    if (oo <= mm) mm = oo - 1; // push medium down
    if (mm <= pp) pp = mm - 1; // cascade to partial
  }
  pp = clamp(pp, 0, 98);
  mm = clamp(mm, pp + 1, 99);
  oo = clamp(oo, mm + 1, 100);
  writePct(pp, mm, oo);
  updateThresholdDOM();
  scheduleSetMouthConfig();
}

function onToggleLevel(idx, checked) {
  const next = [...state.mouthConfig.enabled];
  next[idx] = checked;
  if (!next.some((e) => e)) {
    showToast("Keep at least one mouth level enabled.");
    renderTuning(); // revert the checkbox
    return;
  }
  state.mouthConfig.enabled = next;
  renderTuning();
  scheduleSetMouthConfig();
}

function clamp(n, lo, hi) {
  return Math.max(lo, Math.min(hi, n));
}

function renderButtons() {
  if (!isPanel) return;
  els.emotions.innerHTML = "";
  const emotions = state.catalog
    ? Object.keys(state.catalog.emotions || {}).sort()
    : [];
  emotions.forEach((e, i) => {
    const cell = document.createElement("div");
    cell.className = "emotion-cell";
    cell.dataset.key = e;
    const b = document.createElement("button");
    b.className = "emotion-btn";
    b.textContent = e;
    b.dataset.key = e;
    b.onclick = () => triggerEmotion(e);
    if (i < 9) {
      const kbd = document.createElement("kbd");
      kbd.className = "hotkey";
      kbd.textContent = String(i + 1);
      b.appendChild(kbd);
    }
    cell.appendChild(b);
    const star = document.createElement("button");
    star.className = "star";
    star.title = `Set "${e}" as the resting emotion`;
    star.setAttribute("aria-label", `Set ${e} as resting emotion`);
    star.textContent = "★";
    star.dataset.key = e;
    star.onclick = (ev) => {
      ev.stopPropagation();
      setDefault(e);
    };
    cell.appendChild(star);
    els.emotions.appendChild(cell);
  });
  els.clearBtn.onclick = clearOverride;

  const buildRow = (container, items) => {
    container.innerHTML = "";
    for (const [key, label, fn, extra, hotkey] of items) {
      const b = document.createElement("button");
      b.textContent = label;
      b.dataset.key = key;
      if (extra) b.classList.add(extra);
      if (hotkey) {
        const kbd = document.createElement("kbd");
        kbd.className = "hotkey";
        kbd.textContent = hotkey;
        b.appendChild(kbd);
      }
      b.onclick = fn;
      container.appendChild(b);
    }
  };

  buildRow(els.mouth, [
    ["auto", "Auto", clearMouth, "auto", "M"],
    ["closed", "Closed", () => setMouth("closed")],
    ["partial", "Partial", () => setMouth("partial")],
    ["medium", "Medium", () => setMouth("medium")],
    ["open", "Open", () => setMouth("open")],
  ]);
  buildRow(els.eyes, [
    ["auto", "Auto", clearEyes, "auto", "E"],
    ["open", "Open", () => setEyes("open")],
    ["closed", "Closed", () => setEyes("closed")],
  ]);
}

function highlight() {
  if (!isPanel) return;
  for (const cell of els.emotions.children) {
    const key = cell.dataset.key;
    cell.classList.toggle("is-active", key === state.emotion);
    cell.classList.toggle("is-default", key === state.defaultEmotion);
  }
  for (const b of els.mouth.children) {
    const isActive = state.mouthOverridden
      ? b.dataset.key === state.mouth
      : b.dataset.key === "auto";
    b.classList.toggle("active", isActive);
  }
  for (const b of els.eyes.children) {
    const isActive = state.eyesOverridden
      ? b.dataset.key === state.eyes
      : b.dataset.key === "auto";
    b.classList.toggle("active", isActive);
  }
}

function applyState(payload) {
  if (payload.default_emotion !== undefined) state.defaultEmotion = payload.default_emotion || "";
  state.emotion = payload.emotion || "";
  state.mouth = payload.mouth;
  state.eyes = payload.eyes;
  state.overridden = payload.overridden === true;
  state.mouthOverridden = payload.mouth_overridden === true;
  state.eyesOverridden = payload.eyes_overridden === true;
  // The avatar video comes from the webcam (getUserMedia), not this WS message;
  // here we only update the UI (meter + control highlighting).
  if (previewVideo && payload.emotion !== undefined) {
    previewVideo.setAttribute(
      "aria-label",
      `avatar, ${payload.emotion || "default"}, mouth ${state.mouth}, eyes ${state.eyes}`
    );
  }
  if (isPanel) {
    updateMeter(payload.volume || 0);
    highlight();
  }
}

let meterPeak = 0;
function updateMeter(volume) {
  const pct = Math.min(100, volume * 100);
  els.meterFill.style.width = pct + "%";
  meterPeak = Math.max(volume, meterPeak * 0.9);
  els.meterPeak.style.width = Math.min(100, meterPeak * 100) + "%";
  const meter = document.getElementById("meter");
  if (meter) meter.setAttribute("aria-valuenow", String(Math.round(pct)));
}

function setConnected(ok) {
  if (!isPanel) return;
  els.dot.classList.toggle("ok", ok);
  if (els.connText) els.connText.textContent = ok ? "connected" : "disconnected";
}

function showToast(message, ms = 4000) {
  if (!isPanel || !els.toast) return;
  els.toast.textContent = message;
  els.toast.classList.add("show");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => els.toast.classList.remove("show"), ms);
}

let socket = null;
let reconnectTimer = null;
let reconnectAttempts = 0;

function connect() {
  socket = new WebSocket(wsUrl()); // text-only: control + meter + config (no video)

  socket.onopen = () => {
    reconnectAttempts = 0;
    setConnected(true);
    send({ type: "Hello" });
  };

  socket.onmessage = (ev) => {
    // The video comes from the webcam (getUserMedia), not this socket — so all
    // WS messages are text JSON control.
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    switch (msg.type) {
      case "Welcome": {
        state.catalog = msg.payload.catalog || null;
        state.defaultEmotion = msg.payload.default_emotion || "";
        if (msg.payload.mouth_config) state.mouthConfig = msg.payload.mouth_config;
        if (msg.payload.envelope) state.envelope = msg.payload.envelope;
        if (els.latencyMode && msg.payload.latency) {
          els.latencyMode.textContent = msg.payload.latency;
        }
        renderButtons();
        renderTuning();
        renderEnvelope();
        highlight();
        break;
      }
      case "StateUpdate": {
        applyState(msg.payload);
        break;
      }
      case "MouthConfigUpdate": {
        if (msg.payload.config) {
          state.mouthConfig = msg.payload.config;
          renderTuning();
        }
        break;
      }
      case "EnvelopeUpdate": {
        if (msg.payload.config) {
          state.envelope = msg.payload.config;
          renderEnvelope();
        }
        break;
      }
      case "Error": {
        showToast(msg.payload.message || "server error");
        break;
      }
    }
  };

  socket.onclose = () => {
    setConnected(false);
    scheduleReconnect();
  };
  socket.onerror = () => {
    try { socket.close(); } catch {}
  };
}

function scheduleReconnect() {
  if (reconnectTimer) return;
  reconnectAttempts += 1;
  const base = Math.min(30000, 1000 * 2 ** (reconnectAttempts - 1));
  const delay = base * (0.5 + Math.random() * 0.5);
  const secs = Math.max(1, Math.round(delay / 1000));
  if (isPanel && els.connText) {
    els.connText.textContent = `reconnecting in ${secs}s…`;
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, delay);
}

window.addEventListener("beforeunload", () => {
  if (reconnectTimer) clearTimeout(reconnectTimer);
});

// --- Keyboard shortcuts (panel only) --------------------------------------
const MOUTH_CYCLE = ["closed", "partial", "medium", "open"];
function cycleMouth() {
  if (!state.mouthOverridden) return setMouth("closed");
  const idx = MOUTH_CYCLE.indexOf(state.mouth);
  if (idx < 0 || idx >= MOUTH_CYCLE.length - 1) return clearMouth();
  setMouth(MOUTH_CYCLE[idx + 1]);
}
function cycleEyes() {
  if (!state.eyesOverridden) return setEyes("open");
  if (state.eyes === "open") return setEyes("closed");
  clearEyes();
}

if (isPanel) {
  document.addEventListener("keydown", (ev) => {
    const t = ev.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
    if (/^[1-9]$/.test(ev.key)) {
      const emotions = state.catalog
        ? Object.keys(state.catalog.emotions || {}).sort()
        : [];
      const pick = emotions[Number(ev.key) - 1];
      if (pick) triggerEmotion(pick);
      return;
    }
    switch (ev.key.toLowerCase()) {
      case "0": clearOverride(); break;
      case "m": cycleMouth(); break;
      case "e": cycleEyes(); break;
      case "d": if (state.emotion) setDefault(state.emotion); break;
    }
  });
}

if (isPanel && els.tuningToggle) {
  els.tuningToggle.onclick = () => {
    const open = els.tuning.hasAttribute("hidden");
    els.tuning.toggleAttribute("hidden", !open);
    els.tuningToggle.textContent = open ? "hide" : "show";
    els.tuningToggle.setAttribute("aria-expanded", String(open));
  };
}
if (previewBtn) {
  previewBtn.onclick = enablePreview;
}

connect();
