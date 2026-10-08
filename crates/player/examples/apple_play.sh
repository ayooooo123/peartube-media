#!/bin/sh
set -eu

: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR to an absolute owned build directory}"
case "$CARGO_TARGET_DIR" in /*) ;; *) echo "CARGO_TARGET_DIR must be absolute" >&2; exit 2 ;; esac
clip=$(realpath "${1:?apple_play.sh clip.mkv [--software] [--transport]}")
shift
cargo build --locked -j 2 --manifest-path "$(dirname "$0")/../Cargo.toml" --example apple_play

# LaunchServices supplies the foreground lifecycle a daemon-launched CLI lacks.
scratch=$(mktemp -d "$CARGO_TARGET_DIR/apple-play.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
app="$scratch/ApplePlay.app"
mkdir -p "$app/Contents/MacOS"
ln "$CARGO_TARGET_DIR/debug/examples/apple_play" "$app/Contents/MacOS/apple_play"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>apple_play</string>
<key>CFBundleIdentifier</key><string>dev.peartube.apple-play</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleName</key><string>PearTube Native Timing</string>
</dict></plist>
PLIST

launch_status=0
/usr/bin/open -n -W --stdout "$scratch/stdout" --stderr "$scratch/stderr" \
    --env "APPLE_PLAY_EXIT_STATUS=$scratch/status" "$app" --args "$clip" "$@" || launch_status=$?
for log in "$scratch/stdout" "$scratch/stderr"; do
    if [ -f "$log" ]; then cat "$log"; fi
done
if [ "$launch_status" -ne 0 ]; then exit "$launch_status"; fi
if [ ! -f "$scratch/status" ]; then
    echo "Native probe exited without reporting its status" >&2
    exit 1
fi
status=$(cat "$scratch/status")
echo "APPLE_EXIT status=$status"
exit "$status"
