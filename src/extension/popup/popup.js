/**
 * boGDan Popup Script
 *
 * Manages the popup UI: cast button, playback controls, detected
 * media list, and real-time status display.
 *
 * ## Status Update Strategy
 *
 * The popup receives real-time status updates via the background
 * service worker, which maintains a WebSocket connection to the
 * boGDan server. When the WebSocket is unavailable, the popup
 * falls back to HTTP polling every 3 seconds.
 *
 * - WebSocket: instant push of MEDIA_STATUS, RESOLVE_PROGRESS, ERROR
 * - HTTP polling: GET /api/status every 3 seconds (fallback)
 */

"use strict";

// ─── DOM Elements ──────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

const statusDot = $("statusDot");
const urlInput = $("urlInput");
const tabBtn = $("tabBtn");
const castBtn = $("castBtn");
const mediaSection = $("mediaSection");
const mediaCount = $("mediaCount");
const mediaList = $("mediaList");
const pauseBtn = $("pauseBtn");
const resumeBtn = $("resumeBtn");
const stopBtn = $("stopBtn");
const seekBackBtn = $("seekBackBtn");
const seekFwdBtn = $("seekFwdBtn");
const volumeSlider = $("volumeSlider");
const volumeLabel = $("volumeLabel");
const progressRow = $("progressRow");
const progressBar = $("progressBar");
const progressFill = $("progressFill");
const positionLabel = $("positionLabel");
const durationLabel = $("durationLabel");
const titleRow = $("titleRow");
const titleText = $("titleText");
const subtitleRow = $("subtitleRow");
const subtitleSelect = $("subtitleSelect");
const statusBox = $("statusBox");
const statusText = $("statusText");
const settingsBtn = $("settingsBtn");
const reconnectBtn = $("reconnectBtn");

// ─── State ─────────────────────────────────────────────────────────

let currentState = "idle";
let currentPosition = 0;
let currentDuration = null;
let wsConnected = false;
let pollingTimer = null;

// ─── Helpers ───────────────────────────────────────────────────────

