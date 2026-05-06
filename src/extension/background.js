/**
 * PiCast Background Service Worker
 *
 * Intercepts media URLs from web requests and manages communication
 * with the PiCast receiver on the Raspberry Pi. Supports both REST
 * API polling and WebSocket-based real-time status updates.
 *
 * See docs/extension/interception.md for the full interception strategy.
 */

const DEFAULT_PICAST_ADDRESS = "picast.local";
const DEFAULT_PICAST_PORT = 8585;
const MEDIA_SIGNATURES = [
  { pattern: /\.m3u8(\?|$)/i, type: "hls", confidence: "high" },
  { pattern: /\.mpd(\?|$)/i, type: "dash", confidence: "high" },
  { pattern: /\.(mp4|webm|mkv|avi|mov)(\?|$)/i, type: "direct", confidence: "high" },
  { pattern: /\.(ts|m4s)(\?|$)/i, type: "segment", confidence: "medium" },
  { pattern: /googlevideo\.com\/videoplayback/i, type: "cdn", confidence: "high" },
  { pattern: /vimeocdn\.com\//i, type: "cdn", confidence: "high" },
  { pattern: /twitch\.tv\/.*\/chunked/i, type: "cdn", confidence: "high" },
];

// Per-tab media URL queues
const tabMediaQueues = new Map();

// ─── WebSocket Connection ────────────────────────────────────

let ws = null;
let wsReconnectTimer = null;
let wsConnected = false;
let lastKnownStatus = null;

// How often to attempt reconnection (ms)
const WS_RECONNECT_INTERVAL = 5000;

/**
 * Build the WebSocket URL from the current PiCast config.
 */
async function getWsUrl() {
  const config = await getPicastConfig();
  const host = config.piAddress || DEFAULT_PICAST_ADDRESS;
  const port = config.piPort || DEFAULT_PICAST_PORT;
  return `ws://${host}:${port}`;
}

/**
 * Connect to the PiCast WebSocket server and set up event handlers.
 *
 * On connection, receives a CONNECTED event. The server then pushes
 * MEDIA_STATUS, RESOLVE_PROGRESS, and ERROR events as they happen.
 */
async function connectWebSocket() {
  // Don't double-connect
  if (ws && ws.readyState === WebSocket.OPEN) return;

  const wsUrl = await getWsUrl();

  try {
    ws = new WebSocket(wsUrl);

    ws.onopen = () => {
      wsConnected = true;
      console.log("[PiCast] WebSocket connected to", wsUrl);
      // Notify all popup pages about the connection state
      broadcastToPopups({ type: "WS_STATUS", connected: true });
    };

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data);
        handleWsEvent(data);
      } catch (err) {
        console.warn("[PiCast] Failed to parse WebSocket message:", err);
      }
    };

    ws.onclose = () => {
      wsConnected = false;
      console.log("[PiCast] WebSocket disconnected");
      broadcastToPopups({ type: "WS_STATUS", connected: false });
      scheduleReconnect();
    };

    ws.onerror = (err) => {
      console.warn("[PiCast] WebSocket error:", err);
      // onclose will fire after onerror, which handles reconnect
    };
  } catch (err) {
    console.warn("[PiCast] WebSocket connection failed:", err);
    scheduleReconnect();
  }
}

/**
 * Schedule a WebSocket reconnection attempt.
 */
function scheduleReconnect() {
  if (wsReconnectTimer) return; // Already scheduled
  wsReconnectTimer = setTimeout(async () => {
    wsReconnectTimer = null;
    await connectWebSocket();
  }, WS_RECONNECT_INTERVAL);
}

/**
 * Handle a parsed WebSocket event from the PiCast server.
 *
 * Server events:
 * - CONNECTED: Initial connection confirmed
 * - MEDIA_STATUS: Player state change (playing, paused, idle, etc.)
 * - RESOLVE_PROGRESS: URL resolution progress (percent)
 * - ERROR: Error message from server
 */
