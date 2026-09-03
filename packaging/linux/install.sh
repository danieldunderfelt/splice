#!/usr/bin/env bash
# Per-user install from a local release build. Distribution packages
# (packaging/deb, packaging/rpm, packaging/arch, packaging/flatpak) are the
# preferred route; this script mirrors their layout under $HOME.
set -euo pipefail

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl --user disable --now app-splice.service 2>/dev/null || true
    rm -f "${HOME}/.config/systemd/user/app-splice.service" \
        "${HOME}/.local/bin/splice" "${HOME}/.local/bin/splice-launch" \
        "${HOME}/.local/share/applications/io.github.danieldunderfelt.Splice.desktop" \
        "${HOME}/.local/share/applications/splice.desktop"
    systemctl --user daemon-reload
    printf 'Removed the per-user Splice install. The udev rule in /etc/udev/rules.d is left in place.\n'
    exit 0
fi

script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "${script_directory}/../.." && pwd)"
binary_path="${repository_root}/target/release/splice"
binary_directory="${HOME}/.local/bin"
applications_directory="${HOME}/.local/share/applications"
systemd_user_directory="${HOME}/.config/systemd/user"
udev_rule="${script_directory}/70-splice.rules"
udev_target="/etc/udev/rules.d/70-splice.rules"
modules_conf="${script_directory}/splice-modules.conf"
modules_target="/etc/modules-load.d/splice.conf"

if [[ ! -x "$binary_path" ]]; then
    printf 'Build the release binary first: cargo build -p splice-app --release\n' >&2
    printf 'Expected to find it at %s.\n' "$binary_path" >&2
    exit 1
fi

mkdir -p "$binary_directory" "$applications_directory" "$systemd_user_directory"
install -m 0755 "$binary_path" "$binary_directory/splice"
install -m 0644 "${script_directory}/io.github.danieldunderfelt.Splice.desktop" "$applications_directory/"
rm -f "$applications_directory/splice.desktop"
sed "s|^ExecStart=.*|ExecStart=${binary_directory}/splice service|" "${script_directory}/app-splice.service" \
    > "$systemd_user_directory/app-splice.service"
rm -f "$binary_directory/splice-launch"

systemctl --user daemon-reload
systemctl --user enable app-splice.service

printf 'Installed Splice at %s.\n' "$binary_directory/splice"

if cmp -s "$udev_rule" "$udev_target" 2>/dev/null && cmp -s "$modules_conf" "$modules_target" 2>/dev/null; then
    printf 'Input device access rule already installed at %s.\n' "$udev_target"
elif [[ -t 0 ]] && command -v sudo >/dev/null 2>&1; then
    printf '\nInstalling the input device access rule to %s (asks for sudo).\n' "$udev_target"
    sudo install -m 0644 "$udev_rule" "$udev_target"
    sudo install -m 0644 "$modules_conf" "$modules_target"
    sudo modprobe uinput
    sudo udevadm control --reload
    sudo udevadm trigger --subsystem-match=input --action=change
    sudo udevadm trigger --sysname-match=uinput --action=change
    sudo udevadm settle --timeout=5
    printf 'Input device access rule installed; it takes effect immediately.\n'
else
    cat <<MSG

Physical-input detection needs access to /dev/input and remote input injection needs
/dev/uinput. Install the udev rule and the module list as root:
  sudo install -m 0644 "$udev_rule" "$udev_target"
  sudo install -m 0644 "$modules_conf" "$modules_target"
  sudo modprobe uinput
  sudo udevadm control --reload
  sudo udevadm trigger --subsystem-match=input --action=change
  sudo udevadm trigger --sysname-match=uinput --action=change
  sudo udevadm settle --timeout=5
MSG
fi

cat <<'MSG'

Start Splice now with:
  systemctl --user start app-splice.service
MSG
