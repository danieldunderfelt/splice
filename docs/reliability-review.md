# Reliability review — 9 September 2026

This pass reviewed the application lifecycle, native capture and injection, raw input,
network sessions, clipboard sharing, discovery, configuration and update paths. Existing
uncommitted raw-input work was preserved. The fixes below address defects found in that
review; passing tests does not establish that every hardware or compositor combination is
free of defects.

## Phantom typing and uppercase text

macOS `FlagsChanged` notifications were treated as ordinary keycodes and used to toggle
remembered state. A notification with keycode zero invented an A press. Duplicate modifier
notifications or a release without a remembered press could leave Shift held. Caps Lock
could also enter the held-key ledger and be replayed as another press on a Linux handoff.

Capture now accepts only real left/right modifier keycodes and derives their state from
the corresponding device flag. Desktop Caps Lock notifications are excluded from held-key
replay. Handoff checks remembered keys against current macOS key state and is serialized
with input callbacks. Raw HID Caps Lock handling remains physical key edges.

Tap timeout, session suspension and Secure Input invalidate remembered state and release
the remote session. Starting capture while the tap is unavailable or Secure Input is
active returns an error. Recovery starts with a clean ledger.

Regressions cover keycode-zero notifications, repeated Shift releases, both Shift keys held,
Caps Lock across handoff, stale held keys, tap timeout and Secure Input transitions. The
phantom-key, stale-key and timeout tests failed before the fixes.

## macOS menu crash

The tray rebuilt its native menu on routine state updates, including round-trip latency
updates. AppKit can retain and dispatch a menu item after this rebuild, while the menu
library has freed that item's backing Rust state. Local crash reports place the observed
aborts in AppKit action dispatch. A regression using the old menu reproduced a native
click failure after rebuilding it.

The menu and static item identities now persist. Machine labels and checkmarks update in
place. Membership changes wait until native menu tracking and its action dispatch finish.
Menu actions also wake the UI immediately. Notification observers are removed on drop.

The native AppKit regression checks 100 state updates, 102 Open actions, and deferred
membership changes. Its identity check failed before the fix. It exercises real native
menu dispatch without requiring Accessibility UI automation.

## Clipboard, network and lifecycle

- Clipboard offer installation previously awaited the platform backend on the engine's
  input loop. A stalled backend could delay key-up and emergency release indefinitely.
  One bounded worker now applies only the latest offer, with cancellation and a deadline.
  End-to-end tests hold a real mock backend operation pending while input release and
  Panic complete, then check replacement, disabling sync and failed offers.
- A content-based clipboard echo guard suppressed legitimate later copies of previously
  received text. Ownership checks now live in the platform backends. macOS pasteboard
  writes and ownership marking are serialized with its poller. Tests cover copying an
  earlier remote value and copying text from an offer that failed to apply.
- Heartbeat timeouts previously waited for the next cadence tick. They now wake at the
  actual expiry and issue the next probe immediately after a miss. Tests cover the first
  expiry, consecutive misses and recovery.
- Accepted connections are limited to 16 concurrent authorization/handshake operations.
  Tests verify failure recovery and that incomplete handshakes retain their admission
  permits. Established sessions release those permits.
- Linux window registration now distinguishes a live window from a pending child launch.
  Closing and reopening within the spawn grace period works; simultaneous opens remain
  deduplicated. An old child's exit cannot clear a newer pending launch.
- Linux Quit stops the window's connection worker. Losing an established connection
  unexpectedly allows one service restart attempt. Failed startup switches to connection
  retries; explicit Retry permits another start, preventing repeated failed launches.
  The service stops accepting connections and gives client tasks bounded time to write
  Quit before its runtime exits, including when engine startup failed. A socket-pair
  regression verifies the message reaches the window and the client exits cleanly.
- macOS injector destruction aborts repeat and keep-awake tasks, releases held input and
  sleep assertions, and releases the CoreGraphics event source. Capture and clipboard
  workers stop when the engine's event receiver closes. Workspace and display observers
  are unregistered instead of retaining old platform instances after restart. Display
  callbacks resolve a registration ID to owned state; callbacks racing shutdown never
  dereference a freed platform pointer.

## Verification and remaining hardware checks

The validation covers workspace tests and strict Clippy checks on native Apple Silicon
macOS and a Linux ARM64 container, release-mode engine integration tests, release builds
for both systems, native menu dispatch, and packaging tests. The new regressions run against actual
capture event handling, native menu objects, socket sessions or connected test engines.
Clipboard fault tests use a controlled backend and do not overwrite the user's clipboard.

| Check | macOS ARM64 | Linux ARM64 |
| --- | --- | --- |
| Workspace, all targets | 246 passed, 1 hardware test ignored | 251 passed, 7 hardware tests ignored |
| Native menu regression | Passed, 102 Open actions | Not applicable |
| Clippy, warnings denied | Passed | Passed |
| Release engine integration | 49 passed | 49 passed |
| Release application build | Passed | Passed |

The five Python packaging tests also passed. Ignored tests require physical HID devices,
uinput/udev access or a live Wayland compositor; they are not counted as passed.

Reproduce the automated checks on each system with:

```sh
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p splice-core --release --test engine_e2e --locked
cargo build -p splice-app --release --locked
python3 -m unittest discover -s packaging/tests -v
```

Before distributing a release, exercise the built app on the actual Mac and Linux peers:

1. Cross repeatedly with no keys held, either Shift held, both Shift keys held, and after
   toggling Caps Lock. Check desktop and raw input separately.
2. Interrupt an active session with sleep, screen lock, Secure Input and a lost network
   connection. Confirm held keys release and a later handoff works.
3. Keep the Mac tray menu open while peers connect/disconnect and latency changes, then
   repeatedly choose Open Splice.
4. Exercise clipboard ownership on the deployed Wayland portal/data-control backend,
   including rapid copies and a peer disconnect during a promised-data request.

Those physical cross-machine checks were not performed in this pass. The installed app
and peer installations were not replaced by the source changes.
