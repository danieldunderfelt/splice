# macOS input platform facts (verified against macOS 26 SDK headers + live experiments, 2026-08)

Authoritative reference for `splice-platform/src/macos/`. Everything here was verified against
the macOS 26 SDK on this machine or live-tested. Follow it.

## Event tap (capture)

- `CGEventTapCreate(tap, place, options, mask, callback, userInfo) -> CFMachPortRef`.
  Locations: `kCGHIDEventTap=0`, `kCGSessionEventTap=1`, `kCGAnnotatedSessionEventTap=2`.
  Options: `kCGEventTapOptionDefault=0` (ACTIVE — may modify/suppress by returning NULL),
  `kCGEventTapOptionListenOnly=1`. Use an **active session-level tap** for Splice.
- Root is NOT required for HID-level taps anymore (header comment is stale); TCC is the gate.
- Run loop: `CFMachPortCreateRunLoopSource` → `CFRunLoopAddSource` on a thread with a RUNNING
  CFRunLoop (dedicated thread). No run loop = zero callbacks, no error.
- Callback discipline: do nothing but timestamp + enqueue to a lock-free/mpsc channel + return.
  Active taps are SYNCHRONOUS — a slow callback lags the whole system's input. Budget ~1 s
  before the watchdog kills the tap; target sub-millisecond.
- Handle both disable events *differently*:
  - `kCGEventTapDisabledByTimeout` (type 0xFFFFFFFE): call `CGEventTapEnable(port, true)`,
    KEEP capture state.
  - `kCGEventTapDisabledByUserInput` (0xFFFFFFFF): unrecoverable via re-enable (Secure Input,
    TCC revoked). Tear down capture cleanly (re-associate cursor! release keys!), recreate tap.
- Health poll: every 5 s check `CGEventTapIsEnabled`; reinstall if re-enable doesn't stick.
  A non-nil, "enabled" tap can still be silently dead after re-signing + Launch Services launch.
- Re-create the tap on `NSWorkspaceDidWakeNotification`,
  `NSWorkspaceSessionDidBecomeActiveNotification` (taps die across sleep/wake and lock/unlock
  on Tahoe and never come back on their own).
- Media keys arrive as `NX_SYSDEFINED = 14` — not in CGEventType; mask bit via
  `CGEventMaskBit(14)` if ever needed (not v1).
- Cannot capture: anything during Secure Event Input (keyboard only — mouse keeps flowing),
  CapsLock physical transitions, per-device attribution.

## Permissions (TCC)

- Three services: `ListenEvent` (Input Monitoring pane), `PostEvent` (toggled by the
  Accessibility switch), `Accessibility` (grants listen+post+AX). **Active tap needs the
  post-level privilege ⇒ Accessibility.** On macOS 13+ the Accessibility grant transitively
  covers listen+post; onboard to ONE toggle: Accessibility.
- Preflight/request: `CGPreflightPostEventAccess()` / `CGRequestPostEventAccess()`,
  `CGPreflightListenEventAccess()` / `CGRequestListenEventAccess()`,
  `AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true})`.
  `CGEventTapCreate` returns NULL exactly when post access is missing.
  NOTE: `AXIsProcessTrusted` returns `Boolean` (unsigned char) — bind as u8, normalize.
- `CGPreflight*` results do NOT update while the process runs; detect revocation by
  periodically attempting a throwaway `CGEventTapCreate` (NULL ⇒ revoked ⇒ tear down capture
  and REASSOCIATE THE CURSOR — revocation while disassociated wedges system input).
- TCC keys the grant to the code-signing designated requirement. Ad-hoc signing = cdhash =
  grants silently die on every rebuild while the toggle still shows ON. Sign with a stable
  identity (self-signed cert OK for dev). Symptom set of stale csreq: tapCreate nil,
  CGEventPost silent no-op, AX error -25211.
- Deep link: `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility`.
- `tccutil reset Accessibility <bundle-id>` for testing (case-sensitive service names).

## Secure Event Input

- While ANY process has SEI on, keyboard events vanish from ALL taps (mouse continues).
  Detect: `IsSecureEventInputEnabled()`; culprit PID via `ioreg -l -d 1 -k IOConsoleUsers`
  → `kCGSSessionSecureInputPID` (key absent = SEI off), then `ps -p <pid> -o comm=`.
  Surface in UI: "keyboard paused: <app> has Secure Input". Also: when SEI ends, pressed-key
  state may have changed — synthesize key-up for everything believed held.

## Capture strategy (freeze, not warp)

- On session start (cursor crosses to a remote machine): hide cursor + disassociate:
  ```
  CGSSetConnectionProperty(_CGSDefaultConnection(), _CGSDefaultConnection(),
                           CFSTR("SetsCursorInBackground"), kCFBooleanTrue)  // private SPI, weak
  CGDisplayHideCursor(0)                          // ref-counted; balance exactly
  CGAssociateMouseAndMouseCursorPosition(false)   // cursor frozen, events carry deltas
  ```
  Read deltas from `kCGMouseEventDeltaX/Y` (fields 4/5) — these are post-acceleration points;
  that is what we want on the wire (see DESIGN decision 10). Tap returns NULL (swallow).
- On session end: `CGAssociateMouseAndMouseCursorPosition(true)`, warp to re-entry point with
  `CGWarpMouseCursorPosition(pt)` then IMMEDIATELY `CGAssociateMouseAndMouseCursorPosition(true)`
  again (SDL trick — cancels the 0.25 s local-event suppression interval), `CGDisplayShowCursor`.
