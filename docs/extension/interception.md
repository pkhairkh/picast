# Browser Extension: Request Interception and MSE Capture

The boGDan browser extension intercepts web page network requests and captures Media Source Extensions (MSE) streams to extract playable media URLs. This document describes the interception mechanism, MSE capture strategy, and the data flow from page detection to boGDan casting.

## Overview

Many modern video websites (YouTube, Twitch, Vimeo) do not expose direct media URLs in the page source. Instead, they use JavaScript-based players that fetch video segments dynamically via the Media Source Extensions (MSE) API. boGDan's browser extension intercepts these requests to extract the manifest URL (HLS `.m3u8` or DASH `.mpd`) or individual segment URLs, then sends them to the boGDan receiver for server-side resolution and playback.

```
Web Page (YouTube, etc.)
│
│  JavaScript player creates MediaSource
│  and appends SourceBuffer segments
│
├──▶ MSE Capture (content script + webRequest)
│     │
│     │  Intercept network requests
│     │  Extract manifest URL (.m3u8 / .mpd)
│     │  OR extract segment URLs
│     │
│     ▼
│  Service Worker
│     │
│     │  POST /api/v1/cast with extracted URL
│     │
│     ▼
│  boGDan Receiver
│     │
│     │  Resolve URL via yt-dlp (if page URL)
│     │  OR play directly (if manifest URL)
│     │
│     ▼
│  GStreamer → V4L2 H.264 HW Decode → DRM/KMS → HDMI
```

## Interception Strategy

The extension uses a two-layer interception strategy: URL-based interception for simple cases and MSE capture for complex players.

### Layer 1: URL-Based Casting (Simple)

For pages with obvious media URLs (direct `<video src="...">`, YouTube watch pages with extractable video IDs), the extension simply sends the page URL to boGDan and lets the server-side resolver (yt-dlp) handle URL extraction. This is the preferred approach because:

- It requires no special permissions beyond `activeTab`
- It works with all 1,800+ sites supported by yt-dlp
- The URL resolution happens through Tor, preserving privacy
- No content script complexity

```javascript
// Simple cast: just send the page URL
async function castCurrentTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const response = await fetch(`http://${bogdanAddr}:8585/api/cast`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      url: tab.url,
      title: tab.title,
      quality: storedQuality || '720p'
    })
  });
  return response.json();
}
```

### Layer 2: MSE Capture (Advanced)

For sites where yt-dlp cannot extract the media URL (e.g., sites with sophisticated anti-bot JavaScript, or sites where the manifest URL is generated dynamically and not easily extractable), the extension captures the manifest URL directly from the browser's network requests.

This requires the `webRequest` permission and operates by monitoring the browser's network stack for requests matching known media patterns.

## webRequest Interception

The `chrome.webRequest` API allows the extension to observe network requests before they are sent. boGDan uses this to detect HLS manifest requests (`.m3u8` URLs) and DASH manifest requests (`.mpd` URLs).

### Manifest URL Detection

```javascript
// Background service worker
chrome.webRequest.onBeforeRequest.addListener(
  function(details) {
    const url = new URL(details.url);

    // Detect HLS manifest requests
    if (url.pathname.endsWith('.m3u8') ||
        url.pathname.includes('.m3u8?')) {
      handleManifestDetected(details.url, 'hls');
      return;
    }

    // Detect DASH manifest requests
    if (url.pathname.endsWith('.mpd') ||
        url.pathname.includes('.mpd?')) {
      handleManifestDetected(details.url, 'dash');
      return;
    }

    // Detect direct media file requests
    if (url.pathname.endsWith('.mp4') ||
        url.pathname.endsWith('.webm') ||
        url.pathname.endsWith('.mkv')) {
      handleDirectMediaDetected(details.url);
      return;
    }
  },
  { urls: ["<all_urls>"] },
  []
);

function handleManifestDetected(manifestUrl, type) {
  // Store the manifest URL for the current tab
  // When the user clicks "Cast", send this URL instead of the page URL
  chrome.storage.session.set({
    [tabId]: { manifestUrl, type, detected: Date.now() }
  });

  // Update badge to show "castable media detected"
  chrome.action.setBadgeText({ text: '▶' });
  chrome.action.setBadgeBackgroundColor({ color: '#4CAF50' });
}
```

### URL Pattern Matching

| Pattern | Type | Description |
|---------|------|-------------|
| `*.m3u8*` | HLS manifest | Master or media playlist for HTTP Live Streaming |
| `*.mpd*` | DASH manifest | Media Presentation Description for MPEG-DASH |
| `*.mp4` | Direct media | MP4 container with H.264 video |
| `*.webm` | Direct media | WebM container with VP8/VP9 video |
| `*.m4s` | DASH segment | MPEG-DASH initialization or media segment |
| `*.ts` | HLS segment | MPEG-TS segment from HLS stream |
| `*videoplayback*` | YouTube | Google Video CDN playback URL |
| `*googlevideo.com/videoplayback*` | YouTube | Direct YouTube video stream URL |

## MSE (Media Source Extensions) Capture

MSE is the most challenging case for URL extraction. Sites like YouTube use MSE to dynamically construct media streams from individual segments, and the manifest URL may not be directly observable in network requests (it may be embedded in JavaScript or generated dynamically).

### MSE Detection

The content script monitors for MediaSource usage on the page:

```javascript
// Content script: detector.js
// Runs at document_idle on all pages