function handleWsEvent(event) {
  switch (event.type) {
    case "CONNECTED":
      console.log("[PiCast] Server confirmed WebSocket connection");
      break;

    case "MEDIA_STATUS":
      lastKnownStatus = {
        state: event.state,
        position_ms: event.position_ms,
        duration_ms: event.duration_ms,
        volume: event.volume,
        source_url: event.source_url,
        title: event.title,
      };
      broadcastToPopups({ type: "STATUS_UPDATE", status: lastKnownStatus });
      break;

    case "RESOLVE_PROGRESS":
      broadcastToPopups({
        type: "RESOLVE_PROGRESS",
        percent: event.percent,
      });
      break;

    case "ERROR":
      broadcastToPopups({ type: "ERROR", message: event.message });
      break;

    default:
      console.debug("[PiCast] Unknown WS event type:", event.type);
  }
}

/**
 * Send a command through the WebSocket if connected.
 *
 * Falls back to REST API if the WebSocket is not available.
 *
 * @param {string} type - Command type (CAST, STOP, PAUSE, RESUME, SEEK, VOLUME)
 * @param {object} data - Additional command data
 * @returns {Promise<object>} - Response from the server
 */
async function sendWsCommand(type, data = {}) {
  if (ws && ws.readyState === WebSocket.OPEN) {
    const command = { type, ...data };
    ws.send(JSON.stringify(command));
    // WebSocket commands are fire-and-forget; the server will push
    // a MEDIA_STATUS event back when the command takes effect.
    return { success: true };
  }

  // Fallback to REST API
  return sendRestCommand(type, data);
}

/**
 * Send a command via the REST API (fallback when WebSocket unavailable).
 */
async function sendRestCommand(type, data = {}) {
  switch (type) {
    case "CAST":
      return handleCast({ url: data.url, title: data.title, torMode: data.torMode });
    case "STOP":
      return picastApi("/api/stop", "POST");
    case "PAUSE":
      return picastApi("/api/pause", "POST");
    case "RESUME":
      return picastApi("/api/resume", "POST");
    case "SEEK":
      return picastApi("/api/seek", "POST", { position_ms: data.position_ms });
    case "VOLUME":
      return picastApi("/api/volume", "POST", { volume: data.volume });
    default:
      throw new Error(`Unknown command type: ${type}`);
  }
}

/**
 * Broadcast a message to all open popup pages.
 *
 * Uses chrome.runtime.sendMessage which reaches all extension pages
 * (popups, options, content scripts that are listening).
 */
function broadcastToPopups(message) {
  try {
    chrome.runtime.sendMessage(message).catch(() => {
      // No listeners — popup is likely closed. This is normal.
    });
  } catch {
    // Extension context invalidated or no listeners
  }
}

/**
 * Disconnect the WebSocket and stop reconnection attempts.
 */
function disconnectWebSocket() {
  if (wsReconnectTimer) {
    clearTimeout(wsReconnectTimer);
    wsReconnectTimer = null;
  }
  if (ws) {
    ws.onclose = null; // Prevent reconnect on deliberate close
    ws.close();
    ws = null;
  }
  wsConnected = false;
}

// ─── WebRequest Interception ──────────────────────────────────

chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (details.type !== "media" && details.type !== "xmlhttprequest" && details.type !== "other") {
      return;
    }

    const url = details.url;
    const tabId = details.tabId;
    if (tabId < 0) return; // Ignore requests not associated with a tab

    for (const sig of MEDIA_SIGNATURES) {
      if (sig.pattern.test(url)) {
        if (!tabMediaQueues.has(tabId)) {
          tabMediaQueues.set(tabId, []);
        }
        const queue = tabMediaQueues.get(tabId);
        queue.push({
          url,
          type: sig.type,
          confidence: sig.confidence,
          timestamp: Date.now(),
        });
        // Keep only last 20 entries per tab
        if (queue.length > 20) queue.shift();
        break;
      }
    }
  },
  { urls: ["<all_urls>"] }
);

