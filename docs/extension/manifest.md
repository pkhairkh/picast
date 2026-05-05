# Browser Extension Manifest

The PiCast browser extension is a Manifest V3 extension for Chrome and Firefox that enables users to cast web page URLs directly from the browser to a PiCast receiver on the local network. This document specifies the manifest.json structure, permissions, background service worker, content scripts, and the discovery mechanism.

## Manifest V3 Structure

The extension uses Manifest V3, which is the current standard for both Chrome and Firefox extensions. Manifest V3 replaces background pages with service workers, imposes stricter content security policies, and requires declarative network request handling via `declarativeNetRequest` instead of `webRequest` blocking.

### manifest.json

```json
{
  "manifest_version": 3,
  "name": "PiCast",
  "version": "0.1.0",
  "description": "Cast web videos to PiCast receiver through Tor",

  "icons": {
    "16": "icons/icon-16.png",
    "32": "icons/icon-32.png",
    "48": "icons/icon-48.png",
    "128": "icons/icon-128.png"
  },

  "action": {
    "default_popup": "popup/popup.html",
    "default_icon": {
      "16": "icons/icon-16.png",
      "32": "icons/icon-32.png"
    },
    "default_title": "Cast to PiCast"
  },

  "background": {
    "service_worker": "background/service-worker.js",
    "type": "module"
  },

  "permissions": [
    "activeTab",
    "storage",
    "dns"
  ],

  "optional_permissions": [
    "webRequest",
    "webRequestBlocking",
    "declarativeNetRequest"
  ],

  "host_permissions": [
    "http://*.local:8080/*",
    "ws://*.local:8081/*"
  ],

  "content_scripts": [
    {
      "matches": ["<all_urls>"],
      "js": ["content/detector.js"],
      "run_at": "document_idle"
    }
  ],

  "options_ui": {
    "page": "options/options.html",
    "open_in_tab": false
  },

  "web_accessible_resources": [
    {
      "resources": ["icons/*"],
      "matches": ["<all_urls>"]
    }
  ]
}
```

## Permissions Explained

Each permission is requested for a specific purpose. The extension follows the principle of least privilege — only requesting permissions that are strictly necessary for its functionality.

| Permission | Purpose | Why Required |
|------------|---------|--------------|
| `activeTab` | Access the URL and title of the currently active tab | Needed to get the page URL when the user clicks the cast button. Does NOT grant access to tab content. |
| `storage` | Persist extension settings (PiCast device IP, quality preference) | Stores the last-known PiCast device address and user preferences across browser sessions. |
| `dns` | mDNS service discovery on the local network | Used to discover PiCast devices advertising `_picast._tcp` via mDNS/DNS-SD without requiring the user to manually enter the IP address. |
| `webRequest` (optional) | Intercept media requests for MSE capture | Used in advanced mode to intercept video/audio requests made by the page's media player. Requires separate user opt-in. |
| `declarativeNetRequest` (optional) | Modify request headers declaratively | Future use for adding custom headers to PiCast requests. Manifest V3 requires declarative rules instead of blocking webRequest. |

### host_permissions

| Pattern | Purpose |
|---------|---------|
| `http://*.local:8080/*` | Access the PiCast HTTP API on mDNS-resolved addresses |
| `ws://*.local:8081/*` | Connect to the PiCast WebSocket for real-time status |

These are scoped to `.local` mDNS hostnames on the PiCast ports (8080, 8081). The extension does NOT request access to arbitrary HTTP/WebSocket servers.

## Background Service Worker

The Manifest V3 background script runs as a service worker (not a persistent background page). It handles:

1. **mDNS Discovery**: Periodically queries `_picast._tcp` to discover PiCast devices on the local network.
2. **Cast Requests**: When the user clicks "Cast", the service worker sends a `POST /api/v1/cast` request to the discovered PiCast device.
3. **WebSocket Connection**: Maintains a WebSocket connection to the PiCast device for real-time status updates (playback state, position, errors).
4. **Badge Updates**: Updates the extension icon badge to reflect the current playback state (playing, paused, buffering, idle).

### Service Worker Lifecycle

