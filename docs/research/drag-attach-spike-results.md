# Continuous cross-edge file drag: Phase 0 spike results

Date: 2026-09-11. Throwaway code lives in `spikes/` and is not part of the Cargo workspace.

Each spike answers one yes/no question needed before building the continuous held-button
file drag (source edge captures the dragged files into an offer; destination re-attaches
the offer to the cursor as a native drag). Design context is in
[file-transfer-and-drag-drop-plan.md](file-transfer-and-drag-drop-plan.md).

## KDE Plasma 6.7 / KWin Wayland (gamedev): YES

Question: can a Wayland client turn an injected pointer button press into a native
drag-and-drop that lands in another application, while injected motion drives it?

Method: `spikes/wayland-drag-attach`. A uinput virtual pointer (same device shape as
Splice's injector: ABS_X/ABS_Y 0..65535 plus BTN_LEFT..BTN_TASK) moves the pointer onto a
200x200 origin surface owned by the spike, presses BTN_LEFT, and the spike calls
`wl_data_device.start_drag` with the press serial and a `text/uri-list` data source.
Immediately after `start_drag` the origin surface's input region is set to empty so the
compositor's drag target picking falls through to the windows beneath. The injector then
moves the pointer to the target and releases.

| Run | Origin surface | Target | Result |
|---|---|---|---|
| 1 | `zwlr_layer_shell_v1` Overlay, 200x200 at the entry point | Fullscreen GTK4 `DropTarget(FileList)` | Drop received, both files read back with matching SHA-256 |
| 2 | Fullscreen `xdg_toplevel` (the shape GNOME would need; no layer-shell) | Same GTK4 target | Same result |
| 3 | Layer-shell as in run 1 | Real Dolphin window (fullscreen via KWin script) | Dolphin showed its standard drop menu (Copy Here / Link Here / Move Into New Folder / Cancel); an injected click on Copy Here copied both files, SHA-256 match |

Observed mechanics that the real implementation must reproduce:

- The injected press produced a normal `wl_pointer.button` with a usable serial on the
  spike's own surface; KWin accepted `start_drag` with it. No special grab handling needed.
- After `start_drag`, `wl_pointer.leave` arrives for the origin surface and the drag grab
  takes over. Emptying the input region on the origin surface is what lets the drop reach
  the application beneath. With the region left intact the origin would be the drop target.
- Injected absolute motion drives the drag; the destination's `accept`/`action` traffic
  tracks it at the injection rate.
- Dolphin reads `text/uri-list` during hover and again at drop, and always shows its drop
  menu for an external drag that only offers Copy. That is Dolphin's normal behaviour for a
  drag from any other application; the user clicks Copy Here (or holds Ctrl at release).
- The running Splice service (overlay capture armed on the left edge) saw none of this:
  no edge hit, no capture, no physical-activity claim, because injection stayed away from
  armed edges.

Implication for Splice's destination half on Linux: at crossing, create the origin surface
under the entry point (layer-shell where available, fullscreen toplevel otherwise), let the
carried held-button press land on it, `start_drag` with a `text/uri-list` source that points
at the FUSE view already used by the shelf, empty the input region, and keep injecting the
forwarded motion. The carried release drops. This reuses the existing FUSE view, the deferred
read gate, and the transfer service unchanged.

## GNOME Shell 50.4 / Mutter 50.4 (this machine, Fedora 44, Wayland): YES, with one change

Run 2026-09-11 on the GNOME machine (3840x2160, scale 1) with the same binary in `--mode toplevel`
and the same uinput device shape. Nautilus 50.2.2, GTK 4.22. No Splice service was running.

| Run | Origin handling after `start_drag` | Target | Result |
|---|---|---|---|
| 4 | Input region emptied (the KWin recipe) | Fullscreen GTK4 `DropTarget(FileList)` | Drag left the origin and never entered anything else; every `target` was null; release cancelled the source |
| 5 | Input region kept (control) | Same | Drag stayed on the origin, drop landed on the origin; Mutter's drag machinery accepts the injected press serial |
| 6 | Origin toplevel unmapped (null buffer attach + commit) | Same | Drop received, both files read back with matching SHA-256 |
| 7 | Input region emptied | Maximized GTK4 target instead of fullscreen | Same failure as run 4 |
| 8 | Origin unmapped | Real Nautilus window, maximized, on an empty folder | Nautilus copied both files into the folder, SHA-256 match, no drop menu |

Observed mechanics on Mutter:

- Mutter delivers the injected `wl_pointer.button` with a serial it honours in `start_drag` (a).
- Emptying the origin's input region does not re-route drag targeting on Mutter (b fails). The
  fallback from the previous section works: unmap the origin right after `start_drag`. Mutter only
  ends a drag when the origin *resource is destroyed* (`destroy_data_device_origin` in
  `meta-wayland-data-device.c`); an unmapped-but-alive surface keeps the grab, and the next injected
  motion repicks the window beneath. Keep the `wl_surface` alive until `dnd_finished`/`cancelled`.
- The fullscreen origin takes activation while mapped. The window beneath got `is-active` back when
  the drag ended, not at unmap time, so a transparent origin costs a brief focus change and nothing
  visible.
- Nautilus reads `text/uri-list` during hover and copies on release with the Copy action. No menu.

Source half on GNOME (no layer-shell strip to receive the drag at the edge): run 9 used
`--mode catch` against `source.py`, a fullscreen GTK4 `DragSource` offering a file list. The
injector pressed on the GTK source and moved past the drag threshold. The spike then created and
committed a fullscreen toplevel *during* the foreign drag. Mutter delivered `wl_data_device.enter`
to the new surface within 90 ms of the commit (offer types: `application/x-gtk-local-dnd`,
`text/plain;charset=utf-8`, `text/uri-list`, `application/vnd.portal.filetransfer`,
`application/vnd.portal.files`). The spike accepted `text/uri-list`, the injected release produced
`drop`, `receive` returned both URIs, and the GTK source saw `drag-end` with the Copy action.

Implication for GNOME: the same continuous design works with two substitutions. Destination side,
the origin is a fully transparent fullscreen toplevel that is unmapped after `start_drag` instead
of having its input region emptied. Source side, when a held-button crossing is detected with a
native drag in flight, mapping a transparent fullscreen toplevel under the cursor is enough to
become the drop target and read the file list, so GNOME does not need an edge strip to capture
the selection. What is still unverified on GNOME is the interaction with the InputCapture portal:
whether a portal barrier fires during a native drag and whether the drag survives the capture
activation/release around it. That needs the real Splice capture path, not this spike.

```
cd spikes/wayland-drag-attach && cargo build
python3 target.py &            # fullscreen GTK4 drop target
./target/debug/wayland-drag-attach --mode toplevel --unmap --screen 3840 2160 --origin 1500 600 --target 1500 1500 /tmp/a /tmp/b
python3 source.py /tmp/a /tmp/b &   # fullscreen GTK4 drag source
./target/debug/wayland-drag-attach --mode catch --screen 3840 2160 --origin 1500 600 --target 1500 1500 /tmp/a /tmp/b
```

### Carried into Splice the same day

`crates/splice-platform/src/linux/dragattach.rs` implements both halves as library calls.
`attach` maps the origin at the entry point (layer-shell where the global exists, fullscreen
toplevel otherwise), sends `start_drag` on the carried press, then empties the input region or
unmaps the toplevel. `catch` maps a fullscreen catcher that accepts `text/uri-list`, reads it
and the portal key at the drop and finishes the offer. `splice-files --pilot-attach <dir> <x> <y>`
and `splice-files --pilot-catch <dir> <x> <y>` drive them against a real deferred FUSE view.
Results on this GNOME machine, Nautilus maximized behind the origin, input from uinput:

| Pilot | Result |
|---|---|
| `--pilot-attach` at 1500,600; press, glide, release over Nautilus | `Armed(FullscreenToplevel)`, `Started`, `Dropped` (gate opened), `Finished`; one commit on Nautilus's first read; both files copied, SHA-256 match; the two pre-drop reads were denied |
| `--pilot-catch` mapped 3 s into a GTK drag from `source.py` | `Entered` 200 ms after mapping, `Dropped` with both URIs and GTK's portal key; the source ended with Copy |

Two things the spike hid. Mutter's first `xdg_toplevel.configure` carries no size, so the origin
sizes itself to the chosen output or it maps as 200x200 nowhere near the pointer. GTK's portal
key arrives NUL-terminated. The module handles both.

Not wired yet, and not GNOME-specific: the engine replays the held button right after
`Frame::Enter`, so the destination must start `attach` and wait for `Armed` before that replay;
the crossing must carry the offer id; and the source side must start `catch` (or hand the KDE
strip the drag) before capture activates. The InputCapture portal's behaviour during a native
drag is the one remaining GNOME unknown.

## macOS: NOT RUN

`spikes/macos-drag-attach/main.swift` is ready. It shows a borderless accepts-first-mouse
panel at the origin point, posts HID-level `CGEvent`s (the same path Splice's injector uses),
and on `mouseDown:` calls `beginDraggingSessionWithItems:event:source:` with
`NSFilePromiseProvider` items, then drives the session with posted drags and a release.
Success is a `draggingSession:endedAtPoint:operation:` with `.copy` and one promise write per
file into the Finder destination.

```
cd spikes/macos-drag-attach && swiftc -O -o macos-drag-attach main.swift
./macos-drag-attach --origin 900 300 --target 900 700 /path/a.txt /path/b.bin
```

The binary (or the terminal launching it) needs Accessibility permission. Put a Finder
window or the Desktop under the target point. Do not run it while the Mac is being driven
through Splice; the posted events would fight the injector.

## Fallback ladder

1. KDE, wlroots (sway, Hyprland, niri), COSMIC: layer-shell origin, confirmed on KWin.
2. GNOME: fullscreen toplevel origin unmapped after `start_drag`, confirmed on Mutter 50.4 with
   GTK4 and Nautilus. Mapping a fullscreen toplevel mid-drag also captures a foreign drag.
3. Any compositor where neither works: the existing two-gesture shelf flow.
4. macOS: AppKit panel plus promise providers, pending Finder verification.
