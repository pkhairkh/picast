/**
 * PiCast Content Script: Media Detector
 *
 * Runs on all pages at `document_idle`. Detects media elements
 * (<video>, <audio>) and MSE (Media Source Extensions) usage,
 * then reports detected media URLs to the background service worker
 * for potential casting.
 *
 * Detection strategies:
 * 1. Direct <video src="..."> and <audio src="..."> elements
 * 2. <video> with <source> children
 * 3. YouTube-style player detection (#movie_player, ytplayer)
 * 4. Vimeo embed detection (iframe[src*="vimeo.com"])
 * 5. Twitch detection
 * 6. Dailymotion detection
 * 7. MSE blob: URL detection and MediaSource API interception
 * 8. MutationObserver for dynamically added media elements
 * 9. Periodic re-scan for late-loading media
 */

(function () {
  "use strict";

  let contextValid = true;

  // Don't run in iframes — only the top-level document.
  try { if (window !== window.top) return; } catch { return; }

  const detectedSources = new Map(); // url -> source info (dedup by URL)
  const MAX_DETECTED = 30;

  function isDomain(hostname, domain) {
    return hostname === domain || hostname.endsWith("." + domain);
  }

  // ─── Direct Media Elements ──────────────────────────────────────

  function scanMediaElements() {
    const elements = document.querySelectorAll("video, audio");

    elements.forEach((el) => {
      // Direct src attribute.
      if (el.src && !el.src.startsWith("blob:")) {
        addSource(el.src, getMediaType(el.src, el), "direct", "high");
      }

      // <source> children.
      el.querySelectorAll("source").forEach((source) => {
        if (source.src && !source.src.startsWith("blob:")) {
          addSource(source.src, getMediaType(source.src, el), "direct", "high");
        }
      });

      // MSE blob: URLs — can't be cast directly, but indicate media presence.
      if (el.src && el.src.startsWith("blob:")) {
        addSource(el.src, "mse-blob", "mse", "low");
      }

      // srcObject (MediaStream).
      if (el.srcObject) {
        addSource("(srcObject/MediaStream)", "media-stream", "mse", "low");
      }

      // currentSrc (resolved source after source selection).
      if (el.currentSrc && !el.currentSrc.startsWith("blob:") && el.currentSrc !== el.src) {
        addSource(el.currentSrc, getMediaType(el.currentSrc, el), "direct", "high");
      }
    });
  }

  // ─── MSE API Interception ───────────────────────────────────────

  /**
   * Intercept MediaSource.addSourceBuffer() calls to detect when
   * a page is using MSE to play media. We record the MIME type
   * which tells us the codec/format being used.
   */
  function interceptMediaSource() {
    if (!window.MediaSource) return;

    const OriginalMediaSource = window.MediaSource;
    const origAddSourceBuffer = OriginalMediaSource.prototype.addSourceBuffer;

    OriginalMediaSource.prototype.addSourceBuffer = function (mimeType) {
      // Report the MSE usage to the service worker
      addSource(
        `mse://${window.location.hostname}/${mimeType}`,
        mimeType,
        "mse",
        "medium"
      );

      // Call the original method
      return origAddSourceBuffer.call(this, mimeType);
    };

    // Also detect when MediaSource is attached to a video element
    const origUrlCreateObjectURL = URL.createObjectURL;
    URL.createObjectURL = function (obj) {
      if (obj instanceof OriginalMediaSource) {
        addSource(
          `mse-object://${window.location.hostname}/${Date.now()}`,
          "application/x-mse",
          "mse",
          "medium"
        );
      }
      return origUrlCreateObjectURL.call(URL, obj);
    };
  }

  // ─── YouTube Detection ──────────────────────────────────────────

  function detectYouTube() {
    // YouTube main site (including Music, Kids, TV subdomains).
    const ytPlayer = document.querySelector("#movie_player");
    if (ytPlayer) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube", "high");
      }
    }

    // YouTube embed.
    if (
      isDomain(window.location.hostname, "youtube.com") &&
      window.location.pathname.startsWith("/embed/")
    ) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-embed", "high");
      }
    }

    // YouTube Shorts
    if (
      isDomain(window.location.hostname, "youtube.com") &&
      window.location.pathname.startsWith("/shorts/")
    ) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-shorts", "high");
      }
    }

    // YouTube Music (music.youtube.com)
    if (isDomain(window.location.hostname, "music.youtube.com")) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-music", "high");
      }
    }

    // YouTube Kids (kids.youtube.com)
    if (isDomain(window.location.hostname, "kids.youtube.com")) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-kids", "high");
      }
    }

    // YouTube TV (tv.youtube.com)
    if (isDomain(window.location.hostname, "tv.youtube.com")) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-tv", "high");
      }
    }

    // YouTube embed iframes
    document.querySelectorAll('iframe[src*="youtube.com/embed/"]').forEach((iframe) => {
      try {
        const src = new URL(iframe.src);
        const videoId = src.pathname.split("/").filter(Boolean).pop();
        if (videoId) {
          addSource(`https://www.youtube.com/watch?v=${videoId}`, "video/mp4", "youtube-embed", "high");
        }
      } catch {}
    });
  }

  function extractYouTubeVideoId(url) {
    try {
      const u = new URL(url);
      // youtube.com/watch?v=...
      if (u.searchParams.has("v")) return u.searchParams.get("v");
      // youtu.be/...
      if (u.hostname === "youtu.be") return u.pathname.slice(1);
      // youtube.com/embed/...
      if (u.pathname.startsWith("/embed/")) return u.pathname.split("/")[2];
      // youtube.com/shorts/...
      if (u.pathname.startsWith("/shorts/")) return u.pathname.split("/")[2];
      // youtube.com/live/...
      if (u.pathname.startsWith("/live/")) return u.pathname.split("/")[2];
    } catch {}
    return null;
  }

  // ─── Vimeo Detection ───────────────────────────────────────────

  function detectVimeo() {
    // Vimeo embed iframes.
    document.querySelectorAll('iframe[src*="vimeo.com"]').forEach((iframe) => {
      try {
        const src = new URL(iframe.src);
        const videoId = src.pathname.split("/").filter(Boolean).pop();
        if (videoId && /^\d+$/.test(videoId)) {
          addSource(`https://vimeo.com/${videoId}`, "video/mp4", "vimeo-embed", "high");
        }
      } catch {}
    });

    // Vimeo main site.
    if (isDomain(window.location.hostname, "vimeo.com")) {
      const pathParts = window.location.pathname.split("/").filter(Boolean);
      if (pathParts.length > 0 && /^\d+$/.test(pathParts[0])) {
        addSource(window.location.href, "video/mp4", "vimeo", "high");
      }
    }
  }

  // ─── Twitch Detection ──────────────────────────────────────────

  function detectTwitch() {
    if (isDomain(window.location.hostname, "twitch.tv")) {
      // Detect channel or video page
      if (window.location.pathname.startsWith("/videos/")) {
        addSource(window.location.href, "video/mp4", "twitch-vod", "high");
      } else if (window.location.pathname.split("/").filter(Boolean).length === 1) {
        addSource(window.location.href, "video/mp4", "twitch-live", "high");
      } else {
        addSource(window.location.href, "video/mp4", "twitch", "medium");
      }
    }

    // Twitch embed iframes
    document.querySelectorAll('iframe[src*="twitch.tv"]').forEach((iframe) => {
      try {
        addSource(iframe.src, "video/mp4", "twitch-embed", "medium");
      } catch {}
    });
  }

  // ─── Dailymotion Detection ─────────────────────────────────────

  function detectDailymotion() {
    if (isDomain(window.location.hostname, "dailymotion.com")) {
      addSource(window.location.href, "video/mp4", "dailymotion", "high");
    }

    document.querySelectorAll('iframe[src*="dailymotion.com"]').forEach((iframe) => {
      try {
        const src = new URL(iframe.src);
        const videoId = src.pathname.split("/").filter(Boolean).pop();
        if (videoId) {
          addSource(`https://www.dailymotion.com/video/${videoId}`, "video/mp4", "dailymotion-embed", "high");
        }
      } catch {}
    });
  }

  // ─── Generic Embedded Player Detection ──────────────────────────

  function detectGenericEmbeds() {
    // JW Player
    if (window.jwplayer || document.querySelector("[id*='jwplayer']")) {
      addSource(window.location.href, "video/mp4", "jwplayer", "medium");
    }

    // Video.js
    if (window.videojs || document.querySelector(".video-js")) {
      addSource(window.location.href, "video/mp4", "videojs", "medium");
    }

    // HTML5 player with poster (indicates intentional video)
    document.querySelectorAll("video[poster]").forEach((el) => {
      if (el.poster && !detectedSources.has(window.location.href)) {
        addSource(window.location.href, "video/mp4", "html5-player", "medium");
      }
    });
  }

  // ─── Helper Functions ──────────────────────────────────────────

  function getMediaType(url, element) {
    try {
      const u = new URL(url, window.location.href);
      const ext = u.pathname.split(".").pop().toLowerCase();
      const mimeMap = {
        mp4: "video/mp4",
        webm: "video/webm",
        mkv: "video/x-matroska",
        m3u8: "application/vnd.apple.mpegurl",
        mpd: "application/dash+xml",
        mp3: "audio/mpeg",
        ogg: "audio/ogg",
        opus: "audio/opus",
        wav: "audio/wav",
        flac: "audio/flac",
        aac: "audio/aac",
        m4a: "audio/mp4",
      };
      if (mimeMap[ext]) return mimeMap[ext];
    } catch {}

    // Fall back to element type.
    if (element && element.tagName === "AUDIO") return "audio/*";
    return "video/*";
  }

  function sendMessageToBackground(msg) {
    if (!contextValid) return;
    try {
      chrome.runtime.sendMessage(msg);
    } catch (e) {
      contextValid = false;
      if (observer) observer.disconnect();
    }
  }

  function addSource(url, mimeType, sourceType, confidence) {
    // Deduplicate by URL.
    if (detectedSources.has(url)) return;

    // Limit total detected sources
    if (detectedSources.size >= MAX_DETECTED) {
      // Remove oldest entry
      const firstKey = detectedSources.keys().next().value;
      detectedSources.delete(firstKey);
    }

    const sourceInfo = {
      src: url,
      mimeType,
      type: sourceType,
      confidence,
      timestamp: Date.now(),
    };

    detectedSources.set(url, sourceInfo);

    // Report to service worker.
    sendMessageToBackground({
      type: "MEDIA_DETECTED",
      sources: [{ src: url, type: sourceType, mimeType, confidence }],
    });
  }

  // ─── MutationObserver ──────────────────────────────────────────

  let observer = null;
  let scanTimer = null;

  function observeDynamicMedia() {
    observer = new MutationObserver((mutations) => {
      let shouldScan = false;

      for (const mutation of mutations) {
        // Check for attribute changes (e.g., src changes on video/audio)
        if (mutation.type === "attributes" && (mutation.target.nodeName === "VIDEO" || mutation.target.nodeName === "AUDIO")) {
          shouldScan = true;
          break;
        }

        for (const node of mutation.addedNodes) {
          if (node.nodeType !== Node.ELEMENT_NODE) continue;

          // Check if the added node is a media element.
          if (node.nodeName === "VIDEO" || node.nodeName === "AUDIO") {
            shouldScan = true;
            break;
          }

          // Check children.
          if (node.querySelector && node.querySelector("video, audio")) {
            shouldScan = true;
            break;
          }

          // Check for iframes (Vimeo/YouTube/Dailymotion embeds).
          if (node.nodeName === "IFRAME" && node.src) {
            try {
              const hostname = new URL(node.src).hostname;
              if (
                isDomain(hostname, "vimeo.com") ||
                isDomain(hostname, "youtube.com") ||
                isDomain(hostname, "dailymotion.com") ||
                isDomain(hostname, "twitch.tv")
              ) {
                shouldScan = true;
                break;
              }
            } catch {}
          }
        }
        if (shouldScan) break;
      }

      if (shouldScan) {
        clearTimeout(scanTimer);
        scanTimer = setTimeout(scanAll, 150); // Debounced delay for src to be set.
      }
    });

    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src"],
    });
  }

  // ─── Scan All ──────────────────────────────────────────────────

  function scanAll() {
    scanMediaElements();
    detectYouTube();
    detectVimeo();
    detectTwitch();
    detectDailymotion();
    detectGenericEmbeds();
  }

  // ─── Initialize ────────────────────────────────────────────────

  function init() {
    interceptMediaSource();
    scanAll();
    observeDynamicMedia();

    // SPA navigation handling
    window.addEventListener("popstate", () => setTimeout(scanAll, 300));
    const origPushState = history.pushState;
    history.pushState = function () {
      origPushState.apply(this, arguments);
      setTimeout(scanAll, 300);
    };
    const origReplaceState = history.replaceState;
    history.replaceState = function () {
      origReplaceState.apply(this, arguments);
      setTimeout(scanAll, 300);
    };

    // Re-scan periodically for late-loading media.
    setTimeout(scanAll, 2000);
    setTimeout(scanAll, 5000);
    setTimeout(scanAll, 10000);
  }

  // Run when DOM is ready (content script runs at document_idle).
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
