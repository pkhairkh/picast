# PiCast Browser Extension

Cast any web video to your PiCast receiver on Raspberry Pi — all traffic routed through Tor for privacy.

## Installation

### Chrome

1. Run `bash build.sh chrome` to create the Chrome build
2. Open `chrome://extensions/`
3. Enable "Developer mode"
4. Click "Load unpacked" → select `build/picast-chrome/`

### Firefox

1. Run `bash build.sh firefox` to create the Firefox build
2. Open `about:debugging#/runtime/this-firefox`
3. Click "Load Temporary Add-on" → select `build/picast-firefox/manifest.json`

**Note:** Firefox temporary add-ons are removed when the browser closes. For permanent installation, the extension must be signed by Mozilla (see [Signing](#signing)).

## Features

- **One-click casting**: Click the PiCast icon to cast the current tab's URL
- **Media detection**: Automatically detects video/audio elements and HLS/DASH manifest URLs on the page
- **Playback controls**: Pause, resume, stop, seek, and volume control from the popup
- **Real-time status**: WebSocket connection for instant playback state updates
- **mDNS discovery**: Auto-discovers PiCast devices on the local network
- **Tor routing**: All traffic routed through Tor for privacy (configurable)
- **Cross-browser**: Works on both Chrome and Firefox with Manifest V3

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌─────────────┐
│  Content Script  │────▶│  Service Worker   │────▶│ PiCast HTTP │
│  (detector.js)   │     │  (service-worker) │     │ API :8585   │
│  Media detection │     │  URL interception │     ├─────────────┤
│  MSE monitoring  │     │  Cast/control     │────▶│ PiCast WS   │
└─────────────────┘     │  WebSocket client  │     │ :8586/ws    │
                        │  Badge management  │     └─────────────┘
┌─────────────────┐     └──────────────────┘
│  Popup           │
│  (popup.html/js) │◀──── chrome.runtime.sendMessage ────┘
│  Cast button     │
│  Playback ctrl   │
└─────────────────┘
```

## Permissions

| Permission | Purpose |
|------------|---------|
| `activeTab` | Get the URL of the currently active tab |
| `storage` | Persist settings (PiCast address, Tor mode) |
| `webRequest` | Intercept media URLs (HLS/DASH/direct) from network requests |
| `dns` (Firefox) | mDNS service discovery |

The extension does **not** read page content, cookies, or form data.

## Building

```bash
# Build for both browsers
bash build.sh

# Chrome only
bash build.sh chrome

# Firefox only
bash build.sh firefox
```

Output: `build/picast-chrome/` and `build/picast-firefox/`

## Signing

### Chrome Web Store

1. Create a developer account at https://chrome.google.com/webstore/devconsole
2. Zip the `build/picast-chrome/` directory
3. Upload and submit for review

### Firefox Add-ons (AMO)

1. Create an account at https://addons.mozilla.org/developers/
2. Zip the `build/picast-firefox/` directory
3. Submit for signing and review
4. For self-distribution, use the "On your own" option to get a signed XPI
