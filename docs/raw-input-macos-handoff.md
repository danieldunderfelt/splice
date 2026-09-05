# Build and validate raw input on macOS

Continue physical acceptance testing of Splice 1.2.0, KVM protocol 4. The native Mac implementation,
builds, signing, descriptor fixtures, and screen-edge indicator checks are recorded in
[Mac validation](raw-input-macos-validation.md). A Developer ID signed test app has been installed at
`/Applications/Splice.app`; no release was published. Do not call raw mode ready for gaming until the
physical-device and game checks below pass.

Keep Desktop mode fully supported. Do not substitute Desktop input when Raw fails. The repository
also prohibits adding code comments. Read [the design](raw-input-design.md),
[the macOS platform research](research/macos-input.md), and `AGENTS.md` before changing the backend.

## Understand what is implemented

| Area | Implementation |
|---|---|
| Physical input | IOHIDManager report callbacks, descriptor-driven relative axes, buttons 1–8, keyboard arrays and NKRO bitmaps, standard consumer keys |
| Mac suppression | Existing session event tap for mouse/keyboard plus a media-key event tap for type 14 |
| Linux injection | Persistent relative uinput mouse and keyboard; per-device held state; high-resolution wheels |
| Transport | Dedicated TCP 41719, TCP_NODELAY, control ownership, Tailscale identity and IP checks, random ticket, strict sequence and session checks |
| Recovery | Bounded report queue; explicit overload/error; heartbeat; release on socket loss; independent local emergency release |
| Selection | Per-destination Desktop/Raw; manual Control buttons; Ctrl+Alt+F12 cycles workspace order |
| Crossing | Immediate, Dwell, Resistance on Mac sources; current Desktop return and onward crossings share the gesture policy |
| Diagnostics | Input settings, active/preparing state, progress, and errors are included in the existing exported UiState |

Raw mode requires the user to select **Stay on selected computer**. There is no destination
edge observer in this implementation. Raw counts never predict Linux pointer coordinates.
Linux source edges continue using Immediate crossing. The Splice panel and a native Mac panel beside
the physical screen edge show gesture progress. The native panel works without the app window open.

The HID decoder accepts unused standard-control slots in descriptors. It checks the mapping when
a report asserts a control, so unused NKRO keys or extra receiver buttons do not prevent activation.
An asserted control without a supported mapping ends the session with an error; a later valid report
clears the affected device error so the user can retry. Invalid descriptors and ambiguous wheel
scaling still prevent activation. Vendor-defined usages are ignored because vendor commands,
firmware, RGB, and arbitrary USB passthrough are outside the standard-input contract.

Source timestamps are monotonic callback enqueue times. They are not hardware timestamps or
one-way latency. The existing peer traffic counters describe the control/Desktop connection;
they do not measure the raw socket. Do not use those counters to claim raw report latency.

## Locate the code

- `crates/splice-platform/src/macos/raw.rs`: IOKit FFI, discovery, feature reports, capture health, local media suppression, report stream.
- `crates/splice-platform/src/macos/raw/probe.rs`: bounded read-only HID report-format probe, exposed as `splice --raw-probe SECONDS`.
- `crates/splice-platform/tests/hid_devices.rs`: captured Apple, Logitech, and NuPhy descriptor regressions and explicit unsupported cases.
- `crates/splice-app/src/edge_indicator.rs`: native AppKit progress panel and display-coordinate tests.
- `crates/splice-platform/src/macos/tap.rs`: cursor capture, passive edge contact, Desktop translation, shared switching shortcut.
- `crates/splice-platform/src/raw/hid.rs`: portable descriptor decoder and report fixtures.
- `crates/splice-platform/src/raw/shortcut.rs`: callback-order-independent switch state and suppression tests.
- `crates/splice-platform/src/raw/usages.rs`: standard keyboard/consumer usages mapped to Linux input codes.
- `crates/splice-platform/src/linux/raw.rs`: native relative devices and ordered injection groups.
- `crates/splice-proto/src/raw.rs`: raw report validation and aggregate held-state ledger.
- `crates/splice-core/src/raw_transport.rs`: raw socket framing, authorization, heartbeats, release guard.
- `crates/splice-core/src/engine/inner/raw.rs`: target preparation, capture commit, handoff, cancellation.
- `crates/splice-core/src/engine/inner/crossing.rs`: gesture integration; `edge_policy.rs` contains its pure state.
- `crates/splice-core/tests/engine_e2e.rs`: three-computer raw sessions, restart, failures, clipboard load, and gesture regressions.

