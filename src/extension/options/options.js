/**
 * boGDan Options Page Script
 *
 * Manages the settings UI: load/save/reset preferences,
 * test connection to boGDan receiver.
 */

"use strict";

var DEFAULTS = {
  piHost: "bogdan.local",
  httpPort: 8585,
  wsPort: 8586,
  torMode: "full",
  showNotifications: true,
  autoCast: false,
  quality: "720p",
  audioDevice: "",
};

// ─── DOM Elements ──────────────────────────────────────────────────

var piHost = document.getElementById("piHost");
var httpPort = document.getElementById("httpPort");
var wsPort = document.getElementById("wsPort");
var quality = document.getElementById("quality");
var audioDevice = document.getElementById("audioDevice");
var torMode = document.getElementById("torMode");
var showNotifications = document.getElementById("showNotifications");
var autoCast = document.getElementById("autoCast");
var testBtn = document.getElementById("testBtn");
var testResult = document.getElementById("testResult");
var refreshDevicesBtn = document.getElementById("refreshDevicesBtn");
var refreshResult = document.getElementById("refreshResult");
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
    // Load audio device — populate list first, then set value
    loadAudioDevices(function () {
      audioDevice.value = settings.audioDevice || "";
    });
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
    audioDevice: audioDevice.value,
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

    // Send audio device to boGDan server so next playback uses it
    if (settings.audioDevice !== undefined) {
      chrome.runtime.sendMessage(
        { type: "SET_AUDIO_DEVICE", device: settings.audioDevice },
        function (result) {
          if (result && !result.error) {
            console.log("[boGDan] Audio device updated on server:", settings.audioDevice);
          } else {
            console.warn("[boGDan] Failed to update audio device on server:", result?.error);
          }
        }
      );
    }

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

  // Route health check through background script to avoid
  // Firefox CSP/CORS issues with fetch() from extension pages
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
}

// ─── Event Handlers ────────────────────────────────────────────────

saveBtn.addEventListener("click", saveSettings);
resetBtn.addEventListener("click", resetSettings);
testBtn.addEventListener("click", testConnection);
refreshDevicesBtn.addEventListener("click", function () {
  loadAudioDevices(function () {
    refreshResult.textContent = "Devices refreshed!";
    refreshResult.className = "test-result success";
    setTimeout(function () { refreshResult.textContent = ""; }, 2000);
  });
});

// ─── Load Audio Devices from Pi ─────────────────────────────────────

function loadAudioDevices(callback) {
  var host = piHost.value.trim() || DEFAULTS.piHost;
  try {
    var u = new URL(host.startsWith("http") ? host : "http://" + host);
    host = u.hostname;
  } catch (e) {}
  var port = parseInt(httpPort.value, 10) || DEFAULTS.httpPort;

  chrome.runtime.sendMessage(
    { type: "FETCH_AUDIO_DEVICES", host: host, port: port },
    function (result) {
      if (chrome.runtime.lastError || !result || !result.devices) {
        // Failed to fetch — keep existing options
        if (callback) callback();
        return;
      }
      populateAudioDeviceSelect(result.devices);
      if (callback) callback();
    }
  );
}

function populateAudioDeviceSelect(devices) {
  // Preserve current selection
  var current = audioDevice.value;
  // Clear all except default
  audioDevice.innerHTML = '<option value="">Default</option>';
  devices.forEach(function (d) {
    var opt = document.createElement("option");
    opt.value = d.device;
    opt.textContent = d.card_name + " (" + d.device + ")";
    audioDevice.appendChild(opt);
  });
  // Restore selection
  if (current) {
    audioDevice.value = current;
  }
}

// ─── Init ──────────────────────────────────────────────────────────

loadSettings();
