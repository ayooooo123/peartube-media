#!/usr/bin/env bash
# Fetch the FFmpeg sample-archive files the manifest names as `samples:<path>`
# (https://samples.ffmpeg.org/<path>) into FFMPEG_SAMPLES (default
# ~/projects/oracles/ffmpeg-samples), then check every one against its pin in
# corpus/samples.sha256. Each pinned file also matches the md5sum list in its
# archive directory. The e2e runner checks the pins again before playing.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="${FFMPEG_SAMPLES:-$HOME/projects/oracles/ffmpeg-samples}"

cut -c67- "$HERE/samples.sha256" | while read -r f; do
  mkdir -p "$ROOT/$(dirname "$f")"
  if [ ! -s "$ROOT/$f" ]; then
    curl -sS --fail -o "$ROOT/$f.part" "https://samples.ffmpeg.org/${f// /%20}"
    mv "$ROOT/$f.part" "$ROOT/$f"
  fi
done
(cd "$ROOT" && shasum -a 256 -c "$HERE/samples.sha256")
