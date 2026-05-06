"use strict";
/**
 * PiCast Background Service Worker
 *
 * Manages communication with the PiCast receiver on the Raspberry Pi:
 * - HTTP API client for cast/control operations with retry logic
 * - WebSocket client for real-time status updates with auto-reconnect
 * - webRequest interception of media URLs (HLS/DASH/direct)
 * - Badge updates reflecting playback state
 * - Service worker keep-alive via chrome.alarms
 * - Auto-cast support when media is detected
 * - Browser notifications for errors
 *
 * Compatible with both Chrome (service_worker) and Firefox (background script).
 * Uses the `chrome.*` namespace (Firefox supports it with the
 * `browser_specific_settings.gecko` manifest key).
 */

// ─── Constants ────────────────────────────────────────────────────

const DEFAULT_PICAST_HOST = "picast.local";
const DEFAULT_HTTP_PORT = 8585;
const DEFAULT_WS_PORT = 8586;
const WS_RECONNECT_BASE_MS = 1000;
const WS_RECONNECT_MAX_MS = 30000;
const WS_PING_INTERVAL_MS = 30000;
const MEDIA_CACHE_MAX = 50;
const MEDIA_CACHE_TTL_MS = 5 * 60 * 1000; // 5 minutes
const API_TIMEOUT_MS = 8000;
const API_MAX_RETRIES = 2;
const VOLUME_DEBOUNCE_MS = 300;
const ALARM_KEEPALIVE = "picast-keepalive";
const ALARM_PERIOD_MINUTES = 0.5; // 30 seconds
const STATUS_STALE_MS = 30000;

