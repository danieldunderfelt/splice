# Splice — Design Document

Splice is a Tailscale-native software KVM for macOS and Linux (Fedora, Wayland: GNOME + KDE).
One mouse and keyboard shared across up to N machines by moving the cursor across screen edges.
Reliability is the #1 requirement; every design decision below exists to kill a known failure mode
of prior tools (lan-mouse, Deskflow/Synergy, Barrier). See `docs/research/` for the verified
platform research backing these decisions. **Do not deviate from the decisions in this document
without an explicit note in code explaining why.**

## Product requirements (from the user)

- Auto-discover all machines on the tailnet running Splice; connect automatically. Zero pairing UX.
- Graphical arrangement UI: machine cards showing their *actual* monitor rectangles to scale,
  drag to arrange relative to each other, per-machine enable/disable toggle, disconnect-all reset.
- Up to 3+ machines side by side. Mouse crosses edges effortlessly; keyboard follows the mouse.
- **Symmetric peers, no server role**: the input "source" is whichever machine most recently
  produced *physical* input. The user can plug their mouse/keyboard into any machine and it
  Just Works from there.
- Clipboard sync (text + images; lazy transfer).
- Latency: expect direct WireGuard paths (~0.1–0.3 ms overhead measured); warn on DERP relay.

## Architecture overview

Single Rust workspace. One process per machine: `splice-app`, a pure-Rust egui/eframe application
(tray icon + hidden-until-opened settings window) embedding the Splice engine as library crates.
No webview, no GTK, no C UI dependencies — this is deliberate: Tauri's Linux layer (GTK3 +
WebKitGTK + libappindicator tray) has open bugs on exactly our targets (tray never registers on
Plasma 6.7 Wayland, blank windows on GNOME Wayland). Engine crates are UI-free so a headless
daemon binary also exists (`splice-daemon`) for debugging/servers.

```
crates/
  splice-proto      wire protocol: types + framing (postcard, length-prefixed)
  splice-tailscale  Tailscale LocalAPI client: peer discovery, WhoIs auth, status watch
  splice-core       engine: peer sessions, source arbitration, focus FSM, layout/edge math,
                    held-key safety, clipboard broker, config persistence
  splice-platform   platform traits + macOS (CGEventTap/CGEventPost/NSPasteboard) and
                    Linux Wayland (ashpd portals + reis/libei, evdev monitor) backends
  splice-daemon     headless binary (engine + tracing, no UI)
  splice-app        egui/eframe app: tray (ksni on Linux, tray-icon[no default features] on
                    macOS), arrangement canvas window, engine embedded
docs/research/      verified platform research — READ THE RELEVANT FILE BEFORE IMPLEMENTING
```

## Core decisions (each one kills a known failure mode)

1. **Source-authoritative cursor.** The machine that currently has physical input (the *source*)
   tracks a virtual cursor position for the machine it is driving (the *target*) and decides all
   transitions (back to source, onward to a third machine). Getting the cursor back NEVER depends
   on the remote machine's capture stack working. (Kills: lan-mouse's stuck-cursor class.)
2. **TCP with TCP_NODELAY, one connection per peer pair.** Guaranteed delivery for
   Enter/Leave/key-up. Motion is sent immediately and coalesced only when capture events are
   already queued. (Kills: stuck keys from lost UDP key-ups.)
3. **Physical evdev scancodes on the wire, never characters, never layout groups.** Each machine
   applies its own keyboard layout. (Kills: Deskflow's 11-year AltGr bug class; lan-mouse's
   layout-group leak.)
