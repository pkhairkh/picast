/**
 * PiCast Background Service Worker
 *
 * Manages communication with the PiCast receiver on the Raspberry Pi:
 * - mDNS/HTTP discovery of PiCast devices on the local network
 * - webRequest interception of media URLs (HLS/DASH/direct)
 * - HTTP API client for cast/control operations
 * - WebSocket client for real-time status updates
 * - Badge updates reflecting playback state
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
const MEDIA_CACHE_MAX = 20;
const MEDIA_CACHE_TTL_MS = 5 * 60 * 1000; // 5 minutes

/** Patterns for detecting media URLs in network requests. */
const MEDIA_SIGNATURES = [
  { pattern: /\.m3u8(\?|$)/i, type: "hls", confidence: "high" },
  { pattern: /\.mpd(\?|$)/i, type: "dash", confidence: "high" },
  { pattern: /\.(mp4|webm|mkv|avi|mov)(\?|$)/i, type: "direct", confidence: "high" },
  { pattern: /\.(ts|m4s)(\?|$)/i, type: "segment", confidence: "medium" },
  { pattern: /googlevideo\.com\/videoplayback/i, type: "cdn", confidence: "high" },
  { pattern: /vimeocdn\.com\//i, type: "cdn", confidence: "high" },
  { pattern: /twitch\.tv\/.*\/chunked/i, type: "cdn", confidence: "high" },
];

// ─── State ────────────────────────────────────────────────────────

/** Per-tab detected media URL queues. */
const tabMediaQueues = new Map();

/** Active WebSocket connection. */
let ws = null;
let wsReconnectAttempts = 0;
let wsReconnectTimer = null;

/** Current PiCast status from WebSocket. */
let currentPicastStatus = null;

/** Whether we're currently connected to a PiCast device. */
let isConnected = false;

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

// ─── HTTP API Client ──────────────────────────────────────────────

async function picastApi(endpoint, method = "GET", body = null) {
  const config = await getConfig();
  const url = `${picastHttpBase(config)}${endpoint}`;
  const opts = {
    method,
    headers: { "Content-Type": "application/json" },
  };
  if (body) opts.body = JSON.stringify(body);

  const response = await fetch(url, opts);
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    throw new Error(`PiCast API ${response.status}: ${text || response.statusText}`);
  }
  return response.json();
}

// ─── WebSocket Client ─────────────────────────────────────────────

async function connectWebSocket() {
  if (ws && ws.readyState === WebSocket.OPEN) return;

  const config = await getConfig();
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
    updateBadge("disconnected");
    scheduleWsReconnect();
  };

  ws.onerror = (e) => {
    console.warn("[PiCast] WebSocket error:", e);
    isConnected = false;
    updateBadge("disconnected");
  };
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

function sendWsCommand(command) {
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    console.warn("[PiCast] WebSocket not connected — cannot send command");
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
      currentPicastStatus = msg;
      updateBadge(msg.state?.toLowerCase() || "idle");
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
      const config = getConfig();
      config.then((c) => {
        if (c.showNotifications) {
          chrome.notifications?.create({
            type: "basic",
            iconUrl: "icons/icon-48.png",
            title: "PiCast Error",
            message: msg.message,
          });
        }
      });
      chrome.runtime
        .sendMessage({ type: "ERROR", message: msg.message })
        .catch(() => {});
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
  playing: "▶",
  paused: "⏸",
  buffering: "⏳",
  resolving: "⏳",
  idle: "",
  disconnected: "✕",
  error: "!",
};

function updateBadge(state) {
  const color = BADGE_COLORS[state] || BADGE_COLORS.idle;
  const text = BADGE_TEXT[state] || "";
  chrome.action.setBadgeBackgroundColor({ color });
  chrome.action.setBadgeText({ text });
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

        // Evict old entries.
        while (queue.length > MEDIA_CACHE_MAX) queue.shift();

        // Show badge indicating media was detected.
        chrome.action.setBadgeText({ text: "▶" });
        chrome.action.setBadgeBackgroundColor({ color: "#4CAF50" });
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
  const config = await getConfig();
  const result = await picastApi("/api/cast", "POST", { url });
  return result;
}

async function handlePause() {
  return picastApi("/api/pause", "POST");
}

async function handleResume() {
  return picastApi("/api/resume", "POST");
}

async function handleStop() {
  return picastApi("/api/stop", "POST");
}

async function handleSeek(positionMs) {
  return picastApi("/api/seek", "POST", { position_ms: positionMs });
}

async function handleVolume(volume) {
  return picastApi("/api/volume", "POST", { volume });
}

async function handleStatus() {
  return picastApi("/api/status");
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

    case "GET_STATUS":
      handleStatus()
        .then(sendResponse)
        .catch((err) => sendResponse({ error: err.message }));
      return true;

    case "GET_MEDIA_QUEUE":
      const tabId = message.tabId;
      const queue = (tabMediaQueues.get(tabId) || []).filter(
        (item) => Date.now() - item.timestamp < MEDIA_CACHE_TTL_MS
      );
      sendResponse({ success: true, queue });
      return false; // sync

    case "WS_STATUS":
      sendResponse({ connected: isConnected, status: currentPicastStatus });
      return false;

    case "MEDIA_DETECTED":
      // From content script — a video/audio element was found on the page.
      if (sender.tab && sender.tab.id) {
        const tabId = sender.tab.id;
        if (!tabMediaQueues.has(tabId)) {
          tabMediaQueues.set(tabId, []);
        }
        for (const source of message.sources || []) {
          tabMediaQueues.get(tabId).push({
            url: source.src,
            type: source.type || "detected",
            confidence: "medium",
            timestamp: Date.now(),
          });
        }
        chrome.action.setBadgeText({ text: "▶" });
        chrome.action.setBadgeBackgroundColor({ color: "#4CAF50" });
      }
      sendResponse({ success: true });
      return false;

    default:
      console.warn("[PiCast] Unknown message type:", message.type);
      return false;
  }
});

// ─── Discovery ────────────────────────────────────────────────────

/**
 * Attempt to discover a PiCast device on the local network.
 * Strategy:
 * 1. Try DNS-SD via chrome.dns (Firefox only)
 * 2. Try HTTP health check to picast.local
 * 3. Try HTTP health check to last-known IP
 */
async function discoverPiCast() {
  const config = await getConfig();
  const base = picastHttpBase(config);

  try {
    const response = await fetch(`${base}/api/health`, {
      method: "GET",
      signal: AbortSignal.timeout(3000),
    });
    if (response.ok) {
      const data = await response.json();
      if (data.status === "ok") {
        console.log("[PiCast] Discovered device at", base);
        isConnected = true;
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

// ─── Initialization ───────────────────────────────────────────────

chrome.runtime.onInstalled.addListener(async () => {
  console.log("[PiCast] Extension installed");
  await discoverPiCast();
});

chrome.runtime.onStartup.addListener(async () => {
  console.log("[PiCast] Browser started");
  await discoverPiCast();
});

// Initial discovery attempt.
discoverPiCast();