/** Patterns for detecting media URLs in network requests. */
const MEDIA_SIGNATURES = [
  { pattern: /\.m3u8(\?|$)/i, type: "hls", confidence: "high" },
  { pattern: /\.mpd(\?|$)/i, type: "dash", confidence: "high" },
  { pattern: /\.(mp4|webm|mkv|avi|mov)(\?|$)/i, type: "direct", confidence: "high" },
  { pattern: /\.(ts|m4s)(\?|$)/i, type: "segment", confidence: "medium" },
  { pattern: /googlevideo\.com\/videoplayback/i, type: "cdn", confidence: "high" },
  { pattern: /vimeocdn\.com\//i, type: "cdn", confidence: "high" },
  { pattern: /twitch\.tv\/.*\/chunked/i, type: "cdn", confidence: "high" },
  { pattern: /cdn\.jwplayer\.com\//i, type: "cdn", confidence: "medium" },
  { pattern: /cloudfront\.net\/.*\.m3u8/i, type: "hls", confidence: "medium" },
  { pattern: /akamaized\.net\/.*\.m3u8/i, type: "hls", confidence: "medium" },
  { pattern: /dailymotion\.com\/.*\/dash/i, type: "dash", confidence: "high" },
  { pattern: /soundcloud\.com\/.*\/stream/i, type: "cdn", confidence: "medium" },
];

// ─── State ────────────────────────────────────────────────────────

/** Per-tab detected media URL queues. */
const tabMediaQueues = new Map();

/** Active WebSocket connection. */
let ws = null;
let wsReconnectAttempts = 0;
let wsReconnectTimer = null;
let wsPingTimer = null;
let wsConnecting = false;

/** Current PiCast status from WebSocket. */
let currentPicastStatus = null;

/** Whether we're currently connected to a PiCast device. */
let isConnected = false;

/** Whether we have an active playback session. */
let hasActiveSession = false;

/** Volume debounce timer. */
let volumeDebounceTimer = null;

/** Badge state tracking. */
let currentBadgeState = "idle";
let errorBadgeTime = 0;

/** Guard against concurrent handleCast (double-cast). */
let castInProgress = false;

/** Guard against double initialization. */
let initialized = false;

// ─── State Persistence ──────────────────────────────────────────

async function saveState() {
  try {
    await chrome.storage.session.set({
      hasActiveSession,
      currentBadgeState,
    });
  } catch {}
}

async function loadState() {
  try {
    const data = await chrome.storage.session.get({
      hasActiveSession: false,
      currentBadgeState: "idle",
    });
    hasActiveSession = data.hasActiveSession;
    currentBadgeState = data.currentBadgeState;
    updateBadge(currentBadgeState);
  } catch {}
}

// ─── Config ───────────────────────────────────────────────────────

async function getConfig() {
  return new Promise((resolve) => {
    chrome.storage.local.get(
      {
        piHost: DEFAULT_PICAST_HOST,
        httpPort: DEFAULT_HTTP_PORT,
        wsPort: DEFAULT_WS_PORT,
        torMode: "full",
        showNotifications: true,
        autoCast: false,
        quality: "720p",
      },
      resolve
    );
  });
}

function picastHttpBase(config) {
  return `http://${config.piHost}:${config.httpPort}`;
}

function picastWsUrl(config) {
  return `ws://${config.piHost}:${config.wsPort}/ws`;
}

// ─── URL Validation ──────────────────────────────────────────────

const VALID_URL_PROTOCOLS = ["http:", "https:", "magnet:"];
const YOUTUBE_PATTERNS = [
  /^https?:\/\/(www\.)?youtube\.com\/watch/i,
  /^https?:\/\/youtu\.be\//i,
  /^https?:\/\/(www\.)?youtube\.com\/embed\//i,
  /^https?:\/\/(www\.)?youtube\.com\/shorts\//i,
];

function isValidCastUrl(url) {
  if (!url || typeof url !== "string") return false;
  const trimmed = url.trim();
  if (trimmed.length === 0) return false;

  // Allow magnet links
  if (trimmed.startsWith("magnet:")) return true;

  try {
    const parsed = new URL(trimmed);
    if (!VALID_URL_PROTOCOLS.includes(parsed.protocol)) return false;
    if (!parsed.hostname || parsed.hostname.length === 0) return false;
    return true;
  } catch {
    return false;
  }
}

// ─── HTTP API Client ──────────────────────────────────────────────

/**
 * Make an API call to the PiCast server with retry logic and timeout.
 * Retries on network errors and 5xx responses.
 */
async function picastApi(endpoint, method = "GET", body = null, retries = API_MAX_RETRIES) {
  const config = await getConfig();
  const url = `${picastHttpBase(config)}${endpoint}`;
  const opts = {
    method,
    headers: { "Content-Type": "application/json" },
  };
  if (body) opts.body = JSON.stringify(body);

  for (let attempt = 0; attempt <= retries; attempt++) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), API_TIMEOUT_MS);
    opts.signal = controller.signal;

    try {
      const response = await fetch(url, opts);
      clearTimeout(timeout);

      if (!response.ok) {
        const text = await response.text().catch(() => "");
        // Retry on 5xx errors
        if (response.status >= 500 && attempt < retries) {
          console.warn(`[PiCast] API ${response.status}, retrying (${attempt + 1}/${retries})...`);
          await delay(500 * (attempt + 1));
          continue;
        }
        throw new Error(`PiCast API ${response.status}: ${text || response.statusText}`);
      }
      return response.json();
    } catch (err) {
      clearTimeout(timeout);
      if (err.name === "AbortError") {
        if (attempt < retries) {
          console.warn(`[PiCast] API timeout, retrying (${attempt + 1}/${retries})...`);
          await delay(500 * (attempt + 1));
          continue;
        }
        throw new Error(`PiCast API timeout after ${API_TIMEOUT_MS}ms`);
      }
      // Network errors — retry (Fix 8: Firefox network error retry)
      if (attempt < retries && (err.message.includes("Failed to fetch") || err.message.includes("NetworkError") || err.name === "TypeError")) {
        console.warn(`[PiCast] Network error, retrying (${attempt + 1}/${retries})...`);
        await delay(500 * (attempt + 1));
        continue;
      }
      throw err;
    }
  }
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ─── WebSocket Client ─────────────────────────────────────────────

