# Splice

Splice shares one mouse, keyboard, and clipboard across macOS and Linux computers on the same Tailscale network. It discovers peers automatically. Move the pointer across an arranged screen edge to control another computer.

Splice is under active development. Linux support targets Fedora 44 or later in a GNOME or KDE Wayland session.

## Prerequisites

Install these on every computer that will run Splice:

- [Rust and Cargo](https://rustup.rs/)
- [Tailscale](https://tailscale.com/download), connected to the same tailnet

On Linux, use a Wayland session. The portals for keyboard, pointer, and clipboard access come from your desktop environment. See [the Linux setup guide](docs/linux-setup.md) for permissions and troubleshooting.

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
cargo build -p splice-app --release
```

The binary is written to `target/release/splice`.

### Build a macOS app

Create a signed `Splice.app` bundle:

```sh
packaging/macos/make-cert.sh
packaging/macos/make-app.sh
```

The first command creates the local `Splice Dev` signing identity. You only need to run it once. The app bundle is written to `build/Splice.app`. Move it to `/Applications`, open it, then enable Splice in **System Settings > Privacy & Security > Accessibility**.

Keep the signing identity when rebuilding. macOS can lose the app's Accessibility permission when an ad-hoc signature changes.

### Install on Linux

Build and install the app for your user:

```sh
cargo build -p splice-app --release
packaging/linux/install.sh
systemctl --user start app-splice.service
```

The installer copies the binary to `~/.local/bin/splice` and enables the `app-splice.service` user service. Complete the portal and physical-input setup in [the Linux setup guide](docs/linux-setup.md).

## Use Splice

1. Start Splice on every computer.
2. Approve the operating system permission prompts.
3. Open Splice from its menu bar or system tray icon.
4. Drag the machine cards so their screen edges touch in the same arrangement as your physical displays.
5. Enable the machines that you want to control.
6. Move the pointer through a shared edge. The keyboard follows the pointer to the other machine.

Splice also synchronizes text and images when **Clipboard sync** is enabled. Use the per-machine pointer-speed controls to adjust remote movement.

Press `Ctrl+Alt+Shift+Escape` to release captured input. You can also choose **Disconnect all** from the app or tray menu.

## Test

Run the workspace test suite:

```sh
cargo test --workspace
```

For implementation details, see [the design document](docs/DESIGN.md).
