# Mac raw input validation, 2026-09-05

The native Mac code and signed app have been built and tested. Physical forwarding, local input
suppression, and game acceptance remain open. This is a development build, not a gaming qualification.

## Build and automated checks

Host: Apple Silicon, macOS 26.6.2, build 25G83. Rust 1.98.0. The active display reports
2560 × 1440 logical points at scale 2. Source commit `16480385543eea76d0fdc4c4b2862fef2009991e`,
with local changes. The binary reports version 1.2.0, protocol 4, target `aarch64-apple-darwin`,
and `dirty: true`.

| Check | Result |
|---|---|
| `cargo test --workspace --locked` | Passed, 203 tests |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed |
| `cargo test -p splice-core --release --test engine_e2e --locked` | Passed, 38 tests |
| `python3 -m unittest discover -s packaging/tests -v` | Passed, 5 tests |
| `cargo build -p splice-app --release --locked` | Passed, native build and link |
| `splice --version-json` | 1.2.0, protocol 4, dirty |
| `packaging/macos/make-app.sh --no-build` | Passed with Developer ID signing |
| `codesign --verify --deep --strict` | Passed for built and installed app |
| `cargo run -p splice-app --example macos_edge_indicator --locked` | Passed on all four edges; visible, updated, hidden, no keyboard focus |
| Installed app `--raw-probe 10` through Launch Services | All 10 device descriptors accepted; Air75 BLE and USB receiver keyboard callbacks decoded without errors |

The first workspace run failed two raw socket tests because macOS lacked loopback aliases.
After adding `127.0.0.2` through `127.0.0.5` to `lo0`, the full suite passed. Loopback results
verify software behavior and are not physical input latency measurements.

The installed app is `/Applications/Splice.app`, bundle identifier `dev.splice.app`, signed with
`Developer ID Application: Dunderfelt Consulting Oy (VTHJ4G9DW9)`. The previous Splice Dev signed
1.1.0, protocol 3 app is backed up at `build/Splice-1.1.0-before-raw.app`. The new bundle is not
notarized. No release was published.

## Descriptor evidence

Descriptors were read from this Mac's IORegistry. The exact binary fixtures are in
`crates/splice-platform/tests/fixtures/hid`. Test input payloads are synthetic and are labeled here
separately from the captured descriptors. No physical report payloads were recorded in these fixtures.

| Device or interface | Connection | Descriptor and synthetic report result | Physical capture and suppression |
|---|---|---|---|
| Apple internal keyboard | FIFO | Pass. Keyboard, independent media report, vendor report, release, and session reset | Unverified |
| Apple internal trackpad | FIFO | Pass for its standard relative mouse report, signed counts, buttons, and independent vendor report | Unverified; this does not establish raw multitouch support |
| Apple headset control | Audio | Pass for media press and release | Unverified |
| Logitech USB Receiver keyboard, `046d:c548` | USB receiver | Pass for keyboard/modifier state and rollover recovery | Keyboard callbacks decoded; suppression unverified |
| Logitech USB Receiver mouse, `046d:c548` | USB receiver | Pass for signed 16-bit motion, vertical/horizontal wheels, buttons 1 and 8, releases, and unused button slots | Descriptor accepted natively; no mouse callbacks during the probe; suppression unverified |
| NuPhy Air75 V3 ISO-1 | Bluetooth Low Energy | Pass for NKRO, Caps Lock, boot keyboard/modifiers, release, and unused usage slots | BLE keyboard callbacks decoded; suppression unverified |

The receiver exposes interfaces, not an authenticated model name for its paired mouse. Mouse model,
DPI, configured polling rate, wired-device coverage, and Bluetooth reconnect results remain unknown.
IORegistry reports interval properties of 1000 µs for the receiver and 8000 µs for the NuPhy and Apple
devices. These values do not measure delivered report cadence.

Descriptor checks found and fixed decoder defects. Wide vendor or padding fields no longer fail
the 32-bit standard-control width check. Declaring NKRO rollover/error bits is valid; setting an
error bit still fails the report without corrupting the previous held state.

