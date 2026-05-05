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
};

// ─── DOM Elements ──────────────────────────────────────────────────

const piHost = document.getElementById("piHost");
const httpPort = document.getElementById("httpPort");
const wsPort = document.getElementById("wsPort");
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
    torMode.value = settings.torMode;
    showNotifications.checked = settings.showNotifications;
    autoCast.checked = settings.autoCast;
  });
}

// ─── Save Settings ─────────────────────────────────────────────────

function saveSettings() {
  const settings = {
    piHost: piHost.value.trim() || DEFAULTS.piHost,
    httpPort: parseInt(httpPort.value, 10) || DEFAULTS.httpPort,
    wsPort: parseInt(wsPort.value, 10) || DEFAULTS.wsPort,
    torMode: torMode.value,
    showNotifications: showNotifications.checked,
    autoCast: autoCast.checked,
  };

  chrome.storage.local.set(settings, () => {
    saveStatus.textContent = "Settings saved!";
    saveBtn.textContent = "Saved ✓";
    setTimeout(() => {
      saveStatus.textContent = "";
      saveBtn.textContent = "Save Settings";
    }, 2000);
  });
}

// ─── Reset Settings ────────────────────────────────────────────────

function resetSettings() {
  if (!confirm("Reset all settings to defaults?")) return;

  chrome.storage.local.set(DEFAULTS, () => {
    loadSettings();
    saveStatus.textContent = "Settings reset to defaults.";
    setTimeout(() => {
      saveStatus.textContent = "";
    }, 2000);
  });
}

// ─── Test Connection ───────────────────────────────────────────────

async function testConnection() {
  const host = piHost.value.trim() || DEFAULTS.piHost;
  const port = parseInt(httpPort.value, 10) || DEFAULTS.httpPort;
  const url = `http://${host}:${port}/api/health`;

  testBtn.disabled = true;
  testBtn.textContent = "Testing…";
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
        testResult.textContent = "✓ Connected successfully!";
        testResult.className = "test-result success";
      } else {
        testResult.textContent = `⚠ Unexpected response: ${JSON.stringify(data)}`;
        testResult.className = "test-result error";
      }
    } else {
      testResult.textContent = `✗ HTTP ${response.status}`;
      testResult.className = "test-result error";
    }
  } catch (err) {
    testResult.textContent = `✗ Connection failed: ${err.message}`;
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
