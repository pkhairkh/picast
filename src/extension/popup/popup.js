/**
 * PiCast Popup Script
 *
 * Manages the popup UI: cast button, playback controls, detected
 * media list, and real-time status display. Communicates with the
 * background service worker via chrome.runtime.sendMessage.
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
const progressFill = $("progressFill");
const positionLabel = $("positionLabel");
const durationLabel = $("durationLabel");
const statusBox = $("statusBox");
const statusText = $("statusText");
const settingsBtn = $("settingsBtn");

// ─── State ─────────────────────────────────────────────────────────

let currentState = "idle";
let currentPosition = 0;
let currentDuration = null;

// ─── Helpers ───────────────────────────────────────────────────────

function formatTime(ms) {
  if (!ms || ms < 0) return "0:00";
  const totalSec = Math.floor(ms / 1000);
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
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

function updatePlaybackUI(state, positionMs, durationMs) {
  currentState = state?.toLowerCase() || "idle";
  currentPosition = positionMs || 0;
  currentDuration = durationMs;

  // Status dot.
  statusDot.className = `status-dot ${currentState === "playing" ? "playing" : currentState === "paused" ? "paused" : currentState === "buffering" || currentState === "resolving" ? "buffering" : "connected"}`;
  statusDot.title = currentState;

  // Controls.
  const isPlaying = currentState === "playing";
  const isPaused = currentState === "paused";
  const isActive = isPlaying || isPaused;
  setControlsEnabled(isActive);
  pauseBtn.disabled = !isPlaying;
  resumeBtn.disabled = !isPaused;

  // Progress.
  if (isActive && currentDuration) {
    progressRow.style.display = "flex";
    positionLabel.textContent = formatTime(currentPosition);
    durationLabel.textContent = formatTime(currentDuration);
    const pct = Math.min((currentPosition / currentDuration) * 100, 100);
    progressFill.style.width = `${pct}%`;
  } else {
    progressRow.style.display = "none";
  }

  // Status text.
  const stateLabels = {
    idle: "Ready to cast",
    resolving: "Resolving content…",
    buffering: "Buffering…",
    playing: "Playing",
    paused: "Paused",
    error: "Error",
  };
  setStatus(stateLabels[currentState] || currentState);
}

// ─── PiCast API (via background service worker) ────────────────────

async function sendMessage(type, data = {}) {
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage({ type, ...data }, (response) => {
      if (chrome.runtime.lastError) {
        reject(new Error(chrome.runtime.lastError.message));
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
          ? item.url.substring(0, 60) + "…"
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

// ─── Refresh Status ────────────────────────────────────────────────

async function refreshStatus() {
  try {
    const status = await sendMessage("GET_STATUS");
    if (status?.error) {
      statusDot.className = "status-dot disconnected";
      setStatus("Cannot connect to PiCast. Check settings.", "error");
      setControlsEnabled(false);
      return;
    }

    updatePlaybackUI(
      status.state,
      status.position_ms || 0,
      status.duration_ms
    );

    if (status.volume != null) {
      volumeSlider.value = status.volume;
      volumeLabel.textContent = `${status.volume}%`;
    }
  } catch {
    statusDot.className = "status-dot disconnected";
    setStatus("Cannot connect to PiCast. Check settings.", "error");
    setControlsEnabled(false);
  }
}

// ─── Event Handlers ────────────────────────────────────────────────

// Cast button.
castBtn.addEventListener("click", async () => {
  const url = urlInput.value.trim();
  if (!url) {
    setStatus("Please enter a URL", "error");
    return;
  }

  castBtn.disabled = true;
  castBtn.textContent = "Casting…";
  setStatus("Sending cast request…");

  try {
    await sendMessage("CAST", { url });
    setStatus("Cast request sent!", "success");
    setTimeout(refreshStatus, 2000);
  } catch (err) {
    setStatus(`Error: ${err.message}`, "error");
  } finally {
    castBtn.disabled = false;
    castBtn.innerHTML = `
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <path d="M2 16.1A5 5 0 0 1 5.9 20M2 12.05A9 9 0 0 1 9.95 20M2 8V6a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-6"/>
        <line x1="2" y1="20" x2="2.01" y2="20"/>
      </svg>
      Cast to PiCast`;
  }
});

// Use current tab URL.
tabBtn.addEventListener("click", loadCurrentTab);

// Playback controls.
pauseBtn.addEventListener("click", async () => {
  try { await sendMessage("PAUSE"); } catch {}
  setTimeout(refreshStatus, 500);
});

resumeBtn.addEventListener("click", async () => {
  try { await sendMessage("RESUME"); } catch {}
  setTimeout(refreshStatus, 500);
});

stopBtn.addEventListener("click", async () => {
  try { await sendMessage("STOP"); } catch {}
  setTimeout(refreshStatus, 500);
});

seekBackBtn.addEventListener("click", async () => {
  const newPos = Math.max(0, currentPosition - 10000);
  try { await sendMessage("SEEK", { position_ms: newPos }); } catch {}
  setTimeout(refreshStatus, 500);
});

seekFwdBtn.addEventListener("click", async () => {
  const newPos = currentPosition + 10000;
  try { await sendMessage("SEEK", { position_ms: newPos }); } catch {}
  setTimeout(refreshStatus, 500);
});

// Volume.
volumeSlider.addEventListener("input", async () => {
  const vol = parseInt(volumeSlider.value, 10);
  volumeLabel.textContent = `${vol}%`;
  try { await sendMessage("VOLUME", { volume: vol }); } catch {}
});

// Settings.
settingsBtn.addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});

// ─── Listen for live status updates from background ────────────────

chrome.runtime.onMessage.addListener((message) => {
  if (message.type === "STATUS_UPDATE") {
    const s = message.status;
    updatePlaybackUI(s.state, s.position_ms, s.duration_ms);
  }
  if (message.type === "ERROR") {
    setStatus(`Error: ${message.message}`, "error");
  }
  if (message.type === "RESOLVE_PROGRESS") {
    setStatus(`Resolving… ${message.percent}%`);
  }
});

// ─── Init ──────────────────────────────────────────────────────────

loadCurrentTab();
loadDetectedMedia();
refreshStatus();

// Poll status every 3 seconds.
setInterval(refreshStatus, 3000);
