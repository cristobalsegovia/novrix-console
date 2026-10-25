#!/usr/bin/env bash
# =============================================================================
# Switchboard Term — Mac App Store (.pkg) Builder for App Store Connect
# =============================================================================
# Signs the application with "Apple Distribution: Cristobal Segovia (SPKP4JVJ77)"
# and builds a signed .pkg installer when "3rd Party Mac Developer Installer"
# certificate is present in Keychain.
# =============================================================================

set -euo pipefail

cd "$(dirname "$0")/.."

export APPLE_SIGNING_IDENTITY="Apple Distribution: Cristobal Segovia (SPKP4JVJ77)"

echo "==> Building Novrix Console for Mac App Store…"
pnpm tauri build --bundles app

APP_PATH="src-tauri/target/release/bundle/macos/Novrix Console.app"
PKG_PATH="src-tauri/target/release/bundle/macos/Novrix_Console_AppStore.pkg"

INSTALLER_CERT=$(security find-identity -v | grep "3rd Party Mac Developer Installer" | head -n 1 | sed -E 's/.*"([^"]+)".*/\1/' || true)

if [[ -n "$INSTALLER_CERT" ]]; then
  echo "==> Creating signed App Store .pkg with '$INSTALLER_CERT'…"
  productbuild --component "$APP_PATH" /Applications --sign "$INSTALLER_CERT" "$PKG_PATH"
  echo "✓ Ready for Apple Transporter upload: $PKG_PATH"
else
  echo "⚠ Note: To generate the final .pkg for Transporter, create a 'Mac Installer Distribution' certificate in developer.apple.com/account/resources/certificates/list"
fi
