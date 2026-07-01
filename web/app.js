// Rusty-Tuber web client. Shared by the control panel (index.html) and the
// OBS stage (stage.html). Connects to the server WebSocket, preloads every
// layer for instant swaps, renders the emotion/mouth buttons (panel only), and
// keeps the displayed avatar in sync with authoritative StateUpdate messages.
//
// The avatar is a stack of transparent PNG layers (all the same canvas size):
//   base   — one or more static body images (rendered bottom-up, never change)
//   eyes   — the eye layer; swaps on blink and on emotion (eye-expression) change
//   mouth  — the mouth layer; swaps with mic volume
// Only the eye and mouth layers ever swap; the body stays put.

const mode = document.body.dataset.mode;
const isPanel = mode === "panel";

const stageWrap = document.getElementById("stage-wrap");
const layerEyes = document.getElementById("layer-eyes");
const layerMouth = document.getElementById("layer-mouth");

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
      stageUrlEl: document.getElementById("stage-url"),
      copyBtn: document.getElementById("copy-stage-url"),
      toast: document.getElementById("toast"),
      tuningToggle: document.getElementById("tuning-toggle"),
      tuning: document.getElementById("tuning"),
      mouthLevels: document.getElementById("mouth-levels"),
      thresholds: document.getElementById("thresholds"),
    }
  : null;

let state = {
  catalog: null,      // layered catalog: { base, mouths, default_eyes, emotions }
  defaultEmotion: "",
  emotion: "",
  mouth: "closed",
  eyes: "open",
  overridden: false,
  // Authoritative override flags come from the server so every client (panel,
  // OBS, phone) agrees even when a Stream Deck issues overrides via REST.
  mouthOverridden: false,
  eyesOverridden: false,
  // Mouth-level config: which levels are active + pick-up thresholds. Drives
  // the tuning card and is pushed back to the server on change.
  mouthConfig: { enabled: [true, true, true, true], partial: 0.02, medium: 0.08, open: 0.18 },
};

const cache = new Map(); // frame URL -> HTMLImageElement (preloaded)
let toastTimer = null;

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}

const MOUTH_KEYS = ["closed", "partial", "medium", "open"];

// Preload every layer PNG so swaps are instant and flicker-free. Covers the
// base body, all mouth levels, and every eye frame (default + each emotion's
// open/closed set).
function preloadAll(catalog) {
  const want = (rel) => {
    if (!rel) return;
    const url = `/frames/${rel}`;
    if (cache.has(url)) return;
    const img = new Image();
    img.src = url;
    cache.set(url, img);
  };
  for (const rel of catalog.base || []) want(rel);
  for (const rel of Object.values(catalog.mouths || {})) want(rel);
  want((catalog.default_eyes || {}).open);
  want((catalog.default_eyes || {}).closed);
  for (const set of Object.values(catalog.emotions || {})) {
    want(set.open);
    want(set.closed);
  }
}