`hidparser` 1.0.4 has no numeric getters for physical bounds. The decoder reads the descriptor's
physical literals before parsing and matches the parser's bound wrappers against those values.
It bounds descriptor expansion before entering the parser, reads wheel multiplier feature reports,
and rejects conflicting multipliers whose collection identities the parser cannot distinguish.
Add captured descriptors as portable regression fixtures when a device exposes a decoder bug.

## Build on the Mac

1. Transfer the complete working changes to the Mac. Check `git status --short`, including untracked
   raw modules and these documents. A checkout of the old HEAD alone does not contain this work.
2. Set up loopback aliases for the multi-computer tests:

```sh
for address in 2 3 4 5; do
  sudo ifconfig lo0 alias "127.0.0.${address}" up
done
```

3. Run native checks:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p splice-core --release --test engine_e2e --locked
python3 -m unittest discover -s packaging/tests -v
cargo build -p splice-app --release --locked
./target/release/splice --version-json
```

The reported version must be 1.2.0 and protocol 4. A dirty source checkout must report itself as dirty.
Linux cross-checks of the platform crate do not replace this native build and link step.

4. Build the app with the user's stable Developer ID identity:

```sh
export SPLICE_CODESIGN_IDENTITY='Developer ID Application: YOUR NAME (TEAMID)'
packaging/macos/make-app.sh --no-build
codesign --verify --deep --strict build/Splice.app
```

Use the actual Keychain identity. Do not change bundle identifier `dev.splice.app` or switch signing
identities between tests. Follow [release signing and notarization](releasing.md) for distribution;
`make-app.sh` signs a local test bundle but does not notarize it.

5. Quit the running Mac instance, install the test bundle at its normal stable path, and launch it.
   Grant Accessibility and Input Monitoring to that installed bundle, then restart it. Keep the
   working release available for restoration. Do not run two Splice instances during capture checks.
6. Build/install protocol 4 on both Linux destinations. Verify `/dev/uinput` access using
   [Linux setup](linux-setup.md). Allow TCP 41717, 41718, and 41719 on the Tailscale interface.
7. On the Mac, select Raw for one Linux destination, select **Stay on selected computer**, and click
   its **Control** button. Preparation must finish before the Mac begins suppressing local input.

## Prove capture and suppression first

Use a USB cable mouse and keyboard, then a wireless USB receiver. Record the devices, descriptor,
connection type, polling rate, DPI, OS versions, build identity, and network path with each result.

1. Compare physical HID reports with the decoded raw reports. Cover signed 8/16/32-bit axes,
   multiple report IDs, composite receivers, extra buttons, keyboard arrays, NKRO, and media keys.
   Verify the report-ID prefix and byte length against the native SDK and actual callbacks.
2. Confirm that moving, clicking, typing, scrolling, and pressing media keys affect only the selected
   destination. Use a harmless text field on the Mac to detect leaks. Check Caps Lock explicitly:
   IOHID may report the physical switch while the Mac also changes its local lock state.
3. Hold Shift across activation and each handoff. Test two keyboards holding the same modifier and
   one releasing it. Test mouse buttons held across a handoff and device removal.
4. Repeatedly switch Mac → GNOME → KDE → Mac with Ctrl+Alt+F12, holding the chord while the next target
   prepares. Neither F12 nor its reserved Ctrl/Alt state may leak into the next target. Test either
   callback arriving first, delayed callbacks, key release during preparation, and rapid repeats.
5. Check vertical and horizontal wheel signs with macOS natural scrolling both enabled and disabled.
   Raw wheel reports preserve physical direction; destination settings control their interpretation.
   Cover high-resolution scrolling and feature-report multipliers without losing fractional detents.
6. Remove/reconnect devices, including the last mouse or keyboard. A removed device must release
   its keys/buttons. Removing the last required device must restore local control. An unsupported
   descriptor or transient rollover report must produce an error and allow a documented retry.
7. Add Bluetooth keyboard/mouse validation using the same reports and session protocol. Exercise
   wake/reconnect before claiming Bluetooth support on tested hardware.

Inspect callback scheduling, event-tap timeout handling, and device lifetime against the native SDK.
Consider `IOHIDManagerRegisterInputReportWithTimeStampCallback` for hardware timestamps if supported
on the deployment target. Keep clocks explicit. Add a bounded capture trace or native test harness
when needed; do not log keystrokes or clipboard contents in ordinary diagnostics.

## Exercise recovery and the gesture UI

- Trigger the configured emergency chord with the network unavailable. The Mac must restore its
  pointer locally without waiting for the engine or socket. Both Linux destinations must release
  held state. Test the Linux physical emergency chord as well.
- Revoke Input Monitoring and Accessibility, enable Secure Input, lock/unlock, and sleep/wake during
  capture and preparation. Show the cause and release input. Check the media tap is still alive
  after wake. Media tap failure now ends capture and recreates the tap; retry requires a fresh session.
- Disconnect Tailscale, close the raw socket, kill the destination, and restart the source with the
  same identity. Check stale input and callbacks cannot take ownership of the new session.
- Exercise a source process crash, including SIGKILL, with an independent recovery method available.
  Existing signal handlers and the in-process watchdog cannot run after SIGKILL. Do not assume they
  prove cursor recovery for that case; verify OS behavior and add external recovery if necessary.
- Test queue overload and delayed acknowledgements. The nominal raw write/read timeout is 750 ms,
  with 200 ms heartbeats. Scheduling and preparation add time; measure actual release latency.
- Test the native screen-edge indicator with Dwell/Resistance and the app window hidden.
  Cover retreat, tangential motion, pause/decay, display scaling, layout changes, and every Desktop
  transition. Input used to push through the edge must not become a target camera jump.

## Compare actual game input

Compare direct attachment, VirtualHere, Desktop mode, and Raw mode on the same local network.
Keep DPI, polling rate, destination sensitivity, acceleration, and test application unchanged.
Measure report count/sign preservation and cadence separately from input-to-display latency.
Exercise 125, 500, and 1000 Hz; measure higher rates on suitable hardware. TCP packet loss can still
stall an ordered stream. Include clipboard transfer, direct versus relayed Tailscale paths, and
network impairment. Do not call loopback results a hardware latency benchmark.

Run native Wayland, XWayland, and Proton games on both GNOME and KDE. Confirm relative camera input,
buttons, keyboard rollover, repeat, and focus lock. Record failures by game and backend rather than
promising universal compatibility.

## Return evidence with the Mac changes

Provide native build/test results, captured descriptor regression fixtures, a device matrix with
pass/fail outcomes, local suppression and emergency-release evidence, and the direct/VirtualHere
comparison. Link any remaining unsupported devices or game cases. Update the design status and
these instructions to reflect what was actually proven.

For a read-only native report check, quit the running instance and launch the installed signed app
through Launch Services. Executing its binary directly from a terminal can use the terminal's
Input Monitoring permission instead of the app's grant.

```sh
open -n -g -W -a /Applications/Splice.app \
  --stdout /tmp/splice-hid-probe.json --stderr /tmp/splice-hid-probe-error.txt \
  --args --raw-probe 10
```

Move the intended mouse and press and release test keys during that interval. The probe records
descriptor bytes, connection metadata, callback counts, lengths, and errors. It does not store input
payloads, key sequences, clipboard data, serial numbers, or Bluetooth addresses. It does not suppress
input, and therefore cannot prove suppression or game behavior. Restart the app after probing.
`ReportInterval` is a device property, not a measured polling rate. Reports from a rejected descriptor
have framing checks only, because there is no supported decoder for them.

The native indicator can also be checked independently of input permissions and the engine:

```sh
cargo run -p splice-app --example macos_edge_indicator --locked
```