// Clean up when tabs close
chrome.tabs.onRemoved.addListener((tabId) => {
  tabMediaQueues.delete(tabId);
});

// ─── Message Handling ─────────────────────────────────────────

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message.type === "CAST") {
    sendWsCommand("CAST", { url: message.url, title: message.title, torMode: message.torMode })
      .then(sendResponse)
      .catch((err) => {
        sendResponse({ success: false, error: err.message });
      });
    return true; // async response
  }

  if (message.type === "GET_MEDIA_QUEUE") {
    const tabId = message.tabId;
    const queue = tabMediaQueues.get(tabId) || [];
    sendResponse({ success: true, queue });
    return false;
  }

  if (message.type === "GET_STATUS") {
    // If WebSocket is connected, return last known status immediately
    if (wsConnected && lastKnownStatus) {
      sendResponse({ success: true, ...lastKnownStatus });
      return false;
    }
    // Fall back to REST API
    getPicastStatus().then(sendResponse).catch((err) => {
      sendResponse({ success: false, error: err.message });
    });
    return true;
  }

  if (message.type === "PAUSE") {
    sendWsCommand("PAUSE").then(sendResponse).catch((err) => {
      sendResponse({ success: false, error: err.message });
    });
    return true;
  }

  if (message.type === "RESUME") {
    sendWsCommand("RESUME").then(sendResponse).catch((err) => {
      sendResponse({ success: false, error: err.message });
    });
    return true;
  }

  if (message.type === "STOP") {
    sendWsCommand("STOP").then(sendResponse).catch((err) => {
      sendResponse({ success: false, error: err.message });
    });
    return true;
  }

  if (message.type === "SEEK") {
    sendWsCommand("SEEK", { position_ms: message.position_ms })
      .then(sendResponse)
      .catch((err) => {
        sendResponse({ success: false, error: err.message });
      });
    return true;
  }

  if (message.type === "VOLUME") {
    sendWsCommand("VOLUME", { volume: message.volume })
      .then(sendResponse)
      .catch((err) => {
        sendResponse({ success: false, error: err.message });
      });
    return true;
  }

  if (message.type === "WS_STATUS") {
    sendResponse({ connected: wsConnected });
    return false;
  }

  if (message.type === "WS_RECONNECT") {
    disconnectWebSocket();
    connectWebSocket().then(() => {
      sendResponse({ success: true, connected: wsConnected });
    }).catch((err) => {
      sendResponse({ success: false, error: err.message });
    });
    return true;
  }
});

// ─── PiCast API Communication ─────────────────────────────────

async function getPicastConfig() {
  const data = await chrome.storage.local.get({
    piAddress: DEFAULT_PICAST_ADDRESS,
    piPort: DEFAULT_PICAST_PORT,
    torMode: "full",
  });
  return data;
}

async function picastApi(endpoint, method = "GET", body = null) {
  const config = await getPicastConfig();
  const url = `http://${config.piAddress}:${config.piPort}${endpoint}`;
  const options = {
    method,
    headers: { "Content-Type": "application/json" },
  };
  if (body) options.body = JSON.stringify(body);
  const response = await fetch(url, options);
  if (!response.ok) throw new Error(`PiCast API error: ${response.status}`);
  return response.json();
}

async function handleCast(message) {
  const config = await getPicastConfig();
  return picastApi("/api/cast", "POST", {
    url: message.url,
    title: message.title || null,
    torMode: message.torMode || config.torMode,
  });
}

async function getPicastStatus() {
  return picastApi("/api/status");
}

// ─── Extension Lifecycle ──────────────────────────────────────

// Connect WebSocket when the service worker starts
connectWebSocket();

// Reconnect when the extension wakes up from idle
chrome.runtime.onStartup.addListener(() => {
  connectWebSocket();
});

// Handle service worker suspend — Chrome MV3 may kill the worker.
// The WS will be cleaned up automatically, and we reconnect on wake.
