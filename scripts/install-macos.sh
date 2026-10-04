#!/usr/bin/env bash
# Build MarkRust and install (or update) MarkRust.app in /Applications.
#
#   scripts/install-macos.sh            # release build, install to /Applications
#   scripts/install-macos.sh --debug    # faster build, for iterating
#   scripts/install-macos.sh --launch   # open the app when done
#   MARKRUST_INSTALL_DIR=~/Applications scripts/install-macos.sh
#
# Re-running replaces the installed bundle at the same path and retains the
# previous bundle for rollback. Changed ad-hoc signatures can require permission
# reapproval.
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
  cargo build --locked --release -p markrust
else
  cargo build --locked -p markrust
fi

BIN="target/$PROFILE/markrust"
[[ -x "$BIN" ]] || { echo "error: $BIN not found after build" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/markrust"
# Query the staged executable, not a build target that another Cargo process
# could replace while this bundle is being assembled.
BUILD_INFO="$("$APP/Contents/MacOS/markrust" --build-info)"
VERSION="$(printf '%s' "$BUILD_INFO" | python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])')"
[[ -n "$VERSION" ]] || { echo "error: could not read markrust version" >&2; exit 1; }
BUILD_DATE="$(printf '%s' "$BUILD_INFO" | python3 -c 'import json, sys; print(json.load(sys.stdin)["built_at_utc"])')"
[[ -n "$BUILD_DATE" ]] || { echo "error: could not read compiled build timestamp" >&2; exit 1; }
echo "==> assembling $APP_NAME.app (v$VERSION)"

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

# Finder open-document events are handled by the GPUI application shell.
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
	<key>MarkRustBuildDate</key>
	<string>$BUILD_DATE</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleDocumentTypes</key>
	<array>
		<dict>
			<key>CFBundleTypeName</key>
			<string>Markdown document</string>
			<key>CFBundleTypeRole</key>
			<string>Editor</string>
			<key>LSItemContentTypes</key>
			<array>
				<string>net.daringfireball.markdown</string>
			</array>
		</dict>
	</array>
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
# Gatekeeper on some setups. This is not a Developer-ID/notarized release;
# TCC permissions may require reapproval when the executable changes.
echo "==> signing (ad-hoc)"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --deep --strict "$APP"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

TARGET="$DEST_DIR/$APP_NAME.app"
mkdir -p "$DEST_DIR"

if pgrep -f "$TARGET/Contents/MacOS/markrust" >/dev/null 2>&1; then
  echo "error: $APP_NAME is running from $TARGET — quit it, then re-run." >&2
  exit 1
fi

if [[ -L "$TARGET" ]]; then
  echo "error: $TARGET is a symbolic link; refusing to replace it." >&2
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
  codesign --verify --deep --strict "$TARGET"
fi

# Stage on the destination volume before moving the old install. A failed
# copy or signature check must leave the current application untouched.
INSTALL_STAGE="$(mktemp -d "$DEST_DIR/.MarkRust-install.XXXXXX")"
if ! cp -R "$APP" "$INSTALL_STAGE/$APP_NAME.app"; then
  echo "error: could not write to $DEST_DIR." >&2
  echo "       The existing installation has not been changed."
  exit 1
fi
codesign --verify --deep --strict "$INSTALL_STAGE/$APP_NAME.app"
if pgrep -f "$TARGET/Contents/MacOS/markrust" >/dev/null 2>&1; then
  echo "error: $APP_NAME started during installation; quit it and re-run." >&2
  exit 1
fi

PREVIOUS=""
if [[ -e "$TARGET" ]]; then
  PREVIOUS="$INSTALL_STAGE/Previous-$APP_NAME.app"
  mv "$TARGET" "$PREVIOUS"
fi
if ! mv "$INSTALL_STAGE/$APP_NAME.app" "$TARGET"; then
  if [[ -n "$PREVIOUS" ]]; then
    mv "$PREVIOUS" "$TARGET"
  fi
  echo "error: installation failed; the previous bundle was restored." >&2
  exit 1
fi
if [[ -n "$PREVIOUS" ]]; then
  echo "==> previous bundle retained at $PREVIOUS"
else
  rmdir "$INSTALL_STAGE"
fi

# Nudge LaunchServices so Finder picks up the new version and icon immediately.
touch "$TARGET"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$TARGET" >/dev/null 2>&1 || true

echo ""
echo "$APP_NAME $VERSION ($BUILD_DATE) installed at $TARGET"
echo "Launch it from Spotlight/Launchpad, or: open '$TARGET'"

if [[ "$LAUNCH" == "1" ]]; then
  open -a "$TARGET"
fi
