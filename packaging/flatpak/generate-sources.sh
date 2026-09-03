#!/usr/bin/env bash
# Regenerate cargo-sources.json (offline crate sources for flatpak-builder) after any
# Cargo.lock change. Uses flatpak-cargo-generator from flatpak-builder-tools.
set -euo pipefail

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "${script_directory}/../.." && pwd)"
generator_url="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py"
generator="$(mktemp --suffix=.py)"
trap 'rm -f "$generator"' EXIT

curl -fsSL "$generator_url" -o "$generator"
python3 "$generator" "${repository_root}/Cargo.lock" -o "${script_directory}/cargo-sources.json"
printf 'Wrote %s\n' "${script_directory}/cargo-sources.json"
