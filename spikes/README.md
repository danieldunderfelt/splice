# Spikes

Throwaway experiments. Each directory is a standalone Cargo package or script, not a
workspace member, and nothing here ships. Results are recorded in
`docs/research/drag-attach-spike-results.md`.

- `wayland-drag-attach`: can an injected button press start a native Wayland drag that another
  application accepts, with injected motion driving it? `--mode layer` uses a layer-shell
  origin surface (KDE, wlroots); `--mode toplevel` uses a fullscreen toplevel (GNOME shape).
  `src/bin/inject.rs` is a scriptable uinput pointer and keyboard (`inject W H move X Y press glide X Y N release sweep STEP key super+left …`)
  that prints a timestamp per move so a window's position can be recovered from its own motion log.
  `target.py` is a fullscreen GTK4 drop target that writes what it received to
  `/tmp/spike-target-result.json`.
- `macos-drag-attach`: the same question for AppKit, using posted `CGEvent`s and
  `NSFilePromiseProvider` items.

Both inject pointer input. Run them only on a desktop you are not currently driving through
Splice, and keep the injected path away from armed screen edges.
