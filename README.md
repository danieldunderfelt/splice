# Splice

Splice shares one mouse, keyboard, clipboard, and files across macOS and Linux computers on the same Tailscale network. It discovers peers automatically. Move the pointer across an arranged screen edge to control another computer.

The workspace version is Splice 1.2.0.

Splice is under active development. Linux support targets Wayland sessions: GNOME and KDE through the desktop portals, and KDE, Hyprland, sway, niri, COSMIC and other layer-shell compositors through a native overlay and uinput backend.

## Prerequisites

Install these on every computer that will run Splice:

- [Rust and Cargo](https://rustup.rs/)
- [Tailscale](https://tailscale.com/download), connected to the same tailnet

On Linux, use a Wayland session. Splice picks the best capture, injection and clipboard implementation your compositor offers, and the window lets you switch. See [the Linux setup guide](docs/linux-setup.md) for the support table, the udev rule, and troubleshooting.

### Build dependencies

macOS needs the Xcode command line tools:

```sh
xcode-select --install
```

Linux needs a C toolchain, `pkg-config`, and the development packages for GTK4 4.10 or newer, FUSE 3, Wayland, xkbcommon, libudev, D-Bus, EGL and X11. GTK4 is for the file shelf, FUSE 3 for the read-only mount behind deferred file drags, and the rest for the window and the input backends. The `fuse3` package belongs on the list as well: the mount runs through its `fusermount3` binary at run time.

Debian, Ubuntu, Mint:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libfuse3-dev fuse3 \
  libwayland-dev libxkbcommon-dev libudev-dev libdbus-1-dev libegl1-mesa-dev \
  libx11-dev libxi-dev libxcursor-dev libxrandr-dev libxinerama-dev
```

Fedora, RHEL, and other RPM distributions:

```sh
sudo dnf install gcc pkgconf-pkg-config gtk4-devel fuse3-devel fuse3 \
  wayland-devel libxkbcommon-devel systemd-devel dbus-devel mesa-libEGL-devel \
  libX11-devel libXi-devel libXcursor-devel libXrandr-devel libXinerama-devel
```

openSUSE:

```sh
sudo zypper install gcc pkgconf-pkg-config gtk4-devel fuse3-devel fuse3 \
  wayland-devel libxkbcommon-devel systemd-devel dbus-1-devel Mesa-libEGL-devel \
  libX11-devel libXi-devel libXcursor-devel libXrandr-devel libXinerama-devel
```

Arch, CachyOS, EndeavourOS:

```sh
sudo pacman -S --needed base-devel gtk4 fuse3 wayland libxkbcommon systemd-libs \
  dbus libglvnd mesa libx11 libxi libxcursor libxrandr libxinerama
```

FUSE 3 is the most recent addition, so a machine that built Splice before file drag and drop landed needs that package added. Without it the build stops in the `fuser` crate with `The system library fuse3 required by crate fuser was not found`; a missing GTK4 development package stops it in `gtk4-sys` the same way.

## Run from source

From the repository root, run the desktop app:

```sh
cargo run -p splice-app
```

Run the same command on each computer. Splice connects through the local Tailscale service and listens for peers on the Tailscale interface.

To run without the graphical interface, use the headless daemon:

```sh
cargo run -p splice-daemon
```

The daemon is intended for servers and debugging. The desktop app is the normal way to use Splice.

Set `RUST_LOG` to change log verbosity:

```sh
RUST_LOG=debug cargo run -p splice-app
```

## Build

Build the optimized desktop binary:

```sh
cargo build -p splice-app --release --locked
```

The binary is written to `target/release/splice`. On Linux the same executable also runs the GTK4 file shelf in a separate process, so a Linux build needs the [build dependencies](#build-dependencies) installed first: GTK4 4.10 or newer, FUSE 3, and the Wayland, xkbcommon, libudev, D-Bus, EGL and X11 development packages.

CI builds every target with Rust 1.98.0 and runs these checks; run them before pushing:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p splice-core --release --test engine_e2e --locked
python3 -m unittest discover -s packaging/tests -v
```

Run the tests as your own user, not as root. A few file-ownership tests make a directory unreadable and expect the read to fail, and root is never refused.

### Build a macOS app

Create a signed `Splice.app` bundle:

```sh
packaging/macos/make-cert.sh
packaging/macos/make-app.sh
```

The first command creates the local `Splice Dev` signing identity. You only need to run it once. The app bundle is written to `build/Splice.app`. Move it to `/Applications`, open it, then enable Splice in **System Settings > Privacy & Security > Accessibility**.

Keep the signing identity when rebuilding. macOS can lose the app's Accessibility permission when an ad-hoc signature changes.

### Install on Linux

Splice runs in a Wayland session. Pick the route that matches your
distribution; all of them install the binary, the desktop entry, the `app-splice.service` user
unit, and the udev rule that grants input-device access.

| Distribution | Route |
|---|---|
| Debian, Ubuntu, Mint | `cargo install cargo-deb && cargo build -p splice-app --release --locked && cargo deb -p splice-app --no-build`, then `sudo apt install ./target/debian/splice_*.deb` |
| Fedora, RHEL, openSUSE | `packaging/rpm/build.sh` (needs `rpm-build` and `rpmdevtools`), then `sudo dnf install ~/rpmbuild/RPMS/*/splice-*.rpm` |
| Arch, CachyOS, EndeavourOS | `cd packaging/arch && makepkg -si` |
| SteamOS, Bazzite, Silverblue and other immutable systems | Flatpak: see `packaging/flatpak/` |
| Any distribution, per user | `cargo build -p splice-app --release --locked && packaging/linux/install.sh` |

The per-user installer puts the binary in `~/.local/bin` and asks for `sudo` once to install the
udev rule. Remove that install again with `packaging/linux/install.sh --uninstall`, for example
before switching to a distribution package.

Then start Splice from the app menu, or from a terminal:

```sh
splice          # start the background service if needed and open the window
splice quit     # stop the service
```

Closing the window leaves the service running. Tick **Start Splice at login** in the window, or
enable the systemd user unit, to have it start with your session:

```sh
systemctl --user enable --now app-splice.service
```

Complete the portal setup in [the Linux setup guide](docs/linux-setup.md).

## Use Splice

1. Install the same current build and start Splice on every computer. Protocol 7 rejects older clients.
2. Approve the operating system permission prompts.
3. Open Splice from its menu bar or system tray icon, or launch it again from the app menu to bring the window back.
4. Drag the machine cards so their screen edges touch in the same arrangement as your physical displays.
5. Enable the machines that you want to control.
6. Move the pointer through a shared edge. The keyboard follows the pointer to the other machine.

Splice synchronizes text and images when **Clipboard sync** is enabled. Open the file shelf from **Files** or the tray to offer files and folders, receive copied files, or pick up an incoming drag. Offering files sends only metadata; receiving or an accepted native drop starts the copy. See [sharing files](docs/file-sharing.md) for the two-step handoff and copy/paste flow.

Use the per-machine pointer-speed controls to adjust remote movement.

Press `Left Shift+Right Shift+Escape` to release captured input. You can also choose **Disconnect all** from the app or tray menu.

## Diagnostics and updates

Open **Diagnostics** to inspect connection phases, build identities, heartbeat age, and input queue
timing, or save a report without clipboard contents or typed keys. See [diagnostics](docs/diagnostics.md).

Use **Updates** to check, download, and install signed releases on this computer or an authorized
Tailnet peer. See [updating Splice](docs/updates.md) for supported installations and port requirements.
Existing protocol 2 clients need one manual upgrade.

GitHub Actions builds and tests Linux x86-64 and both macOS architectures. Release tags produce signed
update manifests and Developer ID signed, notarized Mac bundles. Set the [required secrets](docs/release-secrets.md)
and follow [the release guide](docs/releasing.md) before publishing the first release.

## Test

Run the workspace test suite:

```sh
cargo test --workspace
```

The Mac tray harness is opt-in with the `native-ui-tests` feature. Run native tests only during a scheduled desktop test session; they can open windows or interact with input.

The suite checks full meshes of three and five machines, restart convergence, multi-hop input,
clipboard isolation, and network failure handling. The KDE compositor check runs separately
inside a Wayland desktop with layer-shell support:

```sh
cargo test -p splice-platform live_overlay_arms_both_edges_after_startup -- --ignored --nocapture
```

For implementation details, see [the design document](docs/DESIGN.md).
The [September 2026 investigation](docs/multi-machine-investigation.md) records the reproduced
failures, fixes, and remaining live verification.

## Raw mouse and keyboard input

Splice supports selectable raw input from Linux and Mac sources to Linux destinations through a
relative virtual mouse and keyboard. Desktop mode remains the default in every existing direction.
Raw mode uses the destination's screen boundaries to return or move onward. Enable **Stay on selected
computer** to disable automatic switching during games. Use **Ctrl+Alt+F12** to cycle through computers,
or use the **Control** buttons during capture. On Linux, start capture by crossing a screen edge.
Mac sources also have Immediate, Dwell, and Resistance crossing.

See [Linux raw input setup and validation](docs/raw-input-linux.md) for device requirements and checks.
Native Mac capture and gaming validation are pending. See the [implementation status](docs/raw-input-design.md)
and [Mac build and validation handoff](docs/raw-input-macos-handoff.md) before releasing raw mode.
Input uses UDP 41717 (Desktop) and UDP 41719 (Raw) on the Tailscale interface.
Keep TCP 41717 for control and clipboard, TCP 41718 for updates, and TCP 41720 for file transfers. Every computer must use protocol 7.
