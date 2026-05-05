/**
 * PiCast Popup Script
 *
 * Manages the popup UI: cast button, playback controls, detected media list,
 * and real-time status display.
 */

// ─── DOM Elements ──────────────────────────────────────────────
const statusDot = document.getElementById("statusDot");
const urlInput = document.getElementById("urlInput");
const castBtn = document.getElementById("castBtn");
const pauseBtn = document.getElementById("pauseBtn");
const stopBtn = document.getElementById("stopBtn");
const seekBackBtn = document.getElementById("seekBackBtn");
const seekFwdBtn = document.getElementById("seekFwdBtn");
const mediaList = document.getElementById("mediaList");
const statusBox = document.getElementById("statusBox");
const settingsLink = document.getElementById("settingsLink");

// ─── State ─────────────────────────────────────────────────────
let currentStatus = null;

// ─── PiCast API ────────────────────────────────────────────────

async function getConfig() {
  return new Promise((resolve) => {
    chrome.storage.local.get(
      { piAddress: "picast.local", piPort: 8585, torMode: "full" },
      resolve
    );
  });
}

async function apiCall(endpoint, method = "GET", body = null) {
  const config = await getConfig();
  const url = `http://${config.piAddress}:${config.piPort}${endpoint}`;
  const opts = { method, headers: { "Content-Type": "application/json" } };
  if (body) opts.body = JSON.stringify(body);
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(`API error: ${res.status}`);
  return res.json();
}

// ─── Status Polling ────────────────────────────────────────────

async function refreshStatus() {
  try {
    const status = await apiCall("/api/status");
    currentStatus = status;
    updateUI(status);
    statusDot.className = "status-dot " + (status.status === "playing" ? "playing" : "connected");
  } catch {
    statusDot.className = "status-dot disconnected";
    statusBox.textContent = "Cannot connect to PiCast. Check settings.";
  }
}

function updateUI(status) {
  const stateLabels = {
    idle: "Ready to cast",
    resolving: "Resolving content...",
    buffering: "Buffering...",
    playing: `Playing: ${status.title || status.url || ""}`,
    paused: `Paused: ${status.title || status.url || ""}`,
    error: `Error: ${status.error || "unknown"}`,
  };
  statusBox.textContent = stateLabels[status.status] || status.status;

  if (status.position != null && status.duration != null) {
    const pct = ((status.position / status.duration) * 100).toFixed(1);
    statusBox.textContent += ` (${pct}%)`;
  }
}

// ─── Load Detected Media ───────────────────────────────────────

async function loadDetectedMedia() {
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab) return;
    const response = await chrome.runtime.sendMessage({
      type: "GET_MEDIA_QUEUE",
      tabId: tab.id,
    });
    if (response.success && response.queue.length > 0) {
      mediaList.innerHTML = "";
      // Show most recent first
      [...response.queue].reverse().forEach((item, idx) => {
        const div = document.createElement("div");
        div.className = "media-item";
        div.textContent = `[${item.type}] ${item.url.substring(0, 80)}...`;
        div.addEventListener("click", () => {
          urlInput.value = item.url;
        });
        mediaList.appendChild(div);
      });
    }
  } catch {
    // Extension context may not be ready
  }
}

// ─── Event Handlers ────────────────────────────────────────────

castBtn.addEventListener("click", async () => {
  const url = urlInput.value.trim();
  if (!url) return;
  castBtn.disabled = true;
  castBtn.textContent = "Casting...";
  try {
    await chrome.runtime.sendMessage({ type: "CAST", url });
    statusBox.textContent = "Cast request sent!";
    setTimeout(refreshStatus, 2000);
  } catch (err) {
    statusBox.textContent = `Error: ${err.message}`;
  } finally {
    castBtn.disabled = false;
    castBtn.textContent = "Cast";
  }
});

pauseBtn.addEventListener("click", async () => {
  try { await apiCall("/api/pause", "POST"); } catch {}
  setTimeout(refreshStatus, 500);
});

stopBtn.addEventListener("click", async () => {
  try { await apiCall("/api/stop", "POST"); } catch {}
  setTimeout(refreshStatus, 500);
});

seekBackBtn.addEventListener("click", async () => {
  try { await apiCall("/api/seek", "POST", { seconds: -10 }); } catch {}
  setTimeout(refreshStatus, 500);
});

seekFwdBtn.addEventListener("click", async () => {
  try { await apiCall("/api/seek", "POST", { seconds: 10 }); } catch {}
  setTimeout(refreshStatus, 500);
});

settingsLink.addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});

// ─── Init ──────────────────────────────────────────────────────
refreshStatus();
loadDetectedMedia();
setInterval(refreshStatus, 3000);
