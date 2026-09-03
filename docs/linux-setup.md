# Set up Splice on Linux

Splice needs a Wayland session on GNOME 46 or later or KDE Plasma 6.4 or later. Those are the
desktops whose portal backends implement Input Capture. The host must run Tailscale and be
connected to the same tailnet as the other Splice machines.

## Install Splice

Use a distribution package when you can; each one installs the binary, the desktop entry, the
`app-splice.service` user unit, and the udev rule for input-device access.

Debian, Ubuntu, and Mint:

```sh
cargo install cargo-deb
cargo deb -p splice-app
sudo apt install ./target/debian/splice_*.deb
```

Fedora and other RPM distributions (needs `rpm-build` and `rpmdevtools`):

```sh
packaging/rpm/build.sh
sudo dnf install ~/rpmbuild/RPMS/*/splice-*.rpm
```

Arch and derivatives:

```sh
cd packaging/arch && makepkg -si
```

Flatpak, for SteamOS, Bazzite, Silverblue and other immutable systems (needs `flatpak-builder`):

```sh
packaging/flatpak/generate-sources.sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/io.github.danieldunderfelt.Splice.yml
```

Per-user install without a package:

```sh
cargo build -p splice-app --release
packaging/linux/install.sh
```

The installer copies `splice` to `~/.local/bin`, writes the user unit with that path, and asks
for `sudo` once to install the udev rule. If you decline, it prints the commands to run later.

## Run Splice

Splice on Linux is two kinds of process. The service (`splice service`) owns the engine, the
portal sessions, and the tray icon, and keeps running in the background. The window is a
separate, short-lived process, so closing it with the X button is a real close that leaves the
service running. Wayland has no way to hide a window, which is why the split exists.

Ways to start and stop it:

| Action | How |
|---|---|
| Open the window | Launch Splice from the app menu, run `splice`, or choose **Open Splice** from the tray. Starts the service first if it is not running. |
| Close the window | The X button. The service keeps running. |
| Start at login | Tick **Start Splice at login** in the window (writes an XDG autostart entry, works on every desktop) or `systemctl --user enable --now app-splice.service`. Both together are harmless: a second service start exits when it finds the first. |
| Quit everything | **Quit Splice** in the window, **Quit** in the tray, the **Quit Splice** action on the app-menu icon, `splice quit`, or `systemctl --user stop app-splice.service`. Captured input is released first. |
| Run in the foreground | `splice service` in a terminal. Ctrl-C stops it. |

The service logs to `~/.config/splice/splice.log` and each window to `splice-window.log` next
to it, unless started from a terminal. Flatpak installs cannot register a systemd user unit; use
**Start Splice at login** instead.

## Input-device access

The physical-input monitor reads `/dev/input/event*` in read-only mode. Portal-injected input
never appears there, so evdev is how Splice tells which machine last received real input. The
`70-splice.rules` udev rule tags input devices `uaccess`, which makes systemd-logind grant the
active seat's user an access control list on those devices. There is no group to join and no
re-login; the access follows your session and ends when it is inactive.

Without the rule, capture still works, but source auto-switching is limited and Splice shows a
warning in its health panel. Membership in the `input` group also satisfies the requirement on
distributions that use it.

## Approve portal access

On the first launch, allow the Input Capture and Remote Desktop portal requests for keyboard and
pointer control. Splice also requests clipboard access through the Remote Desktop session. Allow it
if the portal asks.

Splice keeps each portal session alive and stores replacement restore tokens. GNOME 50's portal has
no way to remember Input Capture consent, so it asks once per launch and again after a display
change; peers connecting and disconnecting never re-prompt. GNOME 51 (Fedora 45, Ubuntu 26.10)
remembers it. KDE remembers the consent. KDE also shows a persistent `Input Capture` notification.
That notification is normal.

Use a Wayland session. Check the session type with:

```sh
echo "$XDG_SESSION_TYPE"
```

The command must print `wayland`.

## The tray

On desktops with a status notifier host (KDE, or GNOME with the AppIndicator extension) the
service shows a tray icon with per-machine toggles, Open, Disconnect all, and Quit. Without one,
everything is still reachable: launch Splice from the app menu to open the window, and use the
window's **Quit Splice** button or `splice quit` to stop it. The tray is optional and Splice never
requires it.

## Check Tailscale connectivity

Run:

```sh
tailscale status
```

Each machine must appear online in the same tailnet. Splice uses the host Tailscale service for
discovery and connects to peers through their Tailscale addresses.

## Troubleshoot common failures

If the service exits, inspect its user log:

```sh
journalctl --user -u app-splice.service -f
```

If a portal request fails, inspect the portal backend that matches your desktop:

```sh
journalctl --user -xeu xdg-desktop-portal-gnome
journalctl --user -xeu xdg-desktop-portal-kde
```

If Splice cannot detect physical input, confirm the rule is installed and that your session holds
the access control list:

```sh
ls /etc/udev/rules.d/70-splice.rules /usr/lib/udev/rules.d/70-splice.rules
getfacl /dev/input/event0
```

If GNOME asks for Input Capture consent once per launch, that behavior is expected on GNOME 50 and
cannot be avoided there. Being asked again while the app keeps running is not expected. KDE should
retain the consent, and its persistent `Input Capture` notification is expected.

If peers do not appear, run `tailscale status` and confirm that Tailscale is running and that the
peer is online. Splice does not use MagicDNS for peer connections.
