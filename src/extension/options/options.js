/**
 * PiCast Options Page Script
 *
 * Manages the settings UI: load/save/reset preferences,
 * test connection to PiCast receiver.
 *
 * Firefox MV3 note: optional_host_permissions ("http://*/*") must be
 * explicitly granted by the user via chrome.permissions.request().
 * The "Test Connection" flow requests this permission if needed,
 * then routes the health-check through the background script to
 * avoid CSP/CORS issues in extension pages.
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

// ─── Host Permission Helpers ───────────────────────────────────────

/**
 * Determine the origin pattern needed for a given host.
 * - *.local hosts are covered by the mandatory host_permissions.
 * - IP addresses and other hostnames need optional_host_permissions.
 */
function needsOptionalPermission(host) {
  // *.local hosts are covered by "http://*.local/*" in host_permissions
  if (host.endsWith(".local")) return false;
  // Everything else (IP addresses, .lan, etc.) needs optional permission
  return true;
}

/**
 * Request the optional "http://*/*" host permission.
 * Must be called from a user-gesture handler (button click).
 * Returns true if permission was granted (or already held).
 */
async function requestHostPermission(host) {
  const origin = `http://${host}/*`;

  // Check if we already have it
  const hasPermission = await new Promise((resolve) => {
    chrome.permissions.contains({ origins: [origin] }, resolve);
  });
  if (hasPermission) return true;

  // We also try the broader pattern as a fallback
  const broadOrigin = "http://*/*";
  const hasBroadPermission = await new Promise((resolve) => {
    chrome.permissions.contains({ origins: [broadOrigin] }, resolve);
  });
  if (hasBroadPermission) return true;

  // Request the specific origin first (less scary to the user)
  console.log("[PiCast Options] Requesting host permission for", origin);
  const granted = await new Promise((resolve) => {
    chrome.permissions.request({ origins: [origin] }, resolve);
  });

  if (granted) return true;

  // Fallback: try the broad pattern
  console.log("[PiCast Options] Specific origin denied, trying broad pattern");
  const broadGranted = await new Promise((resolve) => {
    chrome.permissions.request({ origins: [broadOrigin] }, resolve);
  });
  return broadGranted;
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

  testBtn.disabled = true;
  testBtn.textContent = "Testing\u2026";
  testResult.textContent = "";
  testResult.className = "test-result";

  try {
    // Step 1: Ensure we have host permission for this target
    if (needsOptionalPermission(host)) {
      testResult.textContent = "Requesting permission\u2026";
      testResult.className = "test-result";

      const granted = await requestHostPermission(host);
      if (!granted) {
        testResult.textContent = "\u2717 Permission denied. The extension needs access to http:// URLs to connect to your Pi.";
        testResult.className = "test-result error";
        return;
      }
    }

    // Step 2: Route the health check through the background script
    // (avoids Firefox CSP/CORS issues with fetch() from extension pages)
    testResult.textContent = "Connecting\u2026";
    const result = await new Promise((resolve, reject) => {
      chrome.runtime.sendMessage(
        { type: "TEST_CONNECTION", host, port },
        (response) => {
          if (chrome.runtime.lastError) {
            reject(new Error(chrome.runtime.lastError.message));
            return;
          }
          resolve(response);
        }
      );
    });

    if (result && result.success) {
      testResult.textContent = `\u2713 ${result.message}`;
      testResult.className = "test-result success";
    } else {
      testResult.textContent = `\u2717 Connection failed: ${result?.error || "Unknown error"}`;
      testResult.className = "test-result error";
    }
  } catch (err) {
    testResult.textContent = `\u2717 Error: ${err.message}`;
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
