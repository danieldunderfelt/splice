# Linux non-portal input backends (verified against compositor sources, 2026-09)

Companion to `wayland-input.md`, which covers the portal path. This file records what the
uinput injection backend, the layer-shell overlay capture backend and the data-control
clipboard backend rely on, and where each compositor deviates. Splice's Linux backend
supervisor (`crates/splice-platform/src/linux/backends.rs`) resolves `Auto` preferences
with these facts.

## Why not EVIOCGRAB for capture

Grabbing `/dev/input/event*` swallows input compositor-independently, but nothing on
Wayland reports the cursor position to a background client, so a grab-only backend cannot
detect edge crossings; the tools that use it (rkvm) switch machines with a hotkey instead.
Edge detection needs the compositor: portal barriers (InputCapture) or layer-shell strips.
Once an edge fires, the compositor also delivers the deltas (EIS or relative-pointer), so
a grab buys nothing for capture. It stays out of Splice.

## Injection: uinput absolute pointer + keyboard (`uinput.rs`)

Design: one persistent virtual pointer with `ABS_X`/`ABS_Y` 0..65535 (no
`INPUT_PROP_DIRECT`, no `REL_X/REL_Y`, no `BTN_TOUCH`/`BTN_TOOL_*`, resolution 0, fuzz 0),
`BTN_LEFT..BTN_TASK`, `REL_WHEEL`/`REL_HWHEEL` plus their `_HI_RES` variants; one virtual
keyboard with `KEY_ESC..KEY_UNKNOWN` (1..240) and nothing else.

- udev `input_id` tags the pointer `ID_INPUT_MOUSE` through the explicit "VMware absolute
  mouse" branch (ABS_X/Y + mouse button, no touch/pen/direct). `INPUT_PROP_POINTER` is
  never read. The keyboard needs key bits 1..31 all set for `ID_INPUT_KEYBOARD`.
- libinput classifies it as a plain pointer and emits `POINTER_MOTION_ABSOLUTE`.
  **No acceleration is applied to absolute motion** (`filter_dispatch` only runs in the
  relative path), so the wire's source-accelerated deltas are applied exactly once and
  the target position stays identical to the source's virtual cursor. This is why the
  device is absolute rather than relative: a relative uinput device would be
  re-accelerated by the target's libinput, and the source-authoritative cursor would drift.
- Mapping: `x_logical = v * layout_width / 65536` where the layout is the bounding box of
  all outputs. Verified whole-layout mapping in mutter (`meta_viewport_info_get_extents`),
  KWin (`workspace()->geometry()`), wlroots/sway (`wlr_output_layout_get_box(NULL)` unless
  `map_to_output`), Hyprland (`CPointerManager::warpAbsolute` min/max over monitors),
  niri, Xorg/xf86-input-libinput (RandR screen). **cosmic-comp maps absolute pointers to
  the seat's active output only** and **gamescope embedded mode passes millimetres
  through unscaled** — multi-monitor targets on COSMIC and Steam Deck gaming mode are not
  correct with this backend.
- Resolution must stay 0: Hyprland treats an absolute device that reports a physical size
  as a touchpad and applies touchpad config to it.
- Wheel: emit `REL_WHEEL ±1` and `REL_WHEEL_HI_RES ±120` in the same frame, only whole
  detents. libinput 1.25–1.28 swallows hi-res deltas below 60 at the start of a scroll;
  libinput ≥ 1.29 bypasses its wheel plugin entirely for virtual devices. Smooth
  (`ScrollPixels`) input accumulates at 120/15 units per logical px, libinput's click
  angle. Vertical sign is inverted (kernel: positive = away from the user), horizontal is
  not.
- Kernel evdev buffers nothing for clients that have not opened the node: events written
  before udev tagged the device and the compositor opened it are lost. The devices are
  created once at backend start and the backend waits for `/run/udev/data/c13:N` before
  reporting ready.
- Injected events appear on `/dev/input`, so the physical-activity monitor skips devices
  named `Splice Virtual *`; otherwise every injected event would claim sourceness.
- The kernel filters an ABS value equal to the previous one (and the SYN with it), so
  placing the pointer at the last written cell would be a no-op: `enter` takes a one-unit
  detour when needed. Only whole detents are emitted for the wheel, from an f64
  accumulator so slow smooth scrolls are not rounded away.
- `/dev/uinput` is a kmod static node: until the module is loaded there is no sysfs
  device for udev to tag, so the packages ship `modules-load.d/splice.conf` and the
  install hooks run `modprobe uinput` before triggering.
- The target's global mouse settings apply to the virtual device: natural scrolling
  inverts the wheel, left-handed mode swaps buttons. There is no per-device opt-out from
  outside the compositor; documented in linux-setup.md.
- uinput works at the lock screen, so a remote machine can unlock this one. The portal
  path refuses injection while locked; the uinput path keeps only the keep-awake
  inhibitor.
- mutter still runs InputCapture barriers against absolute motion, KWin explicitly
  exempts absolute motion from edge barriers; either way the engine's "a target never
  turns a barrier hit into a crossing" invariant handles it.

## Capture: layer-shell overlay (`overlay.rs`)

