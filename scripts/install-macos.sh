#!/usr/bin/env bash
# Build MarkRust and install (or update) MarkRust.app in /Applications.
#
#   scripts/install-macos.sh            # release build, install to /Applications
#   scripts/install-macos.sh --debug    # faster build, for iterating
#   scripts/install-macos.sh --launch   # open the app when done
#   MARKRUST_INSTALL_DIR=~/Applications scripts/install-macos.sh
#
# Re-running updates the installed bundle in place, so the Dock icon, bundle id
# and any granted permissions stay attached to the same app.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="MarkRust"
BUNDLE_ID="${MARKRUST_BUNDLE_ID:-com.alexeyabramov.markrust}"
DEST_DIR="${MARKRUST_INSTALL_DIR:-/Applications}"
PROFILE="release"
LAUNCH=0

for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE="debug" ;;
    --release) PROFILE="release" ;;
    --launch|--open) LAUNCH=1 ;;
    -h|--help) sed -n '2,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this script builds a macOS .app bundle; use cargo install elsewhere." >&2
  exit 1
fi

# GPUI compiles Metal shaders, which the Command Line Tools do not ship. Point
# at full Xcode when xcode-select is aimed at the CLT (the usual setup here).
if [[ -z "${DEVELOPER_DIR:-}" ]] && ! xcrun --find metal >/dev/null 2>&1; then
  for candidate in /Applications/Xcode.app/Contents/Developer /Applications/Xcode-beta.app/Contents/Developer; do
    if [[ -d "$candidate" ]]; then
      export DEVELOPER_DIR="$candidate"
      echo "==> using DEVELOPER_DIR=$DEVELOPER_DIR (metal compiler)"
      break
    fi
  done
fi
if ! xcrun --find metal >/dev/null 2>&1; then
  echo "error: no Metal compiler found. Install Xcode (not just Command Line Tools)," >&2
  echo "       or set DEVELOPER_DIR to an Xcode that has it." >&2
  exit 1
fi

echo "==> building markrust ($PROFILE)"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p markrust
else
  cargo build -p markrust
fi

BIN="target/$PROFILE/markrust"
[[ -x "$BIN" ]] || { echo "error: $BIN not found after build" >&2; exit 1; }

VERSION="$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
for pkg in json.load(sys.stdin)["packages"]:
    if pkg["name"] == "markrust":
        print(pkg["version"])
        break
')"
[[ -n "$VERSION" ]] || { echo "error: could not read markrust version" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

echo "==> assembling $APP_NAME.app (v$VERSION)"
cp "$BIN" "$APP/Contents/MacOS/markrust"

# Icon: build a full .icns so Finder, Dock and cmd-tab all look right.
ICON_SRC="assets/icon/icon.png"
if [[ -f "$ICON_SRC" ]]; then
  ICONSET="$STAGE/$APP_NAME.iconset"
  mkdir -p "$ICONSET"
  for spec in "16:16x16" "32:16x16@2x" "32:32x32" "64:32x32@2x" \
              "128:128x128" "256:128x128@2x" "256:256x256" "512:256x256@2x" \
              "512:512x512" "1024:512x512@2x"; do
    px="${spec%%:*}"
    name="${spec##*:}"
    sips -z "$px" "$px" "$ICON_SRC" --out "$ICONSET/icon_$name.png" >/dev/null 2>&1
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/$APP_NAME.icns"
else
  echo "    (no $ICON_SRC — installing without a custom icon)"
fi

# NOTE: no CFBundleDocumentTypes on purpose. The app takes a file path on the
# command line but does not yet handle Finder open-document events, so claiming
# .md would make double-clicked files open a blank window. Add the document
# types together with the open-event handler, not before.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleExecutable</key>
	<string>markrust</string>
	<key>CFBundleIconFile</key>
	<string>$APP_NAME</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.productivity</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSHumanReadableCopyright</key>
	<string>Alexey Abramov — MPL-2.0</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc signature: unsigned binaries copied into /Applications get killed by
# Gatekeeper on some setups, and a stable signature keeps granted permissions
# (screen recording, accessibility) attached across updates.
echo "==> signing (ad-hoc)"
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 || {
  echo "    warning: ad-hoc signing failed; the app may need a Gatekeeper override" >&2
}

TARGET="$DEST_DIR/$APP_NAME.app"
mkdir -p "$DEST_DIR"

if pgrep -f "$TARGET/Contents/MacOS/markrust" >/dev/null 2>&1; then
  echo "error: $APP_NAME is running from $TARGET — quit it, then re-run." >&2
  exit 1
fi

if [[ -e "$TARGET" ]]; then
  # Only ever replace a bundle that is actually ours.
  EXISTING_ID="$(defaults read "$TARGET/Contents/Info" CFBundleIdentifier 2>/dev/null || echo "")"
  if [[ "$EXISTING_ID" != "$BUNDLE_ID" ]]; then
    echo "error: $TARGET exists but its bundle id is '$EXISTING_ID', not '$BUNDLE_ID'." >&2
    echo "       Refusing to replace an app this script did not create." >&2
    exit 1
  fi
  echo "==> updating existing install at $TARGET"
  rm -rf "$TARGET"
else
  echo "==> installing to $TARGET"
fi

if ! cp -R "$APP" "$TARGET" 2>/dev/null; then
  echo "error: could not write to $DEST_DIR." >&2
  echo "       Try: sudo scripts/install-macos.sh, or MARKRUST_INSTALL_DIR=\"\$HOME/Applications\" scripts/install-macos.sh" >&2
  exit 1
fi

# Nudge LaunchServices so Finder picks up the new version and icon immediately.
touch "$TARGET"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$TARGET" >/dev/null 2>&1 || true

echo ""
echo "$APP_NAME $VERSION installed at $TARGET"
echo "Launch it from Spotlight/Launchpad, or: open -a $APP_NAME"

if [[ "$LAUNCH" == "1" ]]; then
  open -a "$TARGET"
fi