async function connectWebSocket() {
  // Fix 13: Clear any pending reconnect timer to unblock reschedule
  if (wsReconnectTimer) {
    clearTimeout(wsReconnectTimer);
    wsReconnectTimer = null;
  }
  // Fix 3: Guard against concurrent connects
  if (wsConnecting) return;

  if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;

  wsConnecting = true;
  try {
    const config = await getConfig();

    // Re-check after async gap (Fix 3)
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) return;

    const url = picastWsUrl(config);

    try {
      ws = new WebSocket(url);
    } catch (e) {
      console.warn("[PiCast] WebSocket connect failed:", e);
      scheduleWsReconnect();
      return;
    }

    ws.onopen = () => {
      console.log("[PiCast] WebSocket connected to", url);
      wsReconnectAttempts = 0;
      isConnected = true;
      updateBadge("connected");
      startWsPing();
      // Fix 15: Broadcast WS status changes to popup
      chrome.runtime.sendMessage({ type: "WS_STATUS", connected: true }).catch(() => {});
    };

    ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        handleWsMessage(msg);
      } catch (e) {
        console.warn("[PiCast] Invalid WebSocket message:", e);
      }
    };

    ws.onclose = () => {
      console.log("[PiCast] WebSocket closed");
      isConnected = false;
      stopWsPing();
      // Fix 4: Fix dead ternary
      updateBadge(hasActiveSession ? "disconnected" : "idle");
      // Fix 15: Broadcast WS status changes to popup
      chrome.runtime.sendMessage({ type: "WS_STATUS", connected: false }).catch(() => {});
      scheduleWsReconnect();
    };

    ws.onerror = (e) => {
      console.warn("[PiCast] WebSocket error:", e);
      isConnected = false;
      stopWsPing();
      // Fix 15: Broadcast WS status changes to popup
      chrome.runtime.sendMessage({ type: "WS_STATUS", connected: false }).catch(() => {});
    };
  } finally {
    // Fix 3: Always reset connecting flag
    wsConnecting = false;
  }
}

function scheduleWsReconnect() {
  if (wsReconnectTimer) return;

  const delay = Math.min(
    WS_RECONNECT_BASE_MS * Math.pow(2, wsReconnectAttempts),
    WS_RECONNECT_MAX_MS
  );
  wsReconnectAttempts++;

  console.log(`[PiCast] WebSocket reconnecting in ${delay}ms (attempt ${wsReconnectAttempts})`);
  wsReconnectTimer = setTimeout(async () => {
    wsReconnectTimer = null;
    await connectWebSocket();
  }, delay);
}

function disconnectWebSocket() {
  if (wsReconnectTimer) {
    clearTimeout(wsReconnectTimer);
    wsReconnectTimer = null;
  }
  stopWsPing();
  if (ws) {
    ws.onclose = null; // Prevent reconnect on intentional close
    ws.onerror = null;
    ws.close();
    ws = null;
  }
  isConnected = false;
  wsReconnectAttempts = 0;
}

function startWsPing() {
  stopWsPing();
  wsPingTimer = setInterval(() => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "ping" }));
    }
  }, WS_PING_INTERVAL_MS);
}

function stopWsPing() {
  if (wsPingTimer) {
    clearInterval(wsPingTimer);
    wsPingTimer = null;
  }
}

function sendWsCommand(command) {
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    return false;
  }
  ws.send(JSON.stringify(command));
  return true;
}

function handleWsMessage(msg) {
  switch (msg.type) {
    case "CONNECTED":
      console.log("[PiCast] Server confirmed connection");
      break;

    case "MEDIA_STATUS":
      // Fix 16/18: Add _receivedAt for staleness tracking
      currentPicastStatus = { ...msg, _receivedAt: Date.now() };
      const state = msg.state?.toLowerCase() || "idle";
      hasActiveSession = state !== "idle";
      updateBadge(state);
      // Forward to any open popups
      chrome.runtime
        .sendMessage({ type: "STATUS_UPDATE", status: msg })
        .catch(() => {}); // popup may be closed
      break;

    case "RESOLVE_PROGRESS":
      chrome.runtime
        .sendMessage({ type: "RESOLVE_PROGRESS", percent: msg.percent })
        .catch(() => {});
      break;

    case "ERROR":
      console.error("[PiCast] Server error:", msg.message);
      // Fix 9: Reset hasActiveSession on ERROR
      hasActiveSession = false;
      updateBadge("error");
      saveState();
      getConfig().then((c) => {
        if (c.showNotifications) {
          try {
            chrome.notifications.create({
              type: "basic",
              iconUrl: "icons/icon-48.png",
              title: "PiCast Error",
              message: msg.message || "Unknown error",
            });
          } catch {}
        }
      });
      chrome.runtime
        .sendMessage({ type: "ERROR", message: msg.message })
        .catch(() => {});
      break;

    case "pong":
      // Keep-alive response
      break;

    default:
      console.debug("[PiCast] Unknown WS message type:", msg.type);
  }
}

// ─── Badge Management ─────────────────────────────────────────────

