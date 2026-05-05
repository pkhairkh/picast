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
 * 5. MSE blob: URL detection
 * 6. MutationObserver for dynamically added media elements
 */

(function () {
  "use strict";

  // Don't run in iframes — only the top-level document.
  if (window !== window.top) return;

  const detectedSources = [];

  // ─── Direct Media Elements ──────────────────────────────────────

  function scanMediaElements() {
    const elements = document.querySelectorAll("video, audio");

    elements.forEach((el) => {
      // Direct src attribute.
      if (el.src && !el.src.startsWith("blob:")) {
        addSource(el.src, getMediaType(el.src, el), "direct");
      }

      // <source> children.
      el.querySelectorAll("source").forEach((source) => {
        if (source.src && !source.src.startsWith("blob:")) {
          addSource(source.src, getMediaType(source.src, el), "direct");
        }
      });

      // MSE blob: URLs — can't be cast directly, but indicate media.
      if (el.src && el.src.startsWith("blob:")) {
        addSource(el.src, "mse-blob", "mse");
      }

      // srcObject (MediaStream).
      if (el.srcObject) {
        addSource("(srcObject/MediaStream)", "media-stream", "mse");
      }

      // currentSrc (resolved source after source selection).
      if (el.currentSrc && !el.currentSrc.startsWith("blob:")) {
        addSource(el.currentSrc, getMediaType(el.currentSrc, el), "direct");
      }
    });
  }

  // ─── YouTube Detection ──────────────────────────────────────────

  function detectYouTube() {
    // YouTube main site.
    const ytPlayer = document.querySelector("#movie_player");
    if (ytPlayer) {
      const videoId = extractYouTubeVideoId(window.location.href);
      if (videoId) {
        addSource(window.location.href, "video/mp4", "youtube");
      }
    }

    // YouTube embed.
    if (window.location.hostname.includes("youtube.com") && window.location.pathname.startsWith("/embed/")) {
      addSource(window.location.href, "video/mp4", "youtube-embed");
    }
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
        if (videoId) {
          addSource(`https://vimeo.com/${videoId}`, "video/mp4", "vimeo-embed");
        }
      } catch {}
    });

    // Vimeo main site.
    if (window.location.hostname.includes("vimeo.com")) {
      addSource(window.location.href, "video/mp4", "vimeo");
    }
  }

  // ─── Twitch Detection ──────────────────────────────────────────

  function detectTwitch() {
    if (window.location.hostname.includes("twitch.tv")) {
      addSource(window.location.href, "video/mp4", "twitch");
    }
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
      };
      if (mimeMap[ext]) return mimeMap[ext];
    } catch {}

    // Fall back to element type.
    if (element && element.tagName === "AUDIO") return "audio/*";
    return "video/*";
  }

  function addSource(url, mimeType, sourceType) {
    // Deduplicate.
    if (detectedSources.some((s) => s.src === url)) return;

    detectedSources.push({
      src: url,
      mimeType,
      type: sourceType,
      timestamp: Date.now(),
    });

    // Report to service worker.
    try {
      chrome.runtime.sendMessage({
        type: "MEDIA_DETECTED",
        sources: [{ src: url, type: sourceType, mimeType }],
      });
    } catch {
      // Extension context may not be ready.
    }
  }

  // ─── MutationObserver ──────────────────────────────────────────

  function observeDynamicMedia() {
    const observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        for (const node of mutation.addedNodes) {
          if (node.nodeType !== Node.ELEMENT_NODE) continue;

          // Check if the added node is a media element.
          if (node.nodeName === "VIDEO" || node.nodeName === "AUDIO") {
            setTimeout(scanMediaElements, 100); // Delay for src to be set.
            return;
          }

          // Check children.
          if (node.querySelector && node.querySelector("video, audio")) {
            setTimeout(scanMediaElements, 100);
            return;
          }

          // Check for iframes (Vimeo embeds).
          if (node.nodeName === "IFRAME" && node.src && node.src.includes("vimeo.com")) {
            detectVimeo();
          }
        }
      }
    });

    observer.observe(document.body || document.documentElement, {
      childList: true,
      subtree: true,
    });
  }

  // ─── Initialize ────────────────────────────────────────────────

  function init() {
    scanMediaElements();
    detectYouTube();
    detectVimeo();
    detectTwitch();
    observeDynamicMedia();

    // Re-scan periodically for late-loading media.
    setTimeout(scanMediaElements, 2000);
    setTimeout(scanMediaElements, 5000);
  }

  // Run when DOM is ready (content script runs at document_idle).
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
