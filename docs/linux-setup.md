# Set up Splice on Linux

Splice needs a Wayland session. The host must run Tailscale and be connected to the same tailnet
as the other Splice machines. Which desktops work, and how:

| Desktop | Capture (this machine as source) | Injection (this machine as target) | Clipboard |
|---|---|---|---|
| GNOME 46+ | Input Capture portal (asks once per launch on GNOME 50, remembered from GNOME 51) | uinput, or Remote Desktop portal | Clipboard portal |
| KDE Plasma 6.x | Wayland overlay (no prompt, cursor hidden) or Input Capture portal | uinput, or Remote Desktop portal | data-control |
| Hyprland, sway, river, labwc, Wayfire, niri | Wayland overlay | uinput | data-control |
| COSMIC | Wayland overlay | uinput (single-monitor targets only) | data-control |

X11 sessions are not supported: Splice needs a Wayland display for its own geometry and edge
handling even when it only injects. Splice picks the best available combination automatically. The **Input backends** section of the
window shows what is active and lets you force a choice; a change takes effect immediately.

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

A Flatpak cannot install host udev rules, so add them yourself once (works on immutable
systems too, `/etc` is writable there):

```sh
sudo install -m 0644 packaging/linux/70-splice.rules /etc/udev/rules.d/
sudo install -m 0644 packaging/linux/splice-modules.conf /etc/modules-load.d/splice.conf
sudo modprobe uinput
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=input --action=change
sudo udevadm trigger --sysname-match=uinput --action=change
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

The `70-splice.rules` udev rule tags `/dev/input/event*` and `/dev/uinput` with `uaccess`, which
makes systemd-logind grant the active seat's user an access control list on them. There is no
group to join and no re-login; the access follows your session and ends when it is inactive.

- `/dev/input` (read-only) is the physical-input monitor: portal-injected input never appears
  there, so evdev is how Splice tells which machine last received real input. It also detects
  the panic chord on the raw devices.
- `/dev/uinput` is the virtual keyboard and pointer used to inject input from other machines
  without a portal session. Injected events appear on `/dev/input` under the names
  `Splice Virtual Pointer` and `Splice Virtual Keyboard`; the monitor ignores them.

The packages also install `modules-load.d/splice.conf` so the `uinput` module is loaded at boot;
the access control list can only be applied to a loaded module's device node. Without the rule,
capture still works through the portal, injection falls back to the Remote Desktop portal where
one exists, source auto-switching is limited, and Splice shows the missing pieces in its health
panel and backend picker. Splice re-checks access every 15 seconds, so installing the rule while
it runs is picked up without a restart. Membership in the `input` group also satisfies the
`/dev/input` half on distributions that use it.

## Choosing backends

**Capture.** The Wayland overlay places invisible 2 px strips on the shared edges; pushing the
pointer through one locks it, hides the cursor and forwards input until it comes back. It never
prompts and works on every compositor with layer-shell support (KDE, wlroots, COSMIC, niri).
GNOME has no layer-shell, so it uses the Input Capture portal, which leaves the cursor visible
at the edge while you are on another machine. Hyprland releases after v0.50.x currently do not
enforce pointer locks on layer surfaces; Splice detects that, reports it in the health panel and
ends the crossing. Hyprland has no Input Capture portal, so until that regression is fixed a
Hyprland machine can only be driven by other machines, not drive them.

**Injection.** The uinput backend creates a virtual absolute pointer and keyboard, so input from
other machines is indistinguishable from a local device: no portal prompt, no libei state
machine, and it works at the lock screen so a remote machine can unlock this one. Your global
mouse settings apply to it: natural scrolling inverts the wheel and left-handed mode swaps the
buttons, as they would for any mouse. Keyboard remappers such as keyd also pick the virtual
keyboard up and apply their layers to it, exactly as they would to a plugged-in keyboard. The Remote Desktop portal is the alternative on GNOME and
KDE. On GNOME the Remote Desktop session is kept for the clipboard even when uinput injects.

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

## Receive raw input from a Mac

Use the same protocol 4 build on every computer. Raw input requires `/dev/uinput` access even if
the Desktop injection backend uses the Remote Desktop portal. Install the udev rule above; a
missing permission produces a preparation error before the Mac captures local input.

Allow TCP 41719 on the Tailscale interface alongside KVM port 41717 and updater port 41718.
The receiver creates `Splice Virtual Raw Mouse` and `Splice Virtual Raw Keyboard`, then keeps
them alive across handoffs. The devices use relative mouse axes and preserve physical key codes.
The destination desktop or game applies its own acceleration, keyboard layout, and repeat settings.

Linux source edges currently support Immediate crossing. Dwell and Resistance require passive
edge observations and are available with a Mac source. See the [native validation handoff](raw-input-macos-handoff.md).
