# Set up Splice on Linux

Splice targets Fedora 44 and later in a Wayland session with GNOME or KDE. The host must
run Tailscale and must be connected to the same tailnet as the other Splice machines.

## Build and install Splice

Install Rust and Cargo, then run these commands from the repository root:

```sh
cargo build -p splice-app --release
packaging/linux/install.sh
```

The installer copies `splice` to `~/.local/bin`, installs the desktop entry, and enables the
`app-splice.service` user unit. Start it without logging out with:

```sh
systemctl --user start app-splice.service
```

The desktop entry is installed at `~/.local/share/applications/splice.desktop`.

## Allow physical-input detection

Add your user to the `input` group, then re-login:

```sh
sudo usermod -aG input $USER
```

The installed launcher also activates configured membership when a lingering user service keeps
an old group list across logouts. This is common on development machines with user lingering
enabled; no reboot or `loginctl terminate-user` workaround is required.

The physical-input monitor reads `/dev/input/event*` devices in read-only mode. Portal-injected
input does not appear on those devices, so Splice uses evdev events to identify the machine that
last received physical input. Without access to the `input` group, capture activation still works,
but source auto-switching is limited and Splice reports a warning.

## Approve portal access

On the first launch, allow the Input Capture and Remote Desktop portal requests for keyboard and
pointer control. Splice also requests clipboard access through the Remote Desktop session. Allow it
if the portal asks.

Splice keeps each portal session alive and stores replacement restore tokens. GNOME 50 may show the
consent prompt on every launch until Fedora 45. KDE remembers the consent. KDE also shows a persistent
`Input Capture` notification. That notification is normal.

Use a Wayland session. Check the session type with:

```sh
echo "$XDG_SESSION_TYPE"
```

The command must print `wayland`.

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

If Splice cannot detect physical input, confirm the group membership and re-login:

```sh
id -nG
ls -l /dev/input/event*
```

If GNOME asks for consent on each launch, that behavior is expected on GNOME 50. KDE should retain
the consent, and its persistent `Input Capture` notification is expected.

If peers do not appear, run `tailscale status` and confirm that Tailscale is running and that the
peer is online. Splice does not use MagicDNS for peer connections.
