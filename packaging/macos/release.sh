#!/usr/bin/env bash
set -euo pipefail

: "${SPLICE_MAC_CERT_P12:?Missing Developer ID certificate}"
: "${SPLICE_MAC_CERT_PASSWORD:?Missing certificate password}"
: "${SPLICE_CODESIGN_IDENTITY:?Missing Developer ID signing identity}"
: "${SPLICE_APPLE_ID:?Missing Apple ID}"
: "${SPLICE_APPLE_TEAM_ID:?Missing Apple team ID}"
: "${SPLICE_APPLE_APP_PASSWORD:?Missing notarization password}"

release_temp="$(mktemp -d)"
release_keychain="${release_temp}/splice.keychain-db"
release_keychain_password="$(openssl rand -hex 32)"
trap 'security delete-keychain "$release_keychain"; rm -rf "$release_temp"' EXIT
export SPLICE_CERT_PATH="${release_temp}/developer-id.p12"
python3 - <<'PY'
import base64, os
with open(os.environ['SPLICE_CERT_PATH'], 'wb') as stream:
    stream.write(base64.b64decode(os.environ['SPLICE_MAC_CERT_P12'], validate=True))
PY
security create-keychain -p "$release_keychain_password" "$release_keychain"
security set-keychain-settings -lut 21600 "$release_keychain"
security unlock-keychain -p "$release_keychain_password" "$release_keychain"
security import "$SPLICE_CERT_PATH" -P "$SPLICE_MAC_CERT_PASSWORD" -A -t cert -f pkcs12 -k "$release_keychain"
security set-key-partition-list -S apple-tool:,apple: -k "$release_keychain_password" "$release_keychain"
security list-keychains -d user -s "$release_keychain"
bash packaging/macos/make-app.sh --no-build
ditto -c -k --keepParent build/Splice.app "${release_temp}/Splice.zip"
xcrun notarytool submit "${release_temp}/Splice.zip" --apple-id "$SPLICE_APPLE_ID" --team-id "$SPLICE_APPLE_TEAM_ID" --password "$SPLICE_APPLE_APP_PASSWORD" --wait --timeout 20m
xcrun stapler staple build/Splice.app
xcrun stapler validate build/Splice.app
spctl --assess --type execute build/Splice.app
