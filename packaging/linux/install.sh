#!/usr/bin/env bash
set -euo pipefail

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "${script_directory}/../.." && pwd)"
binary_path="${repository_root}/target/release/splice"
desktop_file="${script_directory}/splice.desktop"
service_file="${script_directory}/app-splice.service"
launcher_file="${script_directory}/splice-launch"
binary_directory="${HOME}/.local/bin"
applications_directory="${HOME}/.local/share/applications"
systemd_user_directory="${HOME}/.config/systemd/user"

if [[ ! -x "$binary_path" ]]; then
    printf 'Build the release binary first: cargo build -p splice-app --release\n' >&2
    printf 'Expected to find it at %s.\n' "$binary_path" >&2
    exit 1
fi

mkdir -p "$binary_directory" "$applications_directory" "$systemd_user_directory"
install -m 0755 "$binary_path" "$binary_directory/splice"
install -m 0755 "$launcher_file" "$binary_directory/splice-launch"
install -m 0644 "$desktop_file" "$applications_directory/splice.desktop"
install -m 0644 "$service_file" "$systemd_user_directory/app-splice.service"

systemctl --user daemon-reload
systemctl --user enable app-splice.service

printf '\nInstalled Splice at %s.\n' "$binary_directory/splice"
cat <<'EOF'
The Splice user service is enabled for graphical sessions.

For physical-input detection and source auto-switching, run:
  sudo usermod -aG input $USER
Then log out and log back in before starting Splice.

Start Splice now with:
  systemctl --user start app-splice.service
EOF
