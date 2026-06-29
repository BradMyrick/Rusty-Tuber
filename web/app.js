// Rusty-Tuber web client. Shared by the control panel (index.html) and the
// OBS stage (stage.html). Connects to the server WebSocket, preloads every
// frame for instant swaps, renders the emotion/mouth buttons (panel only), and
// keeps the displayed avatar in sync with authoritative StateUpdate messages.

const mode = document.body.dataset.mode;
const isPanel = mode === "panel";

const avatar = document.getElementById("avatar");
const stageUrlEl = document.getElementById("stage-url");

const els = isPanel
  ? {
      dot: document.getElementById("dot"),
      meterFill: document.getElementById("meter-fill"),
      emotions: document.getElementById("emotions"),
      mouth: document.getElementById("mouth"),
      clearBtn: document.getElementById("clear"),
    }
  : null;

let state = {
  catalog: {},       // emotion -> {closed, slight?, medium?, open}
  defaultEmotion: "",
  emotion: "",
  mouth: "closed",
  overridden: false,
};

const cache = new Map(); // frame URL -> HTMLImageElement (preloaded)

function wsUrl() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}

function preloadAll(catalog) {
  for (const [emotion, frames] of Object.entries(catalog)) {
    for (const rel of [frames.closed, frames.slight, frames.medium, frames.open]) {
      if (!rel) continue;
      const url = `/frames/${rel}`;
      if (cache.has(url)) continue;
      const img = new Image();
      img.src = url;
      cache.set(url, img);
    }
  }
}

function send(msg) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(msg));
  }
}

const triggerEmotion = (e) => send({ type: "TriggerEmotion", payload: { emotion: e } });
const clearOverride = () => send({ type: "ClearOverride" });
const setMouth = (m) => send({ type: "SetMouthOverride", payload: { mouth: m } });
const clearMouth = () => send({ type: "ClearMouthOverride" });

function renderButtons() {
  if (!isPanel) return;
  els.emotions.innerHTML = "";
  const emotions = Object.keys(state.catalog).sort();
  for (const e of emotions) {
    const b = document.createElement("button");
    b.textContent = e;
    b.dataset.emotion = e;
    if (e === state.defaultEmotion) b.classList.add("default-emo");
    if (e === state.emotion) b.classList.add("active");
    b.onclick = () => triggerEmotion(e);
    els.emotions.appendChild(b);
  }
  els.clearBtn.onclick = clearOverride;

  els.mouth.innerHTML = "";
  const mouths = [
    ["auto", "Auto", clearMouth],
    ["closed", "Closed", () => setMouth("closed")],
    ["slight", "Slight", () => setMouth("slight")],
    ["medium", "Medium", () => setMouth("medium")],
    ["open", "Open", () => setMouth("open")],
  ];
  for (const [key, label, fn] of mouths) {
    const b = document.createElement("button");
    b.textContent = label;
    b.dataset.mouth = key;
    if (key === "auto") b.classList.add("auto");
    b.onclick = fn;
    els.mouth.appendChild(b);
  }
}

function highlight() {
  if (!isPanel) return;
  for (const b of els.emotions.children) {
    b.classList.toggle("active", b.dataset.emotion === state.emotion);
    b.classList.toggle("default-emo", b.dataset.emotion === state.defaultEmotion);
  }
  // Mouth override active when not "auto" and overridden flag set via server.
  for (const b of els.mouth.children) {
    b.classList.toggle("active", b.dataset.mouth === state.mouth);
  }
}

function applyState(payload) {
  if (payload.default_emotion) state.defaultEmotion = payload.default_emotion;
  state.emotion = payload.emotion;
  state.mouth = payload.mouth;
  state.overridden = payload.overridden;
  if (payload.frame && avatar.src.indexOf(payload.frame) === -1) {
    avatar.src = payload.frame;
  }
  if (isPanel) {
    els.meterFill.style.width = `${Math.min(100, payload.volume * 100)}%`;
    highlight();
  }
}

function setConnected(ok) {
  if (!isPanel) return;
  els.dot.classList.toggle("ok", ok);
}

let socket = null;
let reconnectTimer = null;

function connect() {
  socket = new WebSocket(wsUrl());

  socket.onopen = () => setConnected(true);

  socket.onmessage = (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    switch (msg.type) {
      case "Welcome": {
        state.catalog = msg.payload.catalog || {};
        state.defaultEmotion = msg.payload.default_emotion || "";
        preloadAll(state.catalog);
        renderButtons();
        break;
      }
      case "StateUpdate": {
        applyState(msg.payload);
        break;
      }
      case "Error": {
        console.warn("server error:", msg.payload.message);
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
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, 1000);
}

if (isPanel && stageUrlEl) {
  stageUrlEl.textContent = `${location.origin}/stage.html`;
}

connect();