function formatTime(ms) {
  if (!ms || ms < 0) return "0:00";
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    return `${h}:${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  }
  return `${m}:${s.toString().padStart(2, "0")}`;
}

function setStatus(text, type = "") {
  statusText.textContent = text;
  statusBox.className = "status-box" + (type ? ` ${type}` : "");
}

function setControlsEnabled(enabled) {
  [pauseBtn, resumeBtn, stopBtn, seekBackBtn, seekFwdBtn, volumeSlider].forEach(
    (btn) => (btn.disabled = !enabled)
  );
}

function updatePlaybackUI(state, positionMs, durationMs, volume, title) {
  currentState = state?.toLowerCase() || "idle";
  currentPosition = positionMs || 0;
  currentDuration = durationMs;

  // Status dot.
  statusDot.className = `status-dot ${
    currentState === "playing" ? "playing" :
    currentState === "paused" ? "paused" :
    currentState === "buffering" || currentState === "resolving" || currentState === "seeking" ? "buffering" :
    wsConnected ? "connected" : "disconnected"
  }`;
  statusDot.title = currentState;

  // Controls.
  const isPlaying = currentState === "playing";
  const isPaused = currentState === "paused";
  const isSeeking = currentState === "seeking";
  const isActive = isPlaying || isPaused || isSeeking;
  setControlsEnabled(isActive);
  pauseBtn.disabled = !isPlaying && !isSeeking;
  resumeBtn.disabled = !isPaused;
  subtitleSelect.disabled = !isActive || !wsConnected;

  // Progress.
  if (isActive && currentDuration) {
    progressRow.style.display = "flex";
    positionLabel.textContent = formatTime(currentPosition);
    durationLabel.textContent = currentDuration > 0 ? formatTime(currentDuration) : "--:--";
    const pct = Math.min((currentPosition / currentDuration) * 100, 100);
    progressFill.style.width = `${pct}%`;
  } else {
    progressRow.style.display = "none";
  }

  // Title.
  if (title && isActive) {
    titleRow.style.display = "block";
    titleText.textContent = title;
  } else {
    titleRow.style.display = "none";
  }

  // Volume.
  if (volume != null && volume >= 0) {
    volumeSlider.value = volume;
    volumeLabel.textContent = `${volume}%`;
  }

  // Subtitle row — show when playback is active.
  if (!isActive) {
    subtitleRow.style.display = "none";
    // Clear stale subtitle options
    while (subtitleSelect.options.length > 1) {
      subtitleSelect.remove(1);
    }
  } else {
    subtitleRow.style.display = "flex";
  }

  // Status text.
  const stateLabels = {
    idle: "Ready to cast",
    resolving: "Resolving content\u2026",
    buffering: "Buffering\u2026",
    playing: "Playing",
    paused: "Paused",
    seeking: "Seeking\u2026",
    error: "Error",
    disconnected: "Disconnected",
  };
  setStatus(stateLabels[currentState] || currentState);
}

// ─── boGDan API (via background service worker) ────────────────────

async function sendMessage(type, data = {}) {
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage({ type, ...data }, (response) => {
      if (chrome.runtime.lastError) {
        const msg = chrome.runtime.lastError.message;
        if (msg.includes("Receiving end does not exist") || msg.includes("message port closed")) {
          reject(new Error("Extension is restarting. Please try again."));
        } else {
          reject(new Error(msg));
        }
        return;
      }
      if (response?.error) {
        reject(new Error(response.error));
        return;
      }
      resolve(response);
    });
  });
}

// ─── WebSocket Status Management ──────────────────────────────────

async function checkWsStatus() {
  try {
    const response = await sendMessage("WS_STATUS");
    wsConnected = response?.connected || false;
    if (!wsConnected && currentState === "idle") {
      statusDot.className = "status-dot disconnected";
    }
  } catch {
    wsConnected = false;
  }
}

async function requestReconnect() {
  try {
    await sendMessage("WS_RECONNECT");
    setStatus("Reconnecting\u2026");
    setTimeout(checkWsStatus, 2000);
  } catch {
    // Background worker may be restarting
  }
}

// ─── Load Current Tab URL ──────────────────────────────────────────

async function loadCurrentTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab?.url) {
    urlInput.value = tab.url;
  }
}

// ─── Load Detected Media ───────────────────────────────────────────

async function loadDetectedMedia() {
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab) return;

    const response = await sendMessage("GET_MEDIA_QUEUE", { tabId: tab.id });
    if (response?.success && response.queue?.length > 0) {
      mediaSection.style.display = "block";
      mediaCount.textContent = response.queue.length;
      mediaList.innerHTML = "";

      [...response.queue].reverse().forEach((item) => {
        const div = document.createElement("div");
        div.className = "media-item";

        const typeSpan = document.createElement("span");
        typeSpan.className = `media-type ${item.type}`;
        typeSpan.textContent = item.type;

        const urlSpan = document.createElement("span");
        urlSpan.textContent = item.url.length > 60
          ? item.url.substring(0, 60) + "\u2026"
          : item.url;

        div.appendChild(typeSpan);
        div.appendChild(urlSpan);

        div.addEventListener("click", () => {
          urlInput.value = item.url;
        });
        mediaList.appendChild(div);
      });
    } else {
      mediaSection.style.display = "none";
    }
  } catch {
    mediaSection.style.display = "none";
  }
}

// ─── Refresh Status (HTTP fallback) ────────────────────────────────

async function refreshStatus() {
  try {
    const status = await sendMessage("GET_STATUS");
    if (status?.error) {
      statusDot.className = "status-dot disconnected";
      setStatus("Cannot connect to boGDan. Check settings.", "error");
      setControlsEnabled(false);
      return;
    }

    updatePlaybackUI(
      status.state || status.status,
      status.position_ms || status.position_secs * 1000 || 0,
      status.duration_ms || (status.duration_secs ? status.duration_secs * 1000 : null),
      status.volume,
      status.title || status.current_title
    );
  } catch {
    statusDot.className = "status-dot disconnected";
    setStatus("Cannot connect to boGDan. Check settings.", "error");
    setControlsEnabled(false);
  }
}

function startPolling() {
  stopPolling();
  pollingTimer = setInterval(refreshStatus, 3000);
}

function stopPolling() {
  if (pollingTimer) {
    clearInterval(pollingTimer);
    pollingTimer = null;
  }
}

function updateConnectionStrategy() {
  if (wsConnected) {
    stopPolling();
    refreshStatus();
  } else {
    startPolling();
  }
}

// ─── Event Handlers ────────────────────────────────────────────────

// Cast button.
castBtn.addEventListener("click", doCast);

// Enter key in URL input.
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    doCast();
  }
});

async function doCast() {
  if (castBtn.disabled) return;
  const url = urlInput.value.trim();
  if (!url) {
    setStatus("Please enter a URL", "error");
    return;
  }
  try {
    if (url.startsWith("magnet:")) {
      // magnet links are valid
    } else {
      const parsed = new URL(url);
      if (!["http:", "https:"].includes(parsed.protocol)) throw new Error();
      if (!parsed.hostname) throw new Error();
    }
  } catch {
    setStatus("Invalid URL. Use http://, https://, or magnet:", "error");
    return;
  }

  castBtn.disabled = true;
  castBtn.textContent = "Casting\u2026";
  setStatus("Sending cast request\u2026");

  try {
    await sendMessage("CAST", { url });
    setStatus("Cast request sent!", "success");
    if (!wsConnected) {
      setTimeout(refreshStatus, 2000);
    }
  } catch (err) {
    setStatus(`Error: ${err.message}`, "error");
  } finally {
    castBtn.disabled = false;
    castBtn.innerHTML = `
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <path d="M2 16.1A5 5 0 0 1 5.9 20M2 12.05A9 9 0 0 1 9.95 20M2 8V6a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-6"/>
        <line x1="2" y1="20" x2="2.01" y2="20"/>
      </svg>
      Cast to boGDan`;
  }
}

// Use current tab URL.
tabBtn.addEventListener("click", loadCurrentTab);

// Playback controls.
pauseBtn.addEventListener("click", async () => {
  try { await sendMessage("PAUSE"); }
  catch (err) { setStatus(`Pause failed: ${err.message}`, "error"); }
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

resumeBtn.addEventListener("click", async () => {
  try { await sendMessage("RESUME"); }
  catch (err) { setStatus(`Resume failed: ${err.message}`, "error"); }
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

stopBtn.addEventListener("click", async () => {
  try { await sendMessage("STOP"); }
  catch (err) { setStatus(`Stop failed: ${err.message}`, "error"); }
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

seekBackBtn.addEventListener("click", async () => {
  const newPos = Math.max(0, currentPosition - 10000);
  try { await sendMessage("SEEK", { position_ms: newPos }); }
  catch (err) { setStatus(`Seek failed: ${err.message}`, "error"); }
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

seekFwdBtn.addEventListener("click", async () => {
  const newPos = currentDuration
    ? Math.min(currentPosition + 10000, currentDuration)
    : currentPosition + 10000;
  try { await sendMessage("SEEK", { position_ms: newPos }); } catch {}
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

// Clickable progress bar for seeking.
progressBar.addEventListener("click", async (e) => {
  if (!currentDuration) return;
  const rect = progressBar.getBoundingClientRect();
  const pct = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  const seekMs = Math.round(pct * currentDuration);
  try { await sendMessage("SEEK", { position_ms: seekMs }); } catch {}
  if (!wsConnected) setTimeout(refreshStatus, 500);
});

let volumeDebounceTimer = null;
volumeSlider.addEventListener("input", () => {
  const vol = parseInt(volumeSlider.value, 10);
  volumeLabel.textContent = `${vol}%`;
  clearTimeout(volumeDebounceTimer);
  volumeDebounceTimer = setTimeout(() => {
    sendMessage("VOLUME", { volume: vol }).catch(() => {});
  }, 200);
});

// Subtitle selector.
subtitleSelect.addEventListener("change", async () => {
  const lang = subtitleSelect.value;
  if (lang === "none") {
    try {
      await sendMessage("SUBTITLE", { lang: "none" });
    } catch (err) {
      setStatus(`Subtitles: ${err.message}`, "error");
    }
    return;
  }
  try {
    await sendMessage("SUBTITLE", { lang });
  } catch (err) {
    setStatus(`Subtitles: ${err.message}`, "error");
  }
});

/**
 * Populate the subtitle dropdown with available languages.
 * Expected format from server: { subtitles: [{ lang: "en", label: "English" }, ...] }
 * Falls back to using lang code as label if no label provided.
 */
function updateSubtitleOptions(subtitles, currentLang) {
  // Clear existing options except "Off".
  while (subtitleSelect.options.length > 1) {
    subtitleSelect.remove(1);
  }

  if (!subtitles || !Array.isArray(subtitles) || subtitles.length === 0) return;

  subtitles.forEach((sub) => {
    const option = document.createElement("option");
    option.value = sub.lang || sub.code || sub.language || "";
    option.textContent = sub.label || sub.name || sub.lang || sub.code || sub.language || "Unknown";
    if (currentLang && option.value === currentLang) {
      option.selected = true;
    }
    subtitleSelect.appendChild(option);
  });
}

// Settings.
settingsBtn.addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});

// Reconnect.
reconnectBtn.addEventListener("click", requestReconnect);

// ─── Listen for live status updates from background ────────────────

chrome.runtime.onMessage.addListener((message) => {
  if (message.type === "STATUS_UPDATE") {
    const s = message.status;
    updatePlaybackUI(
      s.state,
      s.position_ms,
      s.duration_ms,
      s.volume,
      s.title || s.source_url
    );

    // Populate subtitle options if provided.
    if (s.subtitles || s.available_subtitles) {
      updateSubtitleOptions(s.subtitles || s.available_subtitles, s.current_subtitle || s.subtitle_lang);
    }
  }

  if (message.type === "WS_STATUS") {
    const wasConnected = wsConnected;
    wsConnected = message.connected;
    if (wasConnected !== wsConnected) {
      updateConnectionStrategy();
    }
    if (!wsConnected && currentState === "idle") {
      statusDot.className = "status-dot disconnected";
    }
  }

  if (message.type === "ERROR") {
    setStatus(`Error: ${message.message}`, "error");
  }

  if (message.type === "RESOLVE_PROGRESS") {
    setStatus(`Resolving\u2026 ${message.percent}%`);
  }
});

// ─── Init ──────────────────────────────────────────────────────────

loadCurrentTab();
loadDetectedMedia();
checkWsStatus().then(() => {
  updateConnectionStrategy();
});

// Periodically check WS status in case background didn't push an update
setInterval(() => {
  checkWsStatus().then(() => {
    updateConnectionStrategy();
  }).catch(() => {});
}, 10000);
