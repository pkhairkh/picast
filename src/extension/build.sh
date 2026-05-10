#!/usr/bin/env bash
# boGDan Browser Extension Build Script
#
# Builds the extension for both Chrome and Firefox.
# The only difference is the manifest.json file.
#
# Usage:
#   ./build.sh          # Build both
#   ./build.sh chrome   # Chrome only
#   ./build.sh firefox  # Firefox only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"

# Source files shared by both browsers.
SHARED_FILES=(
  "background/service-worker.js"
  "content/detector.js"
  "popup/popup.html"
  "popup/popup.css"
  "popup/popup.js"
  "options/options.html"
  "options/options.css"
  "options/options.js"
  "icons/icon-16.png"
  "icons/icon-32.png"
  "icons/icon-48.png"
  "icons/icon-128.png"
  "_locales/en/messages.json"
)

build_extension() {
  local browser="$1"
  local target_dir="${BUILD_DIR}/bogdan-${browser}"

  echo "Building boGDan extension for ${browser}..."

  rm -rf "${target_dir}"
  mkdir -p "${target_dir}"

  # Copy manifest.
  if [ -f "${SCRIPT_DIR}/manifest-${browser}.json" ]; then
    cp "${SCRIPT_DIR}/manifest-${browser}.json" "${target_dir}/manifest.json"
    echo "  manifest.json (from manifest-${browser}.json)"
  else
    echo "  ERROR: manifest-${browser}.json not found!"
    exit 1
  fi

  # Copy shared files.
  for file in "${SHARED_FILES[@]}"; do
    local dir
    dir="$(dirname "${target_dir}/${file}")"
    mkdir -p "${dir}"
    if [ -f "${SCRIPT_DIR}/${file}" ]; then
      cp "${SCRIPT_DIR}/${file}" "${target_dir}/${file}"
    else
      echo "  ERROR: Required file ${file} not found!"
      exit 1
    fi
  done
  echo "  ${#SHARED_FILES[@]} shared files"

  # Create zip for distribution.
  local zip_name="bogdan-${browser}-v0.3.0.zip"
  (cd "${target_dir}" && zip -q -r "${BUILD_DIR}/${zip_name}" .)
  echo "  ${zip_name}"

  echo "  ${browser} build complete: ${target_dir}"
}

# ─── Main ──────────────────────────────────────────────────────────

mkdir -p "${BUILD_DIR}"

case "${1:-both}" in
  chrome)  build_extension "chrome" ;;
  firefox) build_extension "firefox" ;;
  both)
    build_extension "chrome"
    build_extension "firefox"
    ;;
  *)
    echo "Usage: $0 [chrome|firefox|both]"
    exit 1
    ;;
esac

echo ""
echo "Build artifacts in: ${BUILD_DIR}/"
