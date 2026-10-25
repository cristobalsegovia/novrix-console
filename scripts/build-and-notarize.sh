#!/usr/bin/env zsh
# =============================================================================
# Switchboard Term — macOS Build, Developer ID Signing & Apple Notarization
# =============================================================================
# Usage:
#   ./scripts/build-and-notarize.sh
#
# Optional environment variables for automatic Apple Notarization:
#   export APPLE_ID="tu-apple-id@dominio.com"
#   export APPLE_PASSWORD="xxxx-xxxx-xxxx-xxxx"   # App-Specific Password from appleid.apple.com
#   export APPLE_TEAM_ID="TU_TEAM_ID"
# =============================================================================

set -euo pipefail

export PATH="$HOME/Library/pnpm/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

echo "==> Checking macOS Code Signing Identities in Keychain…"
IDENTITY=$(security find-identity -v -p codesigning | grep "Developer ID Application" | head -n 1 | sed -E 's/.*"([^"]+)".*/\1/' || true)

if [[ -n "$IDENTITY" ]]; then
  echo "✓ Found Developer ID certificate: $IDENTITY"
  export APPLE_SIGNING_IDENTITY="$IDENTITY"
else
  echo "⚠ No 'Developer ID Application' certificate found in Keychain."
  echo "  To sign as a trusted macOS developer:"
  echo "    1. Open Xcode -> Settings (⌘,) -> Accounts"
  echo "    2. Select your Apple Developer account -> Manage Certificates…"
  echo "    3. Click '+' -> 'Developer ID Application'"
  echo "  Proceeding with local ad-hoc signed build…"
fi

if [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
  echo "✓ Apple Notarization credentials detected (Team: $APPLE_TEAM_ID)."
else
  echo "ℹ Notarization env vars (APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID) not set; skipping remote notarization."
fi

echo "==> Building Novrix Console v0.7.0…"
pnpm tauri build

APP_BUNDLE="$ROOT_DIR/src-tauri/target/release/bundle/macos/Novrix Console.app"
DMG_BUNDLE=$(ls -1 "$ROOT_DIR"/src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -n 1 || true)

echo ""
echo "==> Build complete!"
echo "    App Bundle: $APP_BUNDLE"
[[ -n "$DMG_BUNDLE" ]] && echo "    DMG Installer: $DMG_BUNDLE"

if [[ -n "$IDENTITY" && -d "$APP_BUNDLE" ]]; then
  echo "==> Verifying code signature & Gatekeeper status…"
  codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
  spctl -a -t exec -vv "$APP_BUNDLE" || true
fi

if [[ -d "$APP_BUNDLE" ]]; then
  echo "==> Installing to /Applications/Novrix Console.app and cleaning duplicate build bundles…"
  rm -rf "/Applications/Switchboard Term.app"
  rm -rf "/Applications/Novrix Console.app"
  mv "$APP_BUNDLE" "/Applications/Novrix Console.app"
  LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
  "$LSREG" -u "$APP_BUNDLE" 2>/dev/null || true
  "$LSREG" -u "/Applications/Switchboard Term.app" 2>/dev/null || true
  "$LSREG" -f "/Applications/Novrix Console.app" 2>/dev/null || true
fi
