# Design lessons from the field (why Splice is shaped this way)

Condensed from research into lan-mouse, Deskflow/Synergy (25 years of issue history),
Barrier/Input Leap (dead), rkvm, styx, Mouse Without Borders, and a binary teardown of Apple
Universal Control. Each lesson maps to a DESIGN.md decision; this file is the "why".

1. **Stuck cursors come from symmetric-capture designs.** lan-mouse needs the REMOTE machine's
   capture stack alive to give the cursor back; when a tap/portal dies you're stranded.
   Deskflow/Synergy never have this because the server owns the client's cursor. Universal
   Control: "sourceDevice" is a field in replicated state, not a role — symmetric peers, but
   the active source is authoritative for the cursor. Splice copies that exact shape.
2. **Stuck keys come from unreliable delivery of releases.** lan-mouse's own source admits
   "Leave can be lost over UDP/DTLS". styx (a lan-mouse rewrite) chose TCP for exactly this
   reason. Deskflow's 12-year "stuck modifier" bug class persists because recovery, not
   prevention, was the fix (they shipped a manual "clear modifiers" toolbar button).
   Splice: TCP + held-key ledgers on both sides + ReleaseAll + release-on-any-anomaly.
3. **Keyboard-layout translation (send characters, re-resolve) is a tar pit.** Deskflow's
   AltGr bug: open 11.5 years, 161 comments, still the #1 source of new reports. rkvm (raw
   scancodes only) was written specifically to escape this. Splice sends evdev scancodes.
4. **2-second liveness windows destroy WiFi usability.** lan-mouse closes the DTLS connection
   after 4 missed 500 ms pings; every suspend/hiccup = full re-handshake. Deskflow uses
   3 s/9 s. Splice: 1 s active/5 s idle heartbeat, 3 missed = degrade (release input, KEEP
   socket), never disconnect on heartbeat alone.
5. **Portal session churn is self-inflicted.** lan-mouse recreated its InputCapture session on
   every barrier update/keymap change — 34 ConnectToEIS calls in 2 h, fd leak, prompts.
   Sessions are precious: create once, reuse, batch barrier updates, rate-limit recreation.
6. **Ad-hoc code signing silently kills macOS permissions on every update** (TCC keys grants
   to the cdhash; the System Settings toggle still shows ON). Lan Mouse ships this bug today.
   Sign with a stable identity from the first dev build.
7. **Edge detection needs a state machine, not a coordinate test.** Apple: hot zone → dwell →
   ready → activate, with debounce/reject timers and the entry offset transmitted so the
   cursor lands where it left. Corner dead zones prevent hot-corner fights. Both-sides
   hysteresis prevents seam oscillation. Accumulate f64, round only at injection.
8. **Multi-monitor as "one big rectangle" is the top paying-user complaint** in Synergy's
   history (bounding-box unions create unreachable dead regions and broken edges). Per-display
   rects, real outer-boundary tests, dead-zone clamping on entry.
9. **Version by capability negotiation, never admission control.** Synergy 1's protocol has
   been compatible for 25 years ("the newer computer serves the older one at the older one's
   version"; "nobody gets turned away"). Postcard is positional: only ADD variants/fields,
   gate new behavior on negotiated caps.
10. **Keep the remote awake.** Universal Control takes a display-sleep assertion on the target
    and marks injected input as user activity; synthesized CGEvents alone don't wake a slept
    display.
11. **A panic escape hatch must be local and unconditional.** Apple ships
    Ctrl-Opt-Cmd-Delete. lan-mouse's release-bind runs through the (possibly dead) capture
    backend — ours must not.
12. **Nobody in OSS does discovery + arrangement UI + Wayland.** Deskflow explicitly refuses
    discovery; lan-mouse has no clipboard and no arrangement UI; everything else is dead or
    Windows-only. Tailscale LocalAPI gives discovery+identity+liveness for free. This is the
    product's reason to exist.
