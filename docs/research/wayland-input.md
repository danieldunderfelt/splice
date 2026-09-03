# Linux/Wayland input platform facts (verified against portal specs, libei 1.6, Fedora 44, 2026-08)

Authoritative reference for `splice-platform/src/wayland/`. Target: Fedora 44+ — GNOME 50
(mutter 50.4, xdg-desktop-portal 1.22.1, xdg-desktop-portal-gnome 50) and KDE Plasma 6.7
(xdg-desktop-portal-kde 6.7.4). libei 1.6. Rust: `ashpd` (portals, tokio feature) + `reis`
(pure-Rust libei, tokio feature) + `zbus`.

## Capture: InputCapture portal + libei receiver

Session flow (ashpd `desktop::input_capture` module has a complete worked example — read it):

1. `CreateSession2` (v2; `CreateSession` is deprecated). Capabilities are requested at `Start`.
2. `Start(parent_window, capabilities=KEYBOARD|POINTER, restore_token, persist_mode=2)`.
   This shows the consent dialog. Response carries granted capabilities and a NEW
   `restore_token` — tokens are SINGLE-USE; persist the new one after every successful
   start/restore (tokens.json). GNOME 50's backend may ignore restore tokens (persistence
   landed in xdg-desktop-portal-gnome 51) → user may be re-prompted once per app launch on
   GNOME until Fedora 45; KDE 6.7 honors them. Design for one long-lived session per process
   lifetime to make prompts rare.
3. `ConnectToEIS` → fd → reis **receiver** context (`ei_handshake.context_type=RECEIVER`).
   Call once per session, before Enable; survives Enable/Disable cycles.
4. `GetZones` → zones `a(uuii)` = (width, height, x_offset, y_offset) in compositor logical
   coords + `zone_set` generation id. Zones may be discontinuous. Empty zones ⇒ no barriers
   possible. Publish zones to core as this machine's display rects.
5. `SetPointerBarriers(barriers, zone_set)` → `failed_barriers`. Geometry rules (get these
   exactly right or barriers fail):
   - Horizontal barriers: y1 == y2; vertical: x1 == x2. No diagonals.
   - Must lie on the OUTSIDE boundary of the union of all zones AND fully within one zone.
   - Coordinates are INCLUSIVE pixels: vertical barrier on left edge of a 1920x1080 zone at
     (0,0) is (0,0,0,1079). Right edge of a zone at (1920,0): x = 1920+1920 = 3840 →
     (3840,0,3840,1079). Left/top edges use origin; right/bottom use origin+extent (not −1).
   - Barriers between two adjacent local monitors are DENIED (interior). Only true outer edges.
   - Setting barriers suspends the session → must call `Enable()` after.
   - Zero-length barrier array clears all.
6. `Enable()` arms capture. **Capture activates ONLY when the compositor decides** (pointer
   hits a barrier) — there is no way to force immediate capture on Wayland. Edge-crossing is
   the only trigger; the whole product design assumes this.