Design (lan-mouse's field-tested shape, with the gaps closed): a 2 px transparent
`zwlr_layer_shell_v1` overlay strip per armed edge segment, anchored to the display that
owns the segment, `exclusive_zone = -1`, real ARGB shm buffer of the strip size (a
transparent buffer still receives input; the input region defaults to the surface). On
`wl_pointer.enter` the strip is only *hovered*; the crossing fires on the first
`zwp_relative_pointer_v1` motion whose component points outward through the edge, which
is the barrier semantic and avoids capturing a pointer that merely slides along the edge.

Grab order matters: `set_cursor(enter_serial, NULL)` → `keyboard_interactivity =
exclusive` + commit → `lock_pointer(persistent)` → optional
`zwp_keyboard_shortcuts_inhibit`. KWin, sway and COSMIC only activate a lock on a surface
that already holds keyboard focus.

- `set_cursor_position_hint` is not a portable warp: KWin and cosmic-comp reject hints
  outside the (2 px) surface, sway ignores it for layer surfaces, only niri (clamped to
  the output) and Hyprland accept an off-strip position. The backend clamps the hint into
  the strip and keeps the along-edge coordinate; the strip stays disarmed until the
  pointer leaves it or 750 ms pass.
- 1 px strips are unreachable on fractionally scaled outputs on Hyprland (lan-mouse #447,
  #454); 2 px is the workaround lan-mouse shipped for Hyprland #6170.
- Hyprland `main` (after v0.50.x) only enforces pointer constraints on surfaces backed by
  a `CWindow`; layer surfaces receive `locked` but the cursor keeps moving. The backend
  abandons a lock that is not activated within 500 ms and reports it in the health panel.
- Sway once regressed on toggling exclusive keyboard interactivity (sway #7936); labwc
  lacks shortcuts-inhibit and had broken pointer constraints (lan-mouse #187); Wayfire
  needs the `shortcuts-inhibit` plugin enabled. mutter has no layer-shell at all.
- `relative_motion` `dx/dy` are accelerated logical px on wlroots and KWin (KWin scales
  only the accelerated pair to surface-local units); the unaccelerated pair is raw device
  units and is not used.
- `wl_keyboard.key` carries raw evdev codes (the +8 lives only in XKB), the compositor
  never repeats, and `wl_keyboard.enter` lists the keys already held (seeded into the
  panic-chord tracker).
- Injected motion from a remote source reaches our own strips too. The supervisor exposes
  the driven state (plus a 1 s grace after leave) and the overlay checks it at the moment
  outward motion would lock, on top of the engine's own grace.
- Pointer focus is tracked separately from the capture phase: after a release the pointer
  is still on the strip (hint clamped into it), so re-arming cannot wait for a new
  `wl_pointer.enter`. The cursor image is restored explicitly on release (the NULL cursor
  set for the lock persists while focus stays on the strip). A `wl_keyboard.leave` or a
  `wl_pointer.leave` while locked means the compositor took the grab away: the capture is
  reported broken so the engine releases everything on the peer.
- With the v7 `wl_pointer` (sctk 0.19) a hi-res wheel delivers a smooth `axis` for every
  sub-step and `axis_discrete` once per detent; wheel-sourced frames are forwarded as whole
  detents only, or the target would scroll roughly twice as far.

## Clipboard: data-control (`datacontrol.rs`)

`ext-data-control-v1` and `zwlr-data-control-unstable-v1` are the same protocol renamed;
one state machine drives both, ext first (KWin dropped wlr when it ported to ext), wlr
as the fallback for older wlroots. Available on KWin, wlroots family, COSMIC, niri;
absent on mutter (declined on privacy grounds) and gamescope.

- The first `selection` event arrives on binding the device (current clipboard for free).
- The previous offer must be destroyed on every `selection`, or objects leak per change.
- `receive(mime, fd)`: drop the local write end immediately or EOF never arrives. Reads
  are capped at `CLIP_MAX_TOTAL` and time out.
- `send(mime, fd)` may arrive with `O_NONBLOCK`; the fd is served on a thread with poll
  timeouts, `EPIPE` counts as success (the consumer read what it needed). The transfer
  never runs inside the dispatch loop.
- There is no `session_is_owner` flag: our own `set_selection` echoes back as one
  `selection` event, recognised by the ownership flag on our source; `cancelled` clears it.
- `offer()` after `set_selection` and reusing a source are protocol errors: one source per
  offer, all mimes declared first. Text is advertised with the legacy aliases (`text/plain`,
  `TEXT`, `STRING`, `UTF8_STRING`) like wl-copy does.
- Primary selection is never touched (mouse-selection chatter, v2-only on wlr).
- Unverified: behaviour under `ext-session-lock`, and whether Flatpak's Wayland proxy
  filters the privileged global (the manifest requests the plain socket).

## Supervisor rules (`backends.rs`)

- `Auto` capture → overlay where all three globals exist, else portal. `Auto` inject →
  uinput when `/dev/uinput` opens read-write, else portal. Clipboard → data-control when
  present, else the Clipboard portal on a RemoteDesktop session.
- The RemoteDesktop session exists iff injection or clipboard uses it, so on GNOME the
  uinput injection choice still keeps the (token-persisted) session alive for the
  clipboard, and on KDE/wlroots nothing portal-related is created at all.
- Swaps replay state: armed edges into a new capture backend, the entered position into
  a new injection backend, the pending remote offer into a new clipboard backend.