4. **Forgiving liveness.** Heartbeat 1 s when a session is active, 5 s idle; 3 missed → peer
   *degraded* (release all input, keep trying) — never tear down sockets on a missed heartbeat.
   (Kills: lan-mouse's 2 s hard-window disconnects.)
5. **Held-input safety, both sides independently.** Both source and target track held keys/buttons.
   Release everything on: Leave, disconnect, degrade, capture loss, Secure Input start/end, panic
   chord. `Frame::ReleaseAll` exists as a belt-and-braces remote trigger.
6. **Local panic chord** (default `Left Shift+Right Shift+Escape`, configurable): unconditionally ends
   capture, re-associates the mouse, shows the cursor, sends Leave+ReleaseAll to all peers.
   Handled entirely locally in the capture backend — must work even if networking is wedged.
7. **Tailscale is discovery + identity + transport.** Host tailscaled (never embedded tsnet).
   Enumerate peers via LocalAPI, probe TCP port 41717 on online peers, authenticate inbound
   connections with WhoIs (same tailnet user, and NOT self), bind listener to the Tailscale IP
   only. No TLS on top (WireGuard already encrypts). No MagicDNS (broken on macOS MAS variant);
   dial `100.x` IPs directly.
8. **Wayland sessions are created once and never churned.** Persistent portal sessions with
   restore tokens; never call `Disable()` on an InputCapture session (mutter bug); barrier
   changes that require session recreation are batched and rate-limited. (Kills: lan-mouse's
   34-reconnects-per-2h fd leak.)
9. **macOS capture via pointer disassociation, not warp-back.**
   `CGAssociateMouseAndMouseCursorPosition(false)` + delta streaming; tap callback swallows
   events and does nothing else. Re-association is safety-critical: crash handler + atexit +
   watchdog. (Kills: warp suppression-interval jitter and frozen-cursor-on-crash.)
10. **Pass source-accelerated deltas through, applied exactly once.** Verified: compositors do
    NOT re-accelerate libei-injected motion (mutter and KWin pass deltas straight through), and
    macOS injection is absolute-position (no acceleration stage exists). So the wire carries the
    source's accelerated deltas in logical px (f64 — never i16), scaled by a per-link sensitivity
    factor (0.25–4.0). Do NOT send raw/unaccelerated deltas — the target would not accelerate
    them and the pointer would feel linear and slow. (Optional future: uinput target backend so
    libinput applies the local curve natively.)
11. **Edge crossing is a timed state machine** (Apple's design): enter edge zone → tiny dwell
    (default 0 ms — user wants effortless; keep the knob) → activate → transmit entry offset
    along the shared edge so the cursor lands exactly where it left. Per-corner dead zones to
    avoid hot-corner fights (default 16 px).
12. **Capability negotiation, never version gating.** `Hello` carries proto range + capability
    strings; peers agree on the intersection; nobody is turned away. (Synergy's 25-year lesson.)
13. **Keep the target awake**: injection declares user activity; while a session is entered the
    target takes a display-sleep inhibition (macOS IOPMAssertion; Linux D-Bus
    `org.freedesktop.ScreenSaver.Inhibit`).
14. **Clipboard is lazy, promise-based, capped.** On local clipboard change: broadcast
    `ClipOffer` with MIME list. Peers install *promised* clipboard items (macOS
    `NSPasteboardItemDataProvider`; Wayland portal `SetSelection` → `SelectionTransfer`).
    Data is fetched over TCP only when an app actually pastes. Size cap 16 MiB, chunked 256 KiB.
    Text ≤ 64 KiB is inlined in the offer (`ClipOffer.inline_text`) so cross-machine paste of
    small text works even if the origin goes offline.
    On Linux the Clipboard portal is attached to the **RemoteDesktop** session (works on GNOME
    46+ and KDE ≥6.4 today) — NOT the InputCapture session (that path needs GNOME 51+ and is
    "active-only"). macOS: poll `changeCount` at 500 ms; use `detectPatterns`/`detectMetadata`
    to gate actual content reads (future-proofs against the 15.4 pasteboard privacy alert,
    which exists but is not yet enforced by default). Images normalize to PNG on the wire
    (macOS pasteboards are TIFF-first — convert). MIME↔UTI map:
    `public.utf8-plain-text`↔`text/plain;charset=utf-8`, `public.html`↔`text/html`,
    `public.png`/`public.tiff`↔`image/png`, `public.rtf`↔`text/rtf`. File URLs are NOT synced
    in v1 (a `file://` path is meaningless on the other machine).
15. **Scroll normalization.** Capture normalizes to device-direction values; the target applies
    its own natural-scroll preference (prevents double inversion). Wire carries either
    `ScrollPixels{dx,dy}` (smooth/trackpad, logical px) or `ScrollLines` in value-120 units
    (one detent = 120). Never emit both for one physical event. Wayland targets: accumulate
    discrete to full ±120 before emitting (GNOME silently drops sub-120); use `std::trunc`
    semantics, not floor. `ScrollStop{cancel:false}` maps to phase-ended (lets target start
    kinetic scroll); `cancel:true` maps to hard stop. macOS trackpad momentum events are NOT
    forwarded — forward only finger-driven scroll + ScrollStop, let the target do momentum.
16. **Key repeat is generated at the destination, never forwarded.** Capture filters
    autorepeat (macOS `kCGKeyboardEventAutorepeat`, evdev value 2); wire carries only
    press/release edges. Wayland targets: apps auto-repeat held keys themselves (client-side
    repeat) — inject nothing extra. macOS targets: injected CGEvents do NOT auto-repeat —
    the macOS emulation backend synthesizes repeats (system delay/rate, autorepeat flag set)
    for the last held non-modifier key, cancelled on any release/Leave/ReleaseAll.

## Identity, discovery, connection topology

- `MachineId` = Tailscale stable node ID (`Node.StableID`). Display name = hostname.
- Poll LocalAPI `status` every 15 s (+ on demand); for each online peer of the same user, probe
  TCP 41717 (2 s timeout, bounded concurrency 4). A `Hello` exchange upgrades a probe to a session.
- Dedupe double-dials: the peer with the lexicographically **smaller** MachineId is the dialer;
  the larger only listens (both listen, but the larger side drops its own outbound dials to the
  smaller). Accept either side's connection if the rule's connection isn't up yet.
- Inbound accept: `WhoIs(remote ip:port)` must resolve to a tailnet node of the same user and a
  `StableID != self`. Otherwise close silently.
- Reconnect with exponential backoff 1 s→30 s while the peer is online per LocalAPI.

## Layout model and edge math (splice-core)

- Each machine reports its displays as rectangles in its own OS logical coordinate space
  (macOS: CG global coords; Linux: portal zones), plus per-display scale.
- The arrangement places each machine's coordinate space into a shared abstract canvas with an
  integer offset (`MachineLayout { offset, enabled }`). The UI drags whole machine cards.
- `LayoutDoc` is replicated state, last-writer-wins by `(lamport, writer MachineId)`. Any machine
  may edit; every edit bumps the lamport and broadcasts `LayoutSync`. On receive: adopt iff newer.
- Crossable edges are computed, not stored: for each pair of machines (both enabled), for each
  pair of display rects, find shared boundary segments where rect A's edge touches rect B's edge
  in canvas coords (within snap tolerance 2 px after UI snapping). Only *outer* edges of a
  machine's display union count. Result: `Vec<EdgeLink { from, to, axis, span, from_edge, to_edge }>`.
- The active source arms its capture backend with its own outgoing edge segments only.
  Wayland: barriers only on true outer edges (portal requirement — interior barriers are denied).
- Entry position mapping: offset along the shared edge span is preserved proportionally
  (spans are equal-length in canvas coords by construction; direct 1:1 along the overlap).

## Focus & source arbitration (splice-core FSM)

Per-machine engine state:

```
Idle          — not source, not target. Local input flows normally, capture armed on edges.
SourceLocal   — this machine is the source; cursor on its own screens. (== Idle + sourceness)
SourceRemote(target, virtual_pos, session_seq)
              — source; cursor is on `target`. Local capture active (events swallowed +
                forwarded). Source tracks virtual_pos against target's display union,
                detects target-edge crossings for onward/backward transitions.
TargetActive(source, session_seq)
              — being driven; injecting received input. Shows cursor at Enter pos.
```

- **Sourceness** is a cluster-wide lamport-clocked claim (`SourceClaim{seq}`), broadcast when a
  machine sees *physical* local input while not already source. Everyone adopts the highest
  claim; the previous source, if in SourceRemote, ends capture (Leave to its target) silently.
- Injected input must never trigger a SourceClaim: macOS tags injected events via event-source
  user-data magic; Linux distinguishes evdev physical devices from portal-injected input
  (portal injection doesn't appear on /dev/input); the evdev monitor IS the physical signal.
- Session seq increments per Enter; a target discards `Input` frames whose seq != current session
  (stale after Leave).
- Enter carries absolute position in the *target's* local coords (source converts via layout).
- Motion accumulates into virtual_pos clamped to the target's display union (dead-zone aware:
  clamp to nearest point inside any display rect).

## Wire protocol (splice-proto)

Length-prefixed (u32 BE, max 1 MiB) postcard-encoded `Frame`. First frame each direction must be
`Hello`/`Welcome`. Protocol version 1. See `crates/splice-proto/src/lib.rs` for the authoritative
types — that file is the contract; extend by *adding* enum variants/fields (postcard is not
self-describing: never reorder or remove existing variants/fields; unknown-variant tolerance is
handled by the version/caps negotiation, so gate new frames on negotiated caps).

## Platform backends (splice-platform)

Trait contracts live in `splice-platform/src/lib.rs` (authoritative). Implementations:

- `macos/` — see `docs/research/macos-input.md` (SDK-verified). CGEventTap session-level active
  tap; disable-event handling (timeout → re-enable; user-input → recreate); sleep/wake/session
  notifications → recreate; health poll 5 s; Secure Input detection with culprit PID surfaced;
  injection via CGEventPost(HID) with explicit flagsChanged, click-state, scroll phases
  (pixel + momentum mapping to ScrollStop); NSPasteboard changeCount poll 300 ms + promised items;
  physical-vs-injected via CGEventSourceUserData magic; IOPMAssertion while entered;
  display reconfiguration callback → republish displays.
- `wayland/` — see `docs/research/wayland-input.md`. ashpd `InputCapture` (capture) +
  `RemoteDesktop` (inject) sessions with persist_mode=2 + restore tokens (store in config dir);
  reis receiver/sender contexts; barriers from core-provided edges; keymap from `ei_keyboard`
  used verbatim (evdev codes pass through); scroll smooth + scroll_stop; clipboard portal
  attached to the RemoteDesktop session (SetSelection/SelectionTransfer/SelectionRead);
  evdev read-only monitor for physical-activity detection (graceful degrade if no input-group
  permission — log + UI warning); D-Bus ScreenSaver inhibit while entered.
- Keycode translation: macOS virtual keycodes ↔ evdev codes via the `keycode` crate
  (Chromium-derived tables, MIT; what lan-mouse ships). Wrap it in `keymap.rs` with our own
  tests over the full modifier/nav/function set. Known quirks to handle: arrow keys on macOS
  must carry `CGEventFlagNumericPad|SecondaryFn` flags when injected; Caps Lock lock-state
  cannot be toggled via CGEventPost (needs IOKit `IOHIDSetModifierLockState`) — v1 policy:
  do not forward CapsLock as a toggle, forward lock *state* only.

## UI (crates/splice-app)

egui/eframe (latest stable), single binary embedding the engine. Design language: clean,
dark/light-aware (follow system), restrained color, generous spacing — this must NOT look like
a default egui demo: custom `egui::Style` (rounded corners, subtle shadows, accent color
`#5B8DEF`, neutral grays), no window decorations chrome beyond the platform default.

- **Arrangement canvas** (central panel): machine cards = actual display rects to scale,
  draggable as a unit (egui drag interactions), edge snapping with 8 px magnetism at ~1/12
  scale; this machine highlighted with accent border; offline peers ghosted at 40% opacity;
  disabled peers desaturated with an enable toggle on the card. Shared edges render as green
  (crossable) / red (touching but blocked) strips per computed EdgeLinks.
- Card content: hostname, OS glyph, connection badge (● direct / ◐ DERP with warning tint /
  ○ offline), RTT ms, "SOURCE" chip when that machine holds sourceness.
- **Side panel**: per-link sensitivity slider (0.25–4.0 log, default 1.0), clipboard sync
  toggle, panic chord display, permissions/health list (macOS: Accessibility state, Secure
  Input warning naming the culprit process; Linux: portal session state, restore-token state,
  evdev monitor state) with fix-it hints.
- **Header bar**: master enable switch, "Disconnect all" button (= local panic + broadcast).
- **Tray**: status icon (idle/active/degraded), menu: per-machine enable toggles, Disconnect
  all, Open Splice, Quit. Linux: `ksni` (pure-Rust StatusNotifierItem — native on KDE, works
  via the AppIndicator extension on GNOME; probe `org.kde.StatusNotifierWatcher` NameHasOwner
  and surface a one-time hint if absent). macOS: `tray-icon` with `default-features = false`
  + `ActivationPolicy::Accessory` (menu-bar only, no Dock icon).

Engine↔UI: engine publishes a `UiState` snapshot via `tokio::sync::watch`; the eframe app
repaints on change (coalesced ≤10 Hz) and calls engine command methods directly (same process).
Window is created on demand (tray → Open); app keeps running with window closed.

## Config & persistence

`directories` crate → config dir (`~/.config/splice` / `~/Library/Application Support/splice`).
`config.json` (settings + layout doc + per-link sensitivity), `tokens.json` (portal restore
tokens), atomic writes (tmp + rename). Machine-local; layout replicates via LayoutSync.

## Packaging

- macOS: `.app` bundle (script `packaging/macos/make-app.sh` — bundle skeleton + Info.plist with
  `LSUIElement=true` and `NSLocalNetworkUsageDescription`, copy binary, codesign). Signing: a
  stable self-signed "Splice Dev" certificate for dev builds (`packaging/macos/make-cert.sh`),
  Developer ID for release. NEVER plain ad-hoc — TCC keys grants to the cert, and ad-hoc =
  cdhash = silent permission loss on every rebuild (System Settings toggle still shows ON).
  Note: `NSAccessibilityUsageDescription` / `NSInputMonitoringUsageDescription` do NOT exist —
  don't cargo-cult them. Accessibility alone transitively grants listen+post on macOS 13+;
  onboard users to exactly one toggle.
- Linux: single binary + `packaging/linux/`: `splice.desktop`, systemd user unit named
  `app-splice.service` (portal-compatible cgroup name), udev rule + group doc for the evdev
  monitor (`docs/linux-setup.md`). RPM later.

## Testing

- Unit tests: proto roundtrips, layout/edge math (property-ish cases incl. non-rectangular
  arrangements), FSM transitions (source claims, stale seq, degrade → release), held-key ledger.
- `splice-core` must be fully testable without platform backends (trait objects + a `MockPlatform`).
- Integration smoke: two in-process engines wired over localhost TCP with mock platforms,
  asserting an end-to-end enter → input → leave → clipboard flow.