- **Disassociation is global state that survives crashes.** Mandatory: signal handlers
  (SIGSEGV/SIGABRT/SIGTERM) + atexit + a watchdog thread that re-associates + shows cursor if
  the engine thread stops heartbeating. Re-associate before any panic path exits.
- Warp deltas: the next real event's delta after a warp INCLUDES the warp displacement —
  track lastWarp and subtract (only relevant on re-entry).

## Coordinates & displays

- CG global space: origin top-left of main display, y-down, units = POINTS (not pixels).
  NSScreen is bottom-left y-up — do not mix; use CG exclusively.
  `CGGetActiveDisplayList`, `CGDisplayBounds` per display.
- `CGDisplayRegisterReconfigurationCallback` fires twice per change; act on the "after" pass
  (flags without `kCGDisplayBeginConfigurationFlag`) → republish display rects to core.
- Edge detection: per-display rect list, NOT the bounding box union (non-rectangular
  arrangements have unreachable dead regions). A point is on the true outer boundary iff
  stepping 1 pt outward lands in no active display.

## Injection

- `CGEventPost(kCGHIDEventTap, ev)`. Event source: create one
  `CGEventSourceCreate(kCGEventSourceStateHIDSystemState)` and reuse.
  Tag EVERY injected event: `CGEventSetIntegerValueField(ev, kCGEventSourceUserData(42),
  SPLICE_MAGIC)` — the capture tap checks field 42 to distinguish physical from injected
  (physical-activity detection for source arbitration must ignore our own injections).
- Mouse: `CGEventCreateMouseEvent(src, type, pos, button)`; positions absolute CG points; for
  moves also set `kCGMouseEventDeltaX/Y` explicitly (games read deltas). Buttons: 0 left,
  1 right, 2 center, 3+ USB order (`kCGEventOtherMouse*` types for >2).
  Click counting: maintain `kCGMouseEventClickState` (1/2/3) yourself with the system
  double-click interval (`NSEvent.doubleClickInterval`), and set a unique monotonically
  increasing `kCGMouseEventNumber` per press (macOS 27 forward-compat; seed from
  `CGEventSourceCounterForEventType`).
- Keyboard: `CGEventCreateKeyboardEvent(src, vk, down)`. Modifiers are NOT sticky: post
  explicit `FlagsChanged` events for modifier edges (with the correct vk code for the
  modifier! never keycode 0 — that is lan-mouse's phantom-'A' bug), AND set cumulative
  `CGEventSetFlags` on every subsequent event while held. Arrow keys/nav cluster: OR in
  `kCGEventFlagMaskNumericPad|kCGEventFlagMaskSecondaryFn`.
- Key repeat: injected events do not auto-repeat. Synthesize: after system-reported initial
  delay (`NSEvent.keyRepeatDelay`), repeat at `NSEvent.keyRepeatInterval` with
  `kCGKeyboardEventAutorepeat=1`, for the last held non-modifier key only; cancel on any
  key event, Leave, ReleaseAll, disconnect.
- Scroll: `CGEventCreateScrollWheelEvent(src, units, wheelCount, v, h)`.
  Discrete lines: `kCGScrollEventUnitLine`, small ints (±1..±10 per event; chunk larger).
  Smooth: `kCGScrollEventUnitPixel` + set `kCGScrollWheelEventIsContinuous(88)=1`.
  Do NOT attempt momentum-phase injection in v1; forward finger scroll only.
- CapsLock: cannot be toggled via CGEventPost. v1: do not forward CapsLock toggles.
- Wake/activity: `IOPMAssertionDeclareUserActivity(CFSTR("Splice input"),
  kIOPMUserActiveLocal, &id)` on session enter and every ~30 s while entered — synthesized
  CGEvents alone do NOT wake a slept display. Hold a
  `kIOPMAssertionTypePreventUserIdleDisplaySleep` assertion while a session is entered;
  release on Leave.

## Pasteboard

- No change notifications. Poll `NSPasteboard.general.changeCount` at 500 ms (cheap).
- Gate content reads: only read data when offering/serving; use types list +
  `detectPatterns`/`detectMetadata` where possible (macOS 15.4 privacy alert exists behind a
  default-off flag; architecture must tolerate it becoming an ask).
- Lazy provide: `NSPasteboardItem.setDataProvider(_:forTypes:)` — provider callback fetches
  bytes from the peer over TCP, blocking briefly (guard with ~2 s timeout, then provide empty).
- TIFF-first: convert `public.tiff` → PNG before offering `image/png` on the wire.
- Loop guard: when Splice sets the pasteboard from a remote offer, record the resulting
  changeCount and skip it in the poller.

## App shape

- `.app` bundle, `LSUIElement=true` + runtime `NSApp.setActivationPolicy(.accessory)` (both).
- `NSLocalNetworkUsageDescription` in Info.plist. NO `NSAccessibilityUsageDescription` /
  `NSInputMonitoringUsageDescription` (they don't exist).
- Hardened runtime for release; zero exception entitlements (no disable-library-validation).
- No App Sandbox. Not App Store.

## Rust crates

`core-graphics` (event tap, events, display), `core-foundation`, `objc2` + `objc2-app-kit`
(+`objc2-foundation`) for NSPasteboard/NSScreen/NSWorkspace notifications, small hand-written
`extern "C"` for: IOPMAssertion (IOKit), `CGSSetConnectionProperty`/`_CGSDefaultConnection`
(SkyLight, weak), `IsSecureEventInputEnabled` (Carbon). Keycode mapping via `keycode` crate.
