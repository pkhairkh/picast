/**
 * PiCast Options Page Script
 *
 * Manages the settings UI: load/save/reset preferences,
 * test connection to PiCast receiver.
 *
 * Firefox MV3 note: optional_host_permissions must be explicitly granted
 * by the user via chrome.permissions.request(). The "Test Connection" flow
 * requests this permission if needed, then routes the health-check through
 * the background script to avoid CSP/CORS issues in extension pages.
 */

"use strict";

var DEFAULTS = {
  piHost: "picast.local",
  httpPort: 8585,
  wsPort: 8586,
  torMode: "full",
  showNotifications: true,
  autoCast: false,
  quality: "720p",
};

// ─── DOM Elements ──────────────────────────────────────────────────

var piHost = document.getElementById("piHost");
var httpPort = document.getElementById("httpPort");
var wsPort = document.getElementById("wsPort");
var quality = document.getElementById("quality");
var torMode = document.getElementById("torMode");
var showNotifications = document.getElementById("showNotifications");
var autoCast = document.getElementById("autoCast");
var testBtn = document.getElementById("testBtn");
var testResult = document.getElementById("testResult");
var saveBtn = document.getElementById("saveBtn");
var resetBtn = document.getElementById("resetBtn");
var saveStatus = document.getElementById("saveStatus");

// ─── Load Settings ─────────────────────────────────────────────────

function loadSettings() {
  chrome.storage.local.get(DEFAULTS, function (settings) {
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
  var host = piHost.value.trim();
  var http = parseInt(httpPort.value, 10);
  var ws = parseInt(wsPort.value, 10);

  // Strip scheme, port, and path if user pasted a full URL
  var sanitizedHost = host;
  try {
    var u = new URL(host.startsWith("http") ? host : "http://" + host);
    sanitizedHost = u.hostname;
  } catch (e) {
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

  var settings = {
    piHost: sanitizedHost,
    httpPort: http,
    wsPort: ws,
    quality: quality.value,
    torMode: torMode.value,
    showNotifications: showNotifications.checked,
    autoCast: autoCast.checked,
  };

  chrome.storage.local.set(settings, function () {
    saveStatus.style.color = "#4caf50";
    saveStatus.textContent = "Settings saved!";
    saveBtn.textContent = "Saved \u2713";
    setTimeout(function () {
      saveStatus.textContent = "";
      saveBtn.textContent = "Save Settings";
    }, 2000);

    // Ask background service worker to reconnect with new settings
    try {
      chrome.runtime.sendMessage({ type: "WS_RECONNECT" }).catch(function () {});
    } catch (e) {}
  });
}

// ─── Reset Settings ────────────────────────────────────────────────

function resetSettings() {
  if (!confirm("Reset all settings to defaults?")) return;

  chrome.storage.local.set(DEFAULTS, function () {
    loadSettings();
    saveStatus.style.color = "#4caf50";
    saveStatus.textContent = "Settings reset to defaults.";
    setTimeout(function () {
      saveStatus.textContent = "";
    }, 2000);
  });
}

// ─── Host Permission Helpers ───────────────────────────────────────

/**
 * Determine the origin pattern needed for a given host.
 * - .local hosts are covered by the mandatory host_permissions in manifest.
 * - IP addresses and other hostnames need optional_host_permissions.
 */
function needsOptionalPermission(host) {
  // .local hosts are already covered by mandatory host_permissions
  if (host.endsWith(".local")) return false;
  // Everything else (IP addresses, .lan, etc.) needs optional permission
  return true;
}

/**
 * Build the match-pattern origin string for a given host.
 * Returns e.g. "http://192.168.50.88/*"
 */
function buildOriginPattern(host) {
  return "http://" + host + "/*";
}

/** The broad match pattern for all HTTP origins. */
var BROAD_ORIGIN = "http://*/*";

/**
 * Request the optional host permission.
 * Must be called from a user-gesture handler (button click).
 * Returns true if permission was granted (or already held).
 */
function requestHostPermission(host) {
  var origin = buildOriginPattern(host);

  return new Promise(function (resolve) {
    // Check if we already have it
    chrome.permissions.contains({ origins: [origin] }, function (hasPermission) {
      if (hasPermission) { resolve(true); return; }

      // Also check the broad pattern
      chrome.permissions.contains({ origins: [BROAD_ORIGIN] }, function (hasBroad) {
        if (hasBroad) { resolve(true); return; }

        // Request the specific origin first (less scary to the user)
        console.log("[PiCast Options] Requesting host permission for", origin);
        chrome.permissions.request({ origins: [origin] }, function (granted) {
          if (granted) { resolve(true); return; }

          // Fallback: try the broad pattern
          console.log("[PiCast Options] Specific origin denied, trying broad pattern");
          chrome.permissions.request({ origins: [BROAD_ORIGIN] }, function (broadGranted) {
            resolve(!!broadGranted);
          });
        });
      });
    });
  });
}

// ─── Test Connection ───────────────────────────────────────────────

function testConnection() {
  var rawHost = piHost.value.trim() || DEFAULTS.piHost;
  // Strip scheme, port, and path if user pasted a full URL
  var host = rawHost;
  try {
    var u = new URL(rawHost.startsWith("http") ? rawHost : "http://" + rawHost);
    host = u.hostname;
  } catch (e) {
    // Not a URL, use as-is
  }
  var port = parseInt(httpPort.value, 10) || DEFAULTS.httpPort;

  testBtn.disabled = true;
  testBtn.textContent = "Testing\u2026";
  testResult.textContent = "";
  testResult.className = "test-result";

  // Step 1: Ensure we have host permission for this target
  var permPromise;
  if (needsOptionalPermission(host)) {
    testResult.textContent = "Requesting permission\u2026";
    permPromise = requestHostPermission(host);
  } else {
    permPromise = Promise.resolve(true);
  }

  permPromise.then(function (granted) {
    if (!granted) {
      testResult.textContent = "\u2717 Permission denied. The extension needs access to HTTP URLs to connect to your Pi.";
      testResult.className = "test-result error";
      testBtn.disabled = false;
      testBtn.textContent = "Test Connection";
      return;
    }

    // Step 2: Route the health check through the background script
    testResult.textContent = "Connecting\u2026";
    chrome.runtime.sendMessage(
      { type: "TEST_CONNECTION", host: host, port: port },
      function (result) {
        if (chrome.runtime.lastError) {
          testResult.textContent = "\u2717 Error: " + chrome.runtime.lastError.message;
          testResult.className = "test-result error";
        } else if (result && result.success) {
          testResult.textContent = "\u2713 " + result.message;
          testResult.className = "test-result success";
        } else {
          testResult.textContent = "\u2717 Connection failed: " + (result ? result.error || "Unknown error" : "No response");
          testResult.className = "test-result error";
        }
        testBtn.disabled = false;
        testBtn.textContent = "Test Connection";
      }
    );
  }).catch(function (err) {
    testResult.textContent = "\u2717 Error: " + err.message;
    testResult.className = "test-result error";
    testBtn.disabled = false;
    testBtn.textContent = "Test Connection";
  });
}

// ─── Event Handlers ────────────────────────────────────────────────

saveBtn.addEventListener("click", saveSettings);
resetBtn.addEventListener("click", resetSettings);
testBtn.addEventListener("click", testConnection);

// ─── Init ──────────────────────────────────────────────────────────

loadSettings();
