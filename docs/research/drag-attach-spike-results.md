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

## GNOME / Mutter: NOT RUN

No GNOME session was reachable (aidev refused SSH; no GNOME on gamedev). Run 2 above shows
the layer-shell-free origin shape works on KWin; whether Mutter (a) delivers a libei or
uinput button to a fullscreen client toplevel with a serial it will honour in `start_drag`,
and (b) lets an input-region-empty fullscreen window pass drag targeting to windows beneath,
is still open. Run the same binary with `--mode toplevel` on a GNOME Wayland session:

```
cd spikes/wayland-drag-attach && cargo build
python3 target.py &            # fullscreen GTK4 drop target
./target/debug/wayland-drag-attach --mode toplevel --screen W H --origin X Y --target X2 Y2 /path/a /path/b
```

If (b) fails on Mutter, the fallback is to unmap the origin toplevel right after `start_drag`
instead of emptying its input region, and if that cancels the drag, GNOME keeps the
two-gesture shelf flow.

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
2. GNOME: fullscreen toplevel origin, pending Mutter verification.
3. Any compositor where neither works: the existing two-gesture shelf flow.
4. macOS: AppKit panel plus promise providers, pending Finder verification.