const BADGE_COLORS = {
  connected: "#4CAF50",
  playing: "#4CAF50",
  paused: "#FF9800",
  buffering: "#2196F3",
  resolving: "#9C27B0",
  idle: "#9E9E9E",
  disconnected: "#F44336",
  error: "#F44336",
};

const BADGE_TEXT = {
  connected: "",
  playing: "\u25B6",
  paused: "\u23F8",
  buffering: "\u23F3",
  resolving: "\u23F3",
  idle: "",
  disconnected: "\u2715",
  error: "!",
};

const BADGE_PRIORITY = {
  error: 10,
  playing: 8,
  buffering: 7,
  resolving: 6,
  paused: 5,
  connected: 3,
  disconnected: 2,
  idle: 1,
};

// Fix 17: Cross-browser badge API (also incorporates Fix 5 and Fix 2)
function updateBadge(state) {
  const currentPriority = BADGE_PRIORITY[currentBadgeState] || 0;
  const newPriority = BADGE_PRIORITY[state] || 0;
  const errorExpired = currentBadgeState === "error" && Date.now() - errorBadgeTime > 10000;
  if (newPriority < currentPriority && state !== "idle" && state !== "disconnected" && !errorExpired) {
    return;
  }
  if (state === "error") errorBadgeTime = Date.now();
  currentBadgeState = state;
  saveState();

  const color = BADGE_COLORS[state] || BADGE_COLORS.idle;
  const text = BADGE_TEXT[state] || "";
  const api = chrome.action || chrome.browserAction;
  if (!api) return;
  try {
    api.setBadgeBackgroundColor({ color });
    api.setBadgeText({ text });
  } catch (e) {
    console.warn("[PiCast] Badge update failed:", e);
  }
}

// ─── webRequest Interception ──────────────────────────────────────

chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    // Only intercept media and XHR requests from tabs.
    if (
      details.type !== "media" &&
      details.type !== "xmlhttprequest" &&
      details.type !== "other"
    ) {
      return;
    }

    const url = details.url;
    const tabId = details.tabId;
    if (tabId < 0) return; // Not associated with a tab

    for (const sig of MEDIA_SIGNATURES) {
      if (sig.pattern.test(url)) {
        // Fix 14: Skip segment URLs — they fill the queue and aren't castable on their own
        if (sig.type === "segment") {
          break;
        }

        if (!tabMediaQueues.has(tabId)) {
          tabMediaQueues.set(tabId, []);
        }
        const queue = tabMediaQueues.get(tabId);

        // Deduplicate: don't add the same URL twice in a row.
        if (queue.length > 0 && queue[queue.length - 1].url === url) return;

        queue.push({
          url,
          type: sig.type,
          confidence: sig.confidence,
          timestamp: Date.now(),
        });

        // Fix 14: Evict expired entries
        const now = Date.now();
        while (queue.length > 0 && now - queue[0].timestamp > MEDIA_CACHE_TTL_MS) {
          queue.shift();
        }

        // Evict old entries.
        while (queue.length > MEDIA_CACHE_MAX) queue.shift();

        // Only update badge if we don't have a higher-priority state
        if (!hasActiveSession) {
          updateBadge("connected");
        }
        break;
      }
    }
  },
  { urls: ["<all_urls>"] }
);

// Clean up when tabs close.
chrome.tabs.onRemoved.addListener((tabId) => {
  tabMediaQueues.delete(tabId);
});

// ─── Cast Logic ───────────────────────────────────────────────────

/**
 * Cast a URL to the PiCast receiver.
 * Strategy:
 * 1. Check if a manifest URL was intercepted for this tab → send that
 * 2. Otherwise, send the page URL → PiCast resolves via yt-dlp
 */
async function handleCast(url, title = null) {
  if (!isValidCastUrl(url)) {
    throw new Error("Invalid URL. Please enter a valid http://, https://, or magnet: URL.");
  }

  // Fix 10: Guard against concurrent handleCast (double-cast)
  if (castInProgress) {
    throw new Error("A cast operation is already in progress");
  }

  try {
    castInProgress = true;

    const config = await getConfig();
    const result = await picastApi("/api/cast", "POST", {
      url,
      title,
      torMode: config.torMode,
      quality: config.quality,
    });

    hasActiveSession = true;
    // Fix 2: Persist state after session change
    saveState();
    updateBadge("resolving");

    // If we have a WebSocket, the status updates will come automatically.
    // Otherwise, do a delayed status check.
    if (!isConnected) {
      setTimeout(async () => {
        try {
          const status = await handleStatus();
          if (status) {
            chrome.runtime.sendMessage({ type: "STATUS_UPDATE", status }).catch(() => {});
          }
        } catch {}
      }, 2000);
    }

    return result;
  } finally {
    castInProgress = false;
  }
}

