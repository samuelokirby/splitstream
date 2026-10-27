#!/usr/bin/env bash
#
# Build a signed macOS .app around the `parakeet` example, so splitstream can
# actually be granted microphone and system-audio permission.
#
# Why a bundle at all: TCC (macOS privacy) reads the usage-description strings
# from an Info.plist and records consent against a bundle identifier plus a
# code-signing identity. A bare `cargo run` binary has neither, so it is
# attributed to whatever launched it (your terminal) and the system-audio tap
# has no way to prompt.
#
#   ./build.sh                 # build + ad-hoc sign
#   ./build.sh --run           # ...and launch it with output in this terminal
#   SIGN_ID="Developer ID Application: Your Name (TEAMID)" ./build.sh
#   BUNDLE_ID=com.yourco.app ./build.sh
#   EXAMPLE=deepgram FEATURES=deepgram ./build.sh
#
# Prerequisite: the model. Run `download-parakeet` first — the example looks in
# ~/Library/Application Support/splitstream/models/parakeet-eou, which is where
# the downloader writes by default. A bundle launched by Finder has `/` as its
# working directory, so a relative ./models path would never resolve.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

EXAMPLE="${EXAMPLE:-parakeet}"
FEATURES="${FEATURES:-}"
APP_NAME="${APP_NAME:-Splitstream Demo}"
BUNDLE_ID="${BUNDLE_ID:-com.example.splitstream-demo}"
EXECUTABLE="splitstream-demo"

# Ad-hoc ("-") signing is enough for the permission prompts to appear on the
# machine that built the app, but the identity changes on every rebuild, so
# macOS forgets the grant and re-prompts. Set SIGN_ID to a real Developer ID
# certificate for a stable identity, and for anything you ship.
SIGN_ID="${SIGN_ID:--}"

app="$root/target/$APP_NAME.app"
contents="$app/Contents"

echo "==> cargo build --release --example $EXAMPLE"
build_args=(build --release --example "$EXAMPLE")
[ -n "$FEATURES" ] && build_args+=(--features "$FEATURES")
(cd "$root" && cargo "${build_args[@]}")

binary="$root/target/release/examples/$EXAMPLE"
[ -x "$binary" ] || { echo "error: $binary was not produced" >&2; exit 1; }

echo "==> assembling $app"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$binary" "$contents/MacOS/$EXECUTABLE"

# Bundle identifier is substituted so BUNDLE_ID actually takes effect; every
# other key comes from Info.plist verbatim.
cp "$here/Info.plist" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" "$contents/Info.plist" >/dev/null
/usr/libexec/PlistBuddy -c "Set :CFBundleName $APP_NAME" "$contents/Info.plist" >/dev/null
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName $APP_NAME" "$contents/Info.plist" >/dev/null

echo "==> codesign (identity: $SIGN_ID)"
# --options runtime enables the hardened runtime, which is what makes the
# entitlements meaningful and is required for notarization.
codesign --force \
         --options runtime \
         --timestamp=none \
         --entitlements "$here/splitstream-demo.entitlements" \
         --sign "$SIGN_ID" \
         "$app"

codesign --display --entitlements - --verbose=2 "$app" 2>&1 | sed 's/^/    /'

cat <<TXT

Built: $app
Bundle ID: $BUNDLE_ID

Run it (keeps the bundle identity, and shows stdout here):
    open "$app" --stdout /dev/stdout --stderr /dev/stderr

Do NOT run Contents/MacOS/$EXECUTABLE directly from a shell: the process is
then *responsible* to your terminal, so the prompts and the grants land on
Terminal.app instead of on this bundle.

Reset the grants to see the prompts again (per-permission, per-bundle-id):
    tccutil reset Microphone $BUNDLE_ID
    tccutil reset AudioCapture $BUNDLE_ID
    tccutil reset All $BUNDLE_ID
TXT

if [ "${1:-}" = "--run" ]; then
    echo
    echo "==> open \"$app\""
    open "$app" --stdout /dev/stdout --stderr /dev/stderr
fi
