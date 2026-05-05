/**
 * PiCast Background Service Worker
 *
 * Intercepts media URLs from web requests and manages communication
 * with the PiCast receiver on the Raspberry Pi.
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
    handleCast(message).then(sendResponse).catch((err) => {
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
    getPicastStatus().then(sendResponse).catch((err) => {
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