async function handlePause() {
  // Try WebSocket first, fall back to HTTP
  if (sendWsCommand({ type: "PAUSE" })) return { status: "paused" };
  return picastApi("/api/pause", "POST");
}

async function handleResume() {
  if (sendWsCommand({ type: "RESUME" })) return { status: "playing" };
  return picastApi("/api/resume", "POST");
}

async function handleStop() {
  hasActiveSession = false;
  // Fix 2: Persist state after session change
  saveState();
  // Fix 6: Fix handleStop via WS skips badge update
  if (sendWsCommand({ type: "STOP" })) {
    updateBadge("idle");
    return { status: "idle" };
  }
  const result = await picastApi("/api/stop", "POST");
  updateBadge("idle");
  return result;
}

async function handleSeek(positionMs) {
  if (sendWsCommand({ type: "SEEK", position_ms: positionMs })) {
    return { position_ms: positionMs };
  }
  return picastApi("/api/seek", "POST", { position_ms: positionMs });
}

async function handleVolume(volume) {
  // Debounce volume changes
  return new Promise((resolve) => {
    if (volumeDebounceTimer) clearTimeout(volumeDebounceTimer);
    volumeDebounceTimer = setTimeout(async () => {
      try {
        let result;
        if (sendWsCommand({ type: "VOLUME", volume })) {
          result = { volume };
        } else {
          result = await picastApi("/api/volume", "POST", { volume });
        }
        resolve(result);
      } catch (err) {
        resolve({ error: err.message });
      }
    }, VOLUME_DEBOUNCE_MS);
  });
}

async function handleSubtitle(lang) {
  if (sendWsCommand({ type: "SUBTITLE", lang })) return { lang };
  // No HTTP endpoint for subtitles — WS only
  throw new Error("Subtitles require WebSocket connection");
}

// Fix 16: Handle stale status with _receivedAt
async function handleStatus() {
  if (currentPicastStatus && isConnected) {
    const age = Date.now() - (currentPicastStatus._receivedAt || 0);
    if (age < STATUS_STALE_MS) return currentPicastStatus;
  }
  return picastApi("/api/status");
}

// ─── Auto-Cast Logic ──────────────────────────────────────────────

/**
 * When auto-cast is enabled and media is detected on a page,
 * automatically send the page URL to PiCast.
 */
async function maybeAutoCast(tabId) {
  const config = await getConfig();
  if (!config.autoCast) return;

  const queue = tabMediaQueues.get(tabId);
  if (!queue || queue.length === 0) return;

  // Don't auto-cast if already playing
  if (hasActiveSession) return;

  try {
    const tab = await chrome.tabs.get(tabId);
    if (!tab?.url) return;

    // Don't auto-cast browser internal pages
    if (tab.url.startsWith("chrome://") || tab.url.startsWith("about:")) return;

    console.log("[PiCast] Auto-casting:", tab.url);
    await handleCast(tab.url, tab.title);
  } catch (err) {
    console.warn("[PiCast] Auto-cast failed:", err.message);
  }
}

