/**
 * PiCast Options Page Script
 *
 * Manages the settings UI: load/save/reset preferences,
 * test connection to PiCast receiver.
 */

"use strict";

const DEFAULTS = {
  piHost: "picast.local",
  httpPort: 8585,
  wsPort: 8586,
  torMode: "full",
  showNotifications: true,
  autoCast: false,
  quality: "720p",
};

// ─── DOM Elements ──────────────────────────────────────────────────

const piHost = document.getElementById("piHost");
const httpPort = document.getElementById("httpPort");
const wsPort = document.getElementById("wsPort");
const quality = document.getElementById("quality");
const torMode = document.getElementById("torMode");
const showNotifications = document.getElementById("showNotifications");
const autoCast = document.getElementById("autoCast");
const testBtn = document.getElementById("testBtn");
const testResult = document.getElementById("testResult");
const saveBtn = document.getElementById("saveBtn");
const resetBtn = document.getElementById("resetBtn");
const saveStatus = document.getElementById("saveStatus");

// ─── Load Settings ─────────────────────────────────────────────────

function loadSettings() {
  chrome.storage.local.get(DEFAULTS, (settings) => {
    piHost.value = settings.piHost;
    httpPort.value = settings.httpPort;
    wsPort.value = settings.wsPort;
    quality.value = settings.quality;
    torMode.value = settings.torMode;
    showNotifications.checked = settings.showNotifications;
    autoCast.checked = settings.autoCast;
  });
}

// ─── Save Settings ─────────────────────────────────────────────────

function saveSettings() {
  // Validate
  const host = piHost.value.trim();
  const http = parseInt(httpPort.value, 10);
  const ws = parseInt(wsPort.value, 10);

  // Strip scheme, port, and path if user pasted a full URL
  let sanitizedHost = host;
  try {
    const u = new URL(host.startsWith("http") ? host : `http://${host}`);
    sanitizedHost = u.hostname;
  } catch {
    // Not a URL, use as-is
  }
  if (!sanitizedHost) {
    saveStatus.textContent = "Host address is required.";
    saveStatus.style.color = "#f44336";
    return;
  }

  if (isNaN(http) || http < 1 || http > 65535) {
    saveStatus.textContent = "HTTP port must be between 1 and 65535.";
    saveStatus.style.color = "#f44336";
    return;
  }

  if (isNaN(ws) || ws < 1 || ws > 65535) {
    saveStatus.textContent = "WebSocket port must be between 1 and 65535.";
    saveStatus.style.color = "#f44336";
    return;
  }

  if (http === ws) {
    saveStatus.textContent = "HTTP and WebSocket ports must be different.";
    saveStatus.style.color = "#f44336";
    return;
  }

  const settings = {
    piHost: sanitizedHost,
    httpPort: http,
    wsPort: ws,
    quality: quality.value,
    torMode: torMode.value,
    showNotifications: showNotifications.checked,
    autoCast: autoCast.checked,
  };

  chrome.storage.local.set(settings, () => {
    saveStatus.style.color = "#4caf50";
    saveStatus.textContent = "Settings saved!";
    saveBtn.textContent = "Saved \u2713";
    setTimeout(() => {
      saveStatus.textContent = "";
      saveBtn.textContent = "Save Settings";
    }, 2000);

    // Ask background service worker to reconnect with new settings
    try {
      chrome.runtime.sendMessage({ type: "WS_RECONNECT" }).catch(() => {});
    } catch {}
  });
}

// ─── Reset Settings ────────────────────────────────────────────────

function resetSettings() {
  if (!confirm("Reset all settings to defaults?")) return;

  chrome.storage.local.set(DEFAULTS, () => {
    loadSettings();
    saveStatus.style.color = "#4caf50";
    saveStatus.textContent = "Settings reset to defaults.";
    setTimeout(() => {
      saveStatus.textContent = "";
    }, 2000);
  });
}

// ─── Test Connection ───────────────────────────────────────────────

async function testConnection() {
  const rawHost = piHost.value.trim() || DEFAULTS.piHost;
  // Strip scheme, port, and path if user pasted a full URL
  let host = rawHost;
  try {
    const u = new URL(rawHost.startsWith("http") ? rawHost : `http://${rawHost}`);
    host = u.hostname;
  } catch {
    // Not a URL, use as-is
  }
  const port = parseInt(httpPort.value, 10) || DEFAULTS.httpPort;
  const url = `http://${host}:${port}/api/health`;

  testBtn.disabled = true;
  testBtn.textContent = "Testing\u2026";
  testResult.textContent = "";
  testResult.className = "test-result";

  try {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 5000);

    const response = await fetch(url, { signal: controller.signal });
    clearTimeout(timeout);

    if (response.ok) {
      const data = await response.json();
      if (data.status === "ok") {
        testResult.textContent = "\u2713 Connected successfully!";
        testResult.className = "test-result success";
      } else {
        testResult.textContent = `\u26A0 Unexpected response: ${JSON.stringify(data)}`;
        testResult.className = "test-result error";
      }
    } else {
      testResult.textContent = `\u2717 HTTP ${response.status}`;
      testResult.className = "test-result error";
    }
  } catch (err) {
    const msg = err.name === "AbortError"
      ? "Connection timed out (5s)"
      : err.message;
    testResult.textContent = `\u2717 Connection failed: ${msg}`;
    testResult.className = "test-result error";
  } finally {
    testBtn.disabled = false;
    testBtn.textContent = "Test Connection";
  }
}

// ─── Event Handlers ────────────────────────────────────────────────

saveBtn.addEventListener("click", saveSettings);
resetBtn.addEventListener("click", resetSettings);
testBtn.addEventListener("click", testConnection);

// ─── Init ──────────────────────────────────────────────────────────

loadSettings();