Signals:
- `Activated{activation_id, cursor_position, barrier_id}`: capture began. `cursor_position` is
  usually OUTSIDE the zones (fast flicks overshoot dozens of px past the barrier) — clamp and
  compute entry offset yourself. `barrier_id` may be 0/absent on KDE (known bug): fall back to
  nearest-barrier-by-perpendicular-distance to the reported position (lan-mouse's workaround).
  The `activation_id` equals the `sequence` in the EIS `start_emulating` event before the
  first device event — use it to correlate the D-Bus and EIS streams (independently ordered).
- `Deactivated{activation_id}`: compositor ended capture (e.g. user switched VT). Treat as
  session end: Leave + release-all.
- `ZonesChanged{zone_set}`: monitor hotplug/rearrange. Recreate the session, republish displays,
  and re-arm the new zone set. Rate-limit; GNOME may also kill sessions on device add/remove —
  handle `EI_EVENT_DISCONNECT` by full session re-establishment (backoff 1 s, max 1 recreation
  per 5 s — never churn like lan-mouse's 34-reconnects bug).
- To hand control back: `Release{activation_id, cursor_position}` — position is a suggestion
  for where the local cursor reappears (compositor may ignore; usually honored). ALWAYS call
  Release on Leave-back; if the EIS connection drops without Release the cursor is stranded.

**NEVER call `Disable()` on a live session** — mutter bug #3908: after Disable, events stop
flowing to the EIS socket permanently while Activated still fires. GNOME 50 also rejects
SetPointerBarriers while enabled even though the portal specification says that call suspends
the session. GNOME 50's backend is InputCapture v1 (impl version 0): no restore tokens, and
every CreateSession shows the consent dialog, so recreating the session per peer change means
a prompt per peer change. Instead, arm the ENTIRE outer boundary of the zone union once per
session (compute segments from GetZones, subtracting flush neighbours) and resolve `Activated`
against the engine's edge map in software; a hit on an unmapped stretch is Released at once.
Peer/layout changes then never touch the portal. Only ZonesChanged recreates the session.

**Do not rely on `ei_keyboard.modifiers` events on GNOME** (mutter #3375: never sent to ei
clients). Track modifier state from raw key up/down + the keymap. Do this on all compositors.

**The cursor is NOT hidden on this machine while captured** (no API for it). Known cosmetic
wart; do not fight it.

libei receiver specifics (reis):
- Events arrive per-device with `frame` grouping (one frame = one logical hardware event,
  timestamp µs CLOCK_MONOTONIC).
- `ei_device.paused` ⇒ assume ALL modifiers/keys lifted (protocol-defined reset). Emit
  release-all to the active target.
- Pointer: `motion_relative(x,y)` floats, logical px, post-acceleration (no unaccelerated
  variant exists in the protocol). Regions carry a `scale` — apply to relative motion when
  crossing mixed-DPI regions.
- Keyboard: `key(code, state)` with RAW EVDEV codes (linux/input-event-codes.h; KEY_A=30).
  NOTE the +8 offset only exists in the XKB layer — never add 8 to wire codes.
- Scroll: `scroll(x,y)` smooth logical px; `scroll_discrete(x,y)` in value-120 (one detent
  = 120); `scroll_stop(x,y,is_cancel)`. A source sends smooth OR discrete for one event,
  never both.

## Injection: RemoteDesktop portal + libei sender

1. `CreateSession` → `SelectDevices(KEYBOARD|POINTER, restore_token, persist_mode=2)` →
   `Start` (consent dialog; token persistence works on GNOME 46+/KDE — store new token).
2. `ConnectToEIS` → fd → reis **sender** context. Once per session, after Start. After EIS
   is connected the `Notify*` D-Bus methods are forbidden — use libei for everything.
3. Create/receive devices: expect a keyboard (with an xkb keymap fd — mmap MAP_PRIVATE) and
   pointer devices. Separate device capabilities for relative pointer, absolute pointer,
   scroll. A newly advertised device is PAUSED: wait for `ei_device.resumed` before sending
   `start_emulating`, input, or `frame`. After `ei_device.paused`, stop sending until the next
   resume and begin emulating again if the logical remote session is still entered. Violating
   this state machine makes GNOME disconnect EIS with a protocol error.
4. `start_emulating(sequence)` when a remote session enters (or its devices later resume);
   `stop_emulating` on Leave, only for devices that are currently emulating. Sequence increases
   monotonically across sessions. Never emit an empty cleanup frame for a paused device.
   Absolute motion targets a device REGION; coordinates outside regions are silently discarded
   (use for Enter positioning). Relative motion is used for everything else.
5. Keyboard injection: send RAW EVDEV codes. The compositor applies its own layout — exactly
   what we want (scancodes on the wire). We do NOT reverse-map keysyms in v1 (machines'
   layouts assumed compatible; documented limitation). One key event per key per frame;
   press+release of same key in one frame is a violation. Release all held keys before
   `stop_emulating`.
6. Frames: every batch of events needs `frame()`, followed by an immediate connection flush.
   Reis buffers requests until `Connection::flush`; polling the receive stream does not flush
   ordinary requests. Motion is sent immediately when the queue is empty; consecutive queued
   motion commands are summed into one motion + frame so an EIS scheduling pause cannot create
   a stale playback backlog.
7. Scroll: smooth px → `scroll()`; ScrollLines → accumulate to full ±120 then
   `scroll_discrete()` (GNOME silently drops sub-120 remainders; use trunc not floor);
   ScrollStop → `scroll_stop(cancel)`.
8. Text fallback (libei 1.6 `ei_text`, cap TEXT=1<<6): available for future use; not v1.

Keep-awake while entered: D-Bus `org.freedesktop.ScreenSaver` `Inhibit("splice",
"Remote input active")` / `UnInhibit` on leave.

## Physical-activity monitor (source arbitration)

Portal-injected input does NOT appear on /dev/input — so evdev devices ARE the physical
signal. Open all current /dev/input/event* read-only via the `evdev` crate (+inotify on
/dev/input for hotplug); on any EV_KEY/EV_REL event → publish physical-activity to core
(debounced 50 ms). Requires the user in the `input` group (udev rule documented in
docs/linux-setup.md). If permission denied: degrade gracefully — log once, surface in UI
("source auto-switching limited on this machine"), fall back to: capture activation and
locally-observed input imply sourceness.

## Clipboard (portal, attached to the RemoteDesktop session)

- `RequestClipboard(session)` MUST be called before `Start()`. Grant reported as
  `clipboard_enabled` in Start's response. Works on GNOME 46+ and KDE ≥6.4 today. (The
  InputCapture-session clipboard needs GNOME 51 — do not use it.)
- If clipboard was NOT granted but we have a restore token, discard the token and create a
  fresh session (Deskflow's trick) so the re-prompt includes clipboard.
- Own: `SetSelection(session, {mime_types})`; then `SelectionTransfer{mime, serial}` signal →
  `SelectionWrite(serial)` → write fd → `SelectionWriteDone(serial, success)`. Answer
  unservable requests with success=false.
- Observe: `SelectionOwnerChanged{mime_types, session_is_owner}` (real change signal — no
  polling). Ignore when `session_is_owner` (that's us — loop guard).
- Read: `SelectionRead(session, mime)` → fd.
- MIME priority: `image/png`, `text/plain;charset=utf-8`, `text/plain`, `text/html`.

## Displays / zones

Use `zxdg_output_v1` logical position and size for pre-consent discovery and display hotplug.
This lets peers target the machine while an InputCapture prompt is open. Portal `GetZones`
remains the source of truth for release clamping and barrier-space validation once the session
exists. Do not use legacy `wl_output.geometry` as logical geometry; fractional scale is already
applied by xdg-output and portal logical coordinates.

## Gotchas summary

- Restore tokens single-use; persist replacements immediately (atomic write).
- KDE shows a persistent "Input Capture" notification — expected, not a bug.
- Screen lock: RemoteDesktop injection may error while locked ("failed to initialize remote
  desktop session" churn) — back off while `org.freedesktop.ScreenSaver.GetActive` is true.
- Suspend/resume invalidates sessions: listen for EIS disconnect + portal Session closed
  signals; re-establish with backoff; never crash (lan-mouse panics here — we must not).
- Mixing X11 endpoints is out of scope: Wayland only.
- journalctl debugging: `journalctl --user -xeu xdg-desktop-portal-gnome.service` (or -kde).