// Build the static base layer(s) under the eye/mouth layers. Called on
// (re)connect when the catalog arrives; the body never changes after this.
function renderBaseLayers(catalog) {
  if (!stageWrap || !layerEyes) return;
  for (const el of stageWrap.querySelectorAll("img.layer-base")) el.remove();
  for (const rel of catalog.base || []) {
    const img = document.createElement("img");
    img.className = "layer layer-base";
    img.alt = "";
    img.src = `/frames/${rel}`;
    stageWrap.insertBefore(img, layerEyes); // keep base below eyes/mouth
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
// `enabled` is indexed by MOUTH_KEYS: [closed, partial, medium, open]. The
// lowest enabled level is the resting mouth. Thresholds are stored as 0..1 and
// shown as percent; strict order partial<medium<open is enforced live by
// clamping each slider within its neighbours, so two can never be equal.
let mouthConfigTimer = null;

function setMouthConfig(cfg) {
  send({ type: "SetMouthConfig", payload: { config: cfg } });
}

// Debounce so dragging a slider doesn't flood the server.
function scheduleSetMouthConfig() {
  if (mouthConfigTimer) clearTimeout(mouthConfigTimer);
  mouthConfigTimer = setTimeout(() => {
    mouthConfigTimer = null;
    setMouthConfig(state.mouthConfig);
  }, 150);
}

function renderTuning() {
  if (!isPanel) return;
  const cfg = state.mouthConfig;

  // Active-level chips with a "resting" marker on the lowest enabled.
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

  // Threshold sliders (partial / medium / open).
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

// Recompute slider min/max/value/labels from state (keeps them ordered).
function updateThresholdDOM() {
  const cfg = state.mouthConfig;
  const pp = Math.round(cfg.partial * 100);
  const mm = Math.round(cfg.medium * 100);
  const oo = Math.round(cfg.open * 100);
  const rows = els.thresholds.children;
  const set = (row, value, min, max) => {
    const range = row.querySelector("input");
    range.min = String(min);
    range.max = String(max);
    range.value = String(value);
    row.querySelector(".tv").textContent = value + "%";
  };
  set(rows[0], pp, 0, mm - 1);          // partial: [0, medium-1]
  set(rows[1], mm, pp + 1, oo - 1);     // medium:  [partial+1, open-1]
  set(rows[2], oo, mm + 1, 100);        // open:    [medium+1, 100]
}

function onThreshold(level, pct) {
  const cfg = state.mouthConfig;
  const pp = Math.round(cfg.partial * 100);
  const mm = Math.round(cfg.medium * 100);
  const oo = Math.round(cfg.open * 100);
  if (level === "partial") cfg.partial = clamp(pct, 0, mm - 1) / 100;
  else if (level === "medium") cfg.medium = clamp(pct, pp + 1, oo - 1) / 100;
  else cfg.open = clamp(pct, mm + 1, 100) / 100;
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
  // Emotions come from the catalog's eye-expression sets. With no emotion art
  // on disk, this is empty and the section renders nothing.
  const emotions = state.catalog ? Object.keys(state.catalog.emotions || {}).sort() : [];
  emotions.forEach((e, i) => {
    // Each emotion is a cell: a trigger button plus a small "set as resting"
    // star so the resting face can be changed from the UI (not just config).
    const cell = document.createElement("div");
    cell.className = "emotion-cell";
    cell.dataset.key = e;

    const b = document.createElement("button");
    b.className = "emotion-btn";
    b.textContent = e;
    b.dataset.key = e;
    b.onclick = () => triggerEmotion(e);
    // Number-key badge for the first nine emotions.
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
  // Mouth/Eyes: highlight Auto when no override is active, else the forced
  // value. The flags are authoritative from the server, so two clients (panel
  // + Stream Deck via REST) always agree.
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
  // Only the eye and mouth layers swap; the base body stays put. Dedupe by URL
  // so identical frames don't trigger a reload (keeps swaps flicker-free).
  if (layerEyes && payload.eyes_frame && !layerEyes.src.endsWith(payload.eyes_frame)) {
    layerEyes.src = payload.eyes_frame;
  }
  if (layerMouth && payload.mouth_frame && !layerMouth.src.endsWith(payload.mouth_frame)) {
    layerMouth.src = payload.mouth_frame;
  }
  if (layerMouth && payload.emotion !== undefined) {
    layerMouth.alt = `avatar, ${payload.emotion || "default"}, mouth ${state.mouth}, eyes ${state.eyes}`;
  }
  if (isPanel) {
    updateMeter(payload.volume || 0);
    highlight();
  }
}

// Peak-hold meter: the fill tracks the live level; a decaying peak marker
// remembers the recent maximum and falls back smoothly.
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
  if (els.connText) {
    els.connText.textContent = ok ? "connected" : "disconnected";
  }
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
  socket = new WebSocket(wsUrl());

  socket.onopen = () => {
    reconnectAttempts = 0;
    setConnected(true);
    send({ type: "Hello" });
  };

  socket.onmessage = (ev) => {
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
        preloadAll(state.catalog || {});
        renderBaseLayers(state.catalog || {});
        renderButtons();
        renderTuning();
        highlight();
        break;
      }
      case "StateUpdate": {
        applyState(msg.payload);
        break;
      }
      case "MouthConfigUpdate": {
        // Another client (or REST) changed the tuning — sync without echoing.
        if (msg.payload.config) {
          state.mouthConfig = msg.payload.config;
          renderTuning();
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

// Exponential backoff with full jitter, capped at 30s, so a long server outage
// doesn't hammer thousands of reconnects and the user sees a countdown.
function scheduleReconnect() {
  if (reconnectTimer) return;
  reconnectAttempts += 1;
  const base = Math.min(30000, 1000 * 2 ** (reconnectAttempts - 1));
  const delay = base * (0.5 + Math.random() * 0.5); // 50–100% of base
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
// 1..9 trigger the first nine emotions, 0 clears the override, M cycles mouth,
// E cycles eyes, D sets the current emotion as the resting face.
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
    if (
      t &&
      (t.tagName === "INPUT" ||
        t.tagName === "TEXTAREA" ||
        t.isContentEditable)
    ) {
      return;
    }
    if (ev.metaKey || ev.ctrlKey || ev.altKey) return;

    if (/^[1-9]$/.test(ev.key)) {
      const emotions = Object.keys(state.catalog).sort();
      const pick = emotions[Number(ev.key) - 1];
      if (pick) triggerEmotion(pick);
      return;
    }
    switch (ev.key.toLowerCase()) {
      case "0":
        clearOverride();
        break;
      case "m":
        cycleMouth();
        break;
      case "e":
        cycleEyes();
        break;
      case "d":
        if (state.emotion) setDefault(state.emotion);
        break;
    }
  });
}

if (isPanel && els.stageUrlEl) {
  els.stageUrlEl.textContent = `${location.origin}/stage.html`;
}
if (isPanel && els.copyBtn) {
  els.copyBtn.onclick = async () => {
    const url = els.stageUrlEl.textContent;
    try {
      await navigator.clipboard.writeText(url);
      showToast("Stage URL copied");
    } catch {
      showToast("Copy failed — select the URL manually");
    }
  };
}
if (isPanel && els.tuningToggle) {
  els.tuningToggle.onclick = () => {
    const open = els.tuning.hasAttribute("hidden");
    els.tuning.toggleAttribute("hidden", !open);
    els.tuningToggle.textContent = open ? "hide" : "show";
    els.tuningToggle.setAttribute("aria-expanded", String(open));
  };
}

connect();