// ─── Message Handling (from popup & content scripts) ──────────────

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  switch (message.type) {
    case "CAST":
      handleCast(message.url, message.title || null)
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true; // async

    case "PAUSE":
      handlePause()
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "RESUME":
      handleResume()
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "STOP":
      handleStop()
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "SEEK":
      handleSeek(message.position_ms)
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "VOLUME":
      handleVolume(message.volume)
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "SUBTITLE":
      handleSubtitle(message.lang)
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "GET_STATUS":
      handleStatus()
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "GET_MEDIA_QUEUE": {
      const tabId = message.tabId;
      const queue = (tabMediaQueues.get(tabId) || []).filter(
        (item) => Date.now() - item.timestamp < MEDIA_CACHE_TTL_MS
      );
      sendResponse({ success: true, queue });
      return false; // sync
    }

    case "WS_STATUS":
      sendResponse({ connected: isConnected, status: currentPicastStatus });
      return false;

    // Fix 7: Fix WS_RECONNECT handler returns connected:false always
    case "WS_RECONNECT":
      disconnectWebSocket();
      connectWebSocket().then(() => {
        // Poll until connection state is known
        const checkInterval = setInterval(() => {
          if (isConnected) {
            clearInterval(checkInterval);
            sendResponse({ success: true, connected: true });
          } else if (ws && ws.readyState === WebSocket.CLOSED) {
            clearInterval(checkInterval);
            sendResponse({ success: false, connected: false });
          }
        }, 100);
        // Safety timeout
        setTimeout(() => {
          clearInterval(checkInterval);
          sendResponse({ success: isConnected, connected: isConnected });
        }, 5000);
      }).catch((err) => {
        sendResponse({ success: false, error: err.message });
      });
      return true;

    case "MEDIA_DETECTED":
      // From content script — a video/audio element was found on the page.
      if (sender.tab && sender.tab.id) {
        const tabId = sender.tab.id;
        if (!tabMediaQueues.has(tabId)) {
          tabMediaQueues.set(tabId, []);
        }
        for (const source of message.sources || []) {
          // Deduplicate
          const existing = tabMediaQueues.get(tabId);
          if (!existing.some((e) => e.url === source.src)) {
            existing.push({
              url: source.src,
              type: source.type || "detected",
              confidence: source.confidence || "medium",
              timestamp: Date.now(),
            });
          }
        }
        if (!hasActiveSession) {
          updateBadge("connected");
        }
        // Attempt auto-cast
        maybeAutoCast(tabId);
      }
      sendResponse({ success: true });
      return false;

    case "DISCOVER":
      discoverPiCast().then((result) => {
        sendResponse({ success: true, discovered: result });
      });
      return true;

    default:
      console.warn("[PiCast] Unknown message type:", message.type);
      return false;
  }
});

// ─── Discovery ────────────────────────────────────────────────────

/**
 * Attempt to discover a PiCast device on the local network.
 * Strategy:
 * 1. Try HTTP health check to configured host
 * 2. Try HTTP health check to last-known IP
 */
async function discoverPiCast() {
  const config = await getConfig();
  const base = picastHttpBase(config);

  try {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 5000);

    const response = await fetch(`${base}/api/health`, {
      method: "GET",
      signal: controller.signal,
    });
    clearTimeout(timeout);

    if (response.ok) {
      const data = await response.json();
      if (data.status === "ok") {
        console.log("[PiCast] Discovered device at", base);
        // Fix 11: Don't set isConnected=true prematurely; ws.onopen handles it
        updateBadge("connected");
        await connectWebSocket();
        return true;
      }
    }
  } catch (e) {
    console.warn("[PiCast] Discovery failed for", base, e.message);
    isConnected = false;
    updateBadge("disconnected");
  }
  return false;
}

// ─── Service Worker Keep-Alive ────────────────────────────────────

/**
 * Chrome kills service workers after ~5 minutes of inactivity.
 * We use chrome.alarms to wake ourselves up periodically and
 * maintain the WebSocket connection.
 */
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === ALARM_KEEPALIVE) {
    // Reconnect WebSocket if needed
    if (!isConnected) {
      connectWebSocket();
    }
    // Refresh discovery if we've never connected
    if (!isConnected && !hasActiveSession) {
      discoverPiCast();
    }
  }
});

async function startKeepAlive() {
  try {
    await chrome.alarms.clear(ALARM_KEEPALIVE);
    chrome.alarms.create(ALARM_KEEPALIVE, {
      periodInMinutes: ALARM_PERIOD_MINUTES,
    });
    console.log("[PiCast] Keep-alive alarm set (every", ALARM_PERIOD_MINUTES, "min)");
  } catch (e) {
    console.warn("[PiCast] Failed to set keep-alive alarm:", e);
  }
}

// ─── Initialization ───────────────────────────────────────────────

// Fix 12: Prevent double initialization on fresh install
async function init() {
  if (initialized) return;
  initialized = true;
  await loadState();
  await startKeepAlive();
  await discoverPiCast();
}

chrome.runtime.onInstalled.addListener(async (details) => {
  console.log("[PiCast] Extension installed:", details.reason);
  await init();
});

chrome.runtime.onStartup.addListener(async () => {
  console.log("[PiCast] Browser started");
  await init();
});

// Start on script load (handles SW restarts too)
init();
