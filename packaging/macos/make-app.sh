#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    printf '%s\n' 'make-app.sh must run on macOS.' >&2
    exit 1
fi

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "${script_directory}/../.." && pwd)"
build_directory="${repository_root}/build"
app_path="${build_directory}/Splice.app"
binary_path="${repository_root}/target/release/splice"
package_id="$(cd "$repository_root" && cargo pkgid -p splice-app)"
app_version="${package_id##*#}"
app_version="${app_version##*@}"

if [[ ! "$app_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    printf 'Could not determine the app version from Cargo: %s\n' "$package_id" >&2
    exit 1
fi

if [[ "${1:-}" == "--no-build" ]]; then
    printf '%s\n' 'Using the verified release binary.'
elif [[ "$#" == 0 ]]; then
    (cd "$repository_root" && cargo build -p splice-app --release --locked)
else
    printf '%s\n' 'usage: make-app.sh [--no-build]' >&2
    exit 2
fi

if [[ ! -x "$binary_path" ]]; then
    printf 'The release binary was not found at %s.\n' "$binary_path" >&2
    exit 1
fi

printf 'Assembling %s...\n' "$app_path"
rm -rf -- "$app_path"
mkdir -p "$app_path/Contents/MacOS"
install -m 0755 "$binary_path" "$app_path/Contents/MacOS/splice"

cat > "$app_path/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>dev.splice.app</string>
    <key>CFBundleName</key>
    <string>Splice</string>
    <key>CFBundleExecutable</key>
    <string>splice</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${app_version}</string>
    <key>CFBundleVersion</key>
    <string>${app_version}</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Splice connects to your other computers over your tailnet.</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

signing_identity="${SPLICE_CODESIGN_IDENTITY:-Splice Dev}"
if ! security find-identity -v -p codesigning | grep -F -- "$signing_identity" >/dev/null; then
    printf 'Required signing identity is unavailable: %s\n' "$signing_identity" >&2
    exit 1
fi
codesign --force --options runtime --timestamp --sign "$signing_identity" "$app_path"

codesign --verify --deep --strict "$app_path"

printf '\nApp bundle: %s\n' "$app_path"
cat <<'EOF'
Next steps:
1. Drag Splice.app to /Applications.
2. Open Splice.
3. In System Settings > Privacy & Security > Accessibility, enable Splice.
EOF
