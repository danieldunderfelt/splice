# Use raw input between Linux computers

Raw mode sends physical mouse counts and keyboard transitions to a relative virtual mouse and
keyboard on another Linux computer. Desktop mode remains supported in every existing direction.
Linux-to-Mac control uses Desktop mode.

## Set up both computers

1. Build the same current Splice checkout on both computers with `cargo build --release -p splice-app`.
2. Install the permissions described in [Linux setup](linux-setup.md). The source needs read access
   to its physical `/dev/input/event*` devices. The destination needs `/dev/uinput` access.
3. Allow TCP 41719 on the Tailscale interface. Keep KVM TCP 41717 and updater TCP 41718 available.
4. Arrange the computers in the workspace.
5. On the source, choose **Raw input** for the Linux destination.
6. Enable **Stay on selected computer**.
7. Cross the arranged screen edge to start capture.
8. Press **Ctrl+Alt+F12** to cycle through enabled, connected computers in workspace order.
   The source computer is included. The configured emergency chord also returns control locally.

A Linux source starts through an edge because Wayland capture must already be active to suppress
local input. During capture, **Control** buttons and the shortcut can switch remote computers
without releasing that Wayland session. Changing input settings returns control locally.

When switching from Raw to Desktop, held keyboard keys and compositor-reported buttons are
synchronized before forwarding resumes. Splice waits up to 500 ms if only one stream reports any
held mouse buttons, then returns control locally with an error if they still disagree. Release the
buttons and cross the edge again before retrying. This can happen when a button was already held
before Wayland capture started. This presence check preserves compositor button mappings; it cannot
prove that every simultaneously held physical button has a corresponding compositor report.

Raw mode locks focus because device counts cannot predict where a game puts its pointer.
Linux starts with Immediate crossing. Dwell and Resistance remain Mac source features.

## Supported devices and settings

USB cables, wireless USB receivers, and Bluetooth devices use the same evdev report path. The source
needs a relative mouse and keyboard. Bluetooth UHID devices are included in discovery. Actual
Bluetooth reconnect, sleep, and radio behavior still need hardware validation.

The current wire format supports eight mouse buttons, physical keyboard codes, and standard consumer
keys. Extra descriptor capabilities do not prevent ordinary input. If a device actually emits an
unsupported axis or button, capture ends with an error. Touchpads, tablets, controller axes, absolute
volume controls, vendor commands, and USB device-management traffic are outside Raw mode.

Stop exclusive keyboard or mouse grabs in services such as keyd, kanata, or input-remapper before
using Raw mode. Such grabs hide physical reports from Splice. Splice never replaces those reports
with accelerated compositor motion. Runtime checks compare captured compositor activity with raw
activity and report missing input. A quiet device cannot prove that no other service has grabbed it.
Wheel activity is excluded from this check because compositor wheel events can be duplicated.

The destination owns pointer acceleration, natural scrolling, keyboard layout, and key repeat.
Source Desktop sensitivity and scrolling preferences do not alter raw counts. Configure the game
for raw mouse input when available. Generic virtual input does not guarantee compatibility with
every game's input handling.

Device hotplug, permission revocation, lost kernel reports, an overflowing queue, or connection loss
ends the raw session and releases held input. After correcting the reported cause, cross the edge
again. Splice does not automatically substitute Desktop mode.

## Run automated checks

Run the normal suites without capturing any local input:

```sh
cargo test --workspace
cargo test --release -p splice-core --test engine_e2e
cargo clippy --workspace --all-targets -- -D warnings
```

Run the native fixture separately on a Linux machine with the udev rule installed:

```sh
cargo test -p splice-platform native_evdev_ --lib -- --ignored --nocapture
cargo test -p splice-platform native_raw_device_capabilities --lib -- --ignored --nocapture
cargo test -p splice-platform native_raw_source_descriptors --lib -- --ignored --nocapture
```

The first command runs two tests, each with its own uinput fixture, exclusively grabbed before
emitting input. They never grab a physical device. They check real evdev counts, wheel resolution,
held-state snapshots, delayed Wayland callbacks during Raw-to-Desktop switching, overload release,
and read errors after fixture removal. Active physical hotplug recovery remains an acceptance check.
The second command creates receiver devices and checks their capabilities without emitting input.
The third checks physical source capabilities and read permissions without grabbing or forwarding
input. These tests do not establish physical Wayland suppression or game latency.

## Complete native acceptance

Use the same build on Fedora KDE and Fedora GNOME. Repeat the checks with each computer as source.
Keep a local recovery keyboard available while validating capture.

1. Confirm Desktop mode still crosses both edges, shares the clipboard, and reconnects normally.
2. Select Raw mode and cross an edge. Confirm the target responds and the source desktop receives
   neither pointer movement nor typing while captured.
3. Hold Shift before crossing. Type on the target, then release Shift and return home. Confirm
   both computers type normally afterward. Repeat with a held mouse button and a dragged window.
4. Cycle across three computers with **Ctrl+Alt+F12**. Confirm one switch per press and no shortcut
   keystrokes on the new target. Hold another modifier while switching between Raw and Desktop targets.
5. Use the emergency chord, disconnect the network, and unplug a device during separate sessions.
   Confirm local control returns and every target releases held keys and buttons.
6. Reconnect the device and retry. Repeat after screen lock, suspend, and Bluetooth reconnect.
7. Compare direct attachment, VirtualHere, Desktop mode, and Raw mode using the same mouse, DPI,
   polling rate, network route, and game settings. Record aiming behavior and latency under load.
   Repeat while transferring a large clipboard payload.

Record the source compositor and capture backend, destination compositor, mouse connection type,
polling rate, game runtime, and build identity with each result. Export **Diagnostics** if a check
fails. Hardware gaming feel and physical KDE/GNOME suppression remain native acceptance work.

Mac implementation and validation belong in the [Mac handoff](raw-input-macos-handoff.md). The shared
`RawCapture::begin` contract now accepts an `Arc<RawOperation>` for failure identity. The readiness
method is now named `prepare` because Linux also gates compositor delivery during preparation.
The Mac changes in this patch only adapt to those shared trait signatures. Native Mac capture behavior remains separate work.