```
Extension installed / browser started:
  1. Service worker activates
  2. Discover PiCast devices via mDNS
  3. If device found: connect WebSocket, update badge
  4. If no device: show "No PiCast found" badge

User clicks "Cast":
  1. Get active tab URL and title
  2. POST /api/v1/cast with {url, title, quality}
  3. Update badge to "casting" state
  4. Monitor WebSocket for state changes

WebSocket disconnects:
  1. Attempt reconnection with exponential backoff
  2. Show "disconnected" badge
  3. On reconnect: push current state immediately
```

## Content Script: Media Detector

The content script (`content/detector.js`) runs on all pages at `document_idle`. It detects media elements on the page and reports them to the service worker for potential casting.

### Detection Strategy

```javascript
// Detect <video> and <audio> elements
const mediaElements = document.querySelectorAll('video, audio');

// Detect Media Source Extensions (MSE) usage
if (window.MediaSource) {
  // Monitor MediaSource instances for active source buffers
}

// Detect YouTube-style player APIs
if (window.ytplayer || document.querySelector('#movie_player')) {
  // Extract video URL from YouTube player API
}

// Report detected media to service worker
chrome.runtime.sendMessage({
  type: 'media_detected',
  sources: [
    { type: 'video', src: 'https://...', mimeType: 'video/mp4' },
    { type: 'mse', src: null, mimeType: 'application/x-mpegURL' }
  ]
});
```

### Media Types Detected

| Type | Detection Method | Cast Strategy |
|------|-----------------|---------------|
| Direct `<video src="">` | `HTMLMediaElement.src` property | Send URL directly to PiCast HTTP API |
| `<video>` with `<source>` children | `HTMLSourceElement.src` | Send source URL to PiCast |
| MSE (MediaSource Extensions) | `MediaSource.readyState` property | Intercept network requests (requires `webRequest` permission) |
| YouTube embed | `#movie_player` element + `ytplayer` API | Extract video ID, construct YouTube URL, send to PiCast |
| Vimeo embed | `iframe[src*="vimeo.com"]` | Extract video ID from iframe src |
| HLS manifest | `.m3u8` URL in network requests | Send manifest URL directly to PiCast |

## Options Page

The options page allows users to configure:

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| PiCast Address | string | (auto-discovered) | Manual IP address override if mDNS discovery fails |
| Default Quality | select | 720p | Preferred quality tier for new cast sessions |
| Tor Mode | select | full | "full" (all traffic through Tor), "resolution-only" (only yt-dlp via Tor), "off" |
| Auto-cast | checkbox | disabled | Automatically cast when a media element is detected on the page |
| Show Notifications | checkbox | enabled | Show browser notifications for state changes and errors |

## Extension Architecture

```
Browser Extension
│
├── manifest.json              ← Extension manifest (Manifest V3)
│
├── background/
│   └── service-worker.js      ← Background service worker
│       ├── mDNS discovery     ← Discovers PiCast devices on LAN
│       ├── HTTP API client    ← Sends cast/control requests
│       ├── WebSocket client   ← Receives real-time status
│       └── Badge manager      ← Updates extension icon
│
├── content/
│   └── detector.js            ← Content script (runs on all pages)
│       ├── Media detection    ← Finds <video>, <audio>, MSE
│       └── Message passing    ← Reports to service worker
│
├── popup/
│   ├── popup.html             ← Cast button popup UI
│   └── popup.js               ← Popup logic (cast, control, status)
│
├── options/
│   ├── options.html           ← Settings page
│   └── options.js             ← Settings logic
│
└── icons/
    ├── icon-16.png
    ├── icon-32.png
    ├── icon-48.png
    └── icon-128.png
```

## Cross-Browser Compatibility

The extension targets both Chrome and Firefox using the WebExtension API (shared subset). Known differences:

| Feature | Chrome | Firefox | Handling |
|---------|--------|---------|----------|
| Service worker background | Supported | Supported (MV3) | Use `chrome.runtime` API |
| `chrome.dns` | Not available | Available | Fall back to HTTP discovery on Chrome |
| `browser.` vs `chrome.` namespace | `chrome.*` | `browser.*` (promises) | Use `chrome.*` with polyfill for promises |
| `declarativeNetRequest` | Supported | Partially supported | Feature-detect and fall back |
| Content Security Policy | Stricter in MV3 | Moderate | Use `world: MAIN` for page context scripts |