function detectMSE() {
  // Method 1: Monitor MediaSource creation
  const originalMediaSource = window.MediaSource;
  if (originalMediaSource) {
    const originalAddSourceBuffer = originalMediaSource.prototype.addSourceBuffer;

    originalMediaSource.prototype.addSourceBuffer = function(mimeType) {
      // Report the MIME type to the service worker
      chrome.runtime.sendMessage({
        type: 'mse_source_buffer',
        mimeType: mimeType,
        mediaSourceReadyState: this.readyState
      });

      return originalAddSourceBuffer.call(this, mimeType);
    };
  }

  // Method 2: Check existing video elements
  document.querySelectorAll('video').forEach(video => {
    if (video.srcObject instanceof MediaStream) {
      chrome.runtime.sendMessage({
        type: 'mse_detected',
        srcType: 'MediaStream'
      });
    }
    if (video.src && video.src.startsWith('blob:')) {
      chrome.runtime.sendMessage({
        type: 'mse_detected',
        srcType: 'blob',
        src: video.src
      });
    }
  });
}

detectMSE();

// Also monitor for dynamically added video elements
const observer = new MutationObserver(mutations => {
  mutations.forEach(mutation => {
    mutation.addedNodes.forEach(node => {
      if (node.nodeName === 'VIDEO' || node.nodeName === 'AUDIO') {
        detectMSE();
      }
    });
  });
});
observer.observe(document.body, { childList: true, subtree: true });
```

### MSE Capture Strategy

When MSE is detected, the extension uses one of three strategies to obtain a castable URL:

| Strategy | When Used | How It Works |
|----------|-----------|-------------|
| **Page URL → yt-dlp** | yt-dlp supports the site | Send the page URL to boGDan. The server-side resolver invokes yt-dlp to extract the manifest URL. Works for 1,800+ sites. |
| **Manifest URL interception** | Manifest URL visible in network requests | The webRequest listener captures the `.m3u8` or `.mpd` URL from network requests. Send this URL directly to boGDan for GStreamer playback. |
| **Fallback: page URL only** | MSE site not supported by yt-dlp, manifest not intercepted | Send the page URL anyway. boGDan will attempt yt-dlp resolution, and if it fails, report an error to the user suggesting they try VLC/DLNA instead. |

### Why Not Capture Individual Segments?

Capturing individual MSE segments (`.m4s`, `.ts` files) and reassembling them on boGDan would require:

1. Intercepting every segment request (high overhead, may break playback)
2. Reassembling segments into a valid stream (complex, error-prone)
3. Maintaining sync between video and audio segments
4. Handling encryption (some sites use Widevine or other DRM for segment-level encryption)

This approach is fragile and unnecessary because boGDan has yt-dlp for server-side resolution. The preferred approach is always: **send the page URL to boGDan and let yt-dlp resolve it**. MSE capture is only needed for the rare case where yt-dlp fails and the manifest URL is directly observable.

## Data Flow: Cast Request

The complete data flow from the user clicking "Cast" to playback starting:

```
1. User clicks "Cast to boGDan" button in extension popup

2. Popup sends message to service worker:
   chrome.runtime.sendMessage({ type: 'cast', tabId: tab.id })

3. Service worker determines the best URL:
   a. Check if a manifest URL was intercepted for this tab
   b. If yes: use the manifest URL
   c. If no: use the page URL (tab.url)

4. Service worker sends HTTP POST to boGDan:
   POST http://<bogdan-ip>:8585/api/cast
   {
     "url": "<manifest-or-page-url>",
     "title": "<tab-title>",
     "quality": "<user-preference>"
   }

5. boGDan receives the request:
   a. If manifest URL: skip yt-dlp, build GStreamer pipeline directly
   b. If page URL: classify → resolve via yt-dlp → build pipeline

6. Service worker connects WebSocket for status:
   ws://<bogdan-ip>:8586/ws

7. Extension popup shows playback state:
   - "Resolving..." (yt-dlp running)
   - "Loading..." (pipeline construction)
   - "Playing" (stream active)
   - "Paused" / "Buffering" / "Error"

8. User can control playback from the popup:
   - Pause/Resume button
   - Seek slider
   - Volume control
   - Stop button
```

## Security Considerations

### Permission Minimization

The extension requests `webRequest` only for MSE capture (optional permission). Users who don't need MSE capture can deny this permission and use the simpler URL-based casting approach.

### No Content Access

The extension does NOT read page content (DOM, cookies, form data). It only:
- Reads the active tab's URL and title (`activeTab` permission)
- Observes network request URLs (`webRequest` permission, optional)
- Sends URLs to the boGDan receiver on the local network

### Network Scope

All boGDan communication is restricted to:
- `http://*.local:8585` (HTTP API)
- `ws://*.local:8586` (WebSocket)

The extension does NOT send data to any external server. All communication is local-network-only.

### No Credential Leakage

The extension does not capture or forward cookies, authentication tokens, or other credentials to boGDan. Media URL resolution happens through Tor on the boGDan device, using the Pi's own network identity (Tor exit relay IP), not the browser's identity.

## Limitations

- **DRM content**: Cannot intercept or cast DRM-protected streams (Netflix, Disney+). The MSE segments are encrypted, and boGDan does not support Widevine CDM.
- **Blob URLs**: `blob:` URLs created by MSE cannot be sent to boGDan — they are only valid within the browser's memory. The extension must intercept the underlying manifest URL instead.
- **CORS restrictions**: The `webRequest` API can observe request URLs but cannot read response bodies. This means the extension cannot capture manifest content — only the manifest URL.
- **Manifest V3 limitations**: The `webRequest` API in MV3 can only observe requests (not modify or block them). For request modification, `declarativeNetRequest` must be used with pre-declared rules.
