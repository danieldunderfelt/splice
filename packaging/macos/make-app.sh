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

printf '%s\n' 'Building splice-app in release mode...'
(
    cd "$repository_root"
    cargo build -p splice-app --release
)

if [[ ! -x "$binary_path" ]]; then
    printf 'The release binary was not found at %s.\n' "$binary_path" >&2
    exit 1
fi

printf 'Assembling %s...\n' "$app_path"
rm -rf -- "$app_path"
mkdir -p "$app_path/Contents/MacOS"
install -m 0755 "$binary_path" "$app_path/Contents/MacOS/splice"

cat > "$app_path/Contents/Info.plist" <<'PLIST'
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
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Splice connects to your other computers over your tailnet.</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

if security find-identity -p codesigning 2>/dev/null | grep -F 'Splice Dev' >/dev/null; then
    printf '%s\n' 'Found the "Splice Dev" signing identity.'
else
    printf '%s\n' 'The "Splice Dev" signing identity is not available.' >&2
fi

if codesign --force --options runtime --sign "Splice Dev" "$app_path"; then
    printf '%s\n' 'Signed the app with "Splice Dev".'
else
    cat >&2 <<'WARNING'
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
WARNING: FALLING BACK TO AD-HOC CODE SIGNING ("-").
MACOS PERMISSIONS WILL SILENTLY BREAK ON EVERY REBUILD.
Run packaging/macos/make-cert.sh and rebuild with a stable identity.
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
WARNING
    codesign --force --options runtime --sign "-" "$app_path"
    printf '%s\n' 'The app has an ad-hoc signature.' >&2
fi

codesign --verify --deep --strict "$app_path"

printf '\nApp bundle: %s\n' "$app_path"
cat <<'EOF'
Next steps:
1. Drag Splice.app to /Applications.
2. Open Splice.
3. In System Settings > Privacy & Security > Accessibility, enable Splice.
EOF