Physical setup exposed another premature check. The Air75 declares unused keyboard usages including
`0007:0082`, and the Logitech receiver declares 16 mouse buttons. Descriptor construction previously
rejected either device without receiving any input. Mapping checks now run when a control is asserted
in a report, consistently for keyboard, button, and consumer fields. The existing protocol accepts
buttons 1 through 8. An actual unsupported button or key still fails without changing held state;
the regression tests verify that a subsequent valid report can release the previously held controls.

After installing the keyboard fix, a 10-second native probe received 25 report-ID 1 callbacks of
9 bytes on each of three Air75 IOHID entries, with no descriptor or decoding errors. The entries
mirror the same physical device and their counts must not be summed as unique reports. This verifies
BLE keyboard framing and decoding on the installed signed app. NKRO report-ID 6 remains covered by
synthetic input only. The probe records no report payloads or typed key sequences.

After the receiver fix, another 10-second probe accepted all 10 IOHID device entries. It decoded
87 Air75 keyboard callbacks on each mirrored entry and 418 unnumbered 8-byte keyboard callbacks
from the USB receiver, with zero invalid reports. The receiver's two mouse entries had no callbacks
during this interval, so physical mouse report decoding remains unverified. The app was restarted
normally after probing. Before/after probe JSON and final build/check logs are retained locally under
`build/raw-input-macos-evidence` with the `splice-hid-compat` prefix.

## Mac recovery and indicator changes

Media suppression now releases raw input and recreates its event tap after disablement or a macOS
session change. Sleep and session resignation release capture immediately. Wake and session activation
invalidate suppression readiness until the main tap has been recreated. Raw held-state snapshots are
cleared across these session changes. Physical wake tests are still required, including a modifier held
through sleep and a key released while locked.

The HID stream recognizes the configured emergency chord independently of the desktop callback.
Automated tests prove synchronous local capture-state release without an engine callback. They do not
prove physical cursor recovery. Reserved switch keys remain suppressed until all HID devices release
them, and desktop autorepeat cannot leak the reserved F12 chord during preparation.

The native panel shows the destination and progress while the main window is hidden. The AppKit smoke
test exercised every edge over another application and asserted that no key window was acquired and
the panel ignored mouse events. A screenshot was inspected locally. Geometry tests cover scale 1 and 2,
negative display origins, topology invalidation, and remote Desktop crossings mapped to the Mac display.
Physical dwell/resistance, retreat, tangential motion, decay, fullscreen Spaces, and display hotplug
still need acceptance testing.

The main tap's permission probe now creates an active tap, so it tests the privilege needed for local
suppression. Raw readiness uses published tap state instead of dereferencing a tap pointer from another
thread while that pointer may be released during recreation.

## Remaining acceptance

The user granted Accessibility and Input Monitoring to the installed Developer ID signed app and
confirmed it starts working. Launch Services probes now receive physical HID callbacks. A direct
terminal invocation still reports missing Input Monitoring under that launch context; it does not
measure the installed app's permission. Use the Launch Services command in the handoff checklist.

Unused descriptor controls no longer require disconnecting the Air75 or Logitech receiver. Forwarding
actual buttons above 8 would require changing shared validation and Linux uinput capabilities together.
These Mac changes preserve that wire contract and the destination's advertised capabilities.

No physical Mac → GNOME → KDE handoff, Caps Lock suppression, Secure Input revocation, network-failure
release timing, unplug/reconnect, Bluetooth wake, SIGKILL cursor recovery, or game test has passed in
this run. No direct-attachment, VirtualHere, or input-to-display comparison was performed. The installed
app also observed a peer closing the Welcome handshake; destination protocol/build readiness remains
to be established. Use the [handoff checklist](raw-input-macos-handoff.md) for the remaining checks.

## Native API references

The local macOS SDK headers were checked for callback types, permission values, and the timestamp
callback's macOS 10.15 availability. Source timestamps remain monotonic enqueue times. A timestamp
API change was not needed to validate the existing framing contract.

Apple's [IOHIDManager implementation](https://github.com/apple-oss-distributions/IOKitUser/blob/main/hid.subproj/IOHIDManager.c)
registers each device's input callback with the manager-supplied context and owns the report buffers.
The [HIDAPI Mac backend](https://github.com/libusb/hidapi/blob/master/mac/hid.c) also treats numbered
reports as containing their report-ID prefix. The native Air75 probe confirmed that framing for its
keyboard report. Other report types still require native evidence; source inspection does not replace
that check.
