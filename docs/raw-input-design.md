# Raw input and deliberate edge crossing

Status: shared implementation and automated Linux checks are in the working tree for Splice 1.2.0,
protocol 4. The native Linux device capability check passed; native desktop and game validation remain. Mac HID capture code passes Apple target compilation checks. Native Mac capture,
local suppression, and gaming acceptance remain unverified. Continue with the
[Mac handoff](raw-input-macos-handoff.md) before releasing raw mode.

The current implementation requires explicitly selected focus lock for raw input. Destination
edge observations are not implemented. Dwell and resistance work with a Mac source, including
its desktop sessions across Linux destinations. Linux source edges retain Immediate crossing;
selecting a delayed policy there produces an error. Progress appears in the Splice window.
A native indicator at the physical screen edge remains Mac follow-up work.

## Agreed scope

Keep the current desktop mode fully supported, including all existing directions, clipboard sharing,
layout, held modifiers, reconnection, and emergency release. Add a selectable raw input mode alongside it.
Raw mode initially supports a mouse and keyboard physically connected to a Mac controlling Linux.
Both Fedora GNOME and KDE are target desktops. Linux-to-Mac raw input is outside the first version.

Support standard HID mouse and keyboard input without model-specific implementations or a vendor
allowlist. USB cables and wireless USB receivers are the first validation targets. Bluetooth uses
the same input model and is part of the compatibility target. It must not require a second protocol.
Actual Bluetooth behavior, including reconnect and wake, needs separate validation.

A deliberate edge crossing delay is acceptable. Continuous input after crossing must have no added
smoothing, artificial delay, or conversion through desktop pointer acceleration.

## Why raw input can improve gaming

The current mode captures source-accelerated logical pixels. The core applies link sensitivity and
tracks a virtual cursor. The destination applies those pixels through desktop injection APIs or the
Linux absolute uinput pointer. This preserves desktop cursor speed and coordinated edge crossings.

The Linux pointer is already a virtual device, but its axes are absolute. A gaming mode needs the
relative movement that a physical mouse supplies. Both capture and injection must change together:
feeding accelerated desktop deltas into a relative mouse would apply destination processing again.

The implemented path is:

```text
Physical USB or Bluetooth mouse and keyboard
    -> macOS HID capture, before desktop pointer acceleration
    -> ordered input connection over the tailnet
    -> Linux virtual relative mouse and keyboard
    -> Linux desktop or game
```

Linux supports relative virtual mice and keyboards through
[uinput](https://docs.kernel.org/input/uinput.html). Apple's HID interfaces expose physical device
input, including [Bluetooth keyboard input and reports](https://developer.apple.com/documentation/corehid/communicatingwithhiddevices).
This supports a transport-independent design. Exact device behavior still requires measurement.

This forwards standard input into virtual devices. It does not need to export a Bluetooth radio,
clone a particular mouse's USB bus connection, or transfer device ownership between USB drivers.
USB-specific vendor software, firmware updates, RGB controls, and arbitrary peripheral passthrough
are separate features and are outside this mode.

VirtualHere is a useful benchmark because the user has already achieved acceptable FPS behavior
with it on the local network. Its [client](https://www.virtualhere.com/usb_client_software) imports USB
devices, and its [control API](https://www.virtualhere.com/client_api) supports acquisition and release.
Splice's implementation is native and does not depend on a VirtualHere installation or license.

## Settings and compatibility

The controls are independent:

| Setting | Choices | Initial behavior |
|---|---|---|
| Input mode for a destination | Desktop, Raw input | Desktop remains the default |
| Edge crossing | Immediate, Dwell, Resistance | Existing immediate behavior remains the default |
| Focus lock | Off, Locked to selected computer | Explicit user selection; switch shortcut and emergency release remain available |

Raw input can be selected for a supported Mac-to-Linux route. Device permissions, target readiness,
and mode capabilities must be checked before the source starts swallowing input. An unsupported
selection reports the missing capability. Splice never silently substitutes desktop mode.

Desktop link sensitivity continues to apply only to desktop mode. Raw mode preserves device counts.
The receiving desktop or game applies its own settings, as with a locally attached relative device.
The standard keyboard path preserves physical key positions, simultaneous keys, modifiers, and
press/release ordering. The destination owns keyboard layout and repeat behavior.

The transport change requires all peers to run the same new Splice release. Keeping desktop mode is
a product guarantee, not an old-wire-protocol compatibility mode.

## Capture on the Mac

Use macOS HID device discovery and input callbacks to identify standard mouse, keyboard, and consumer
control usages. Interpret capabilities and HID elements or descriptors rather than special-casing
particular models. Keep device identity separate from its USB or Bluetooth transport.

The implementation uses IOHIDManager input report callbacks and descriptor-driven decoding.
It monitors devices without seizing them. The existing active session event tap suppresses local
mouse and keyboard events; a second active tap suppresses media events of type 14. Native tests
must prove these two paths neither lose physical reports nor leak local input.
Timestamps currently record callback enqueue time on a monotonic clock, not hardware report time.

Capture must preserve signed relative motion, buttons, wheel resolution, physical key state, and
standard media keys. Handle composite receivers, multiple HID collections, different report IDs,
multiple simultaneous devices, hotplug, and Bluetooth reconnect. Resolve input permission failures
explicitly and identify unsupported input capabilities before activation.

Keep the existing cursor and emergency-release machinery. Verify how HID capture and local event
suppression interact before release. Input must reach exactly one active destination;
it must not also move the Mac pointer or type into Mac apps. Selected devices' state transitions must
remain ordered at capture activation and release. A read-only HID monitor alone is insufficient.

The Mac is only the source in this version. Creating virtual input devices on macOS, obtaining
virtual-HID or DriverKit distribution entitlements, and bundling a Mac driver are not prerequisites.
Any future raw input into macOS needs its own platform and signing validation.

## Injection on Linux

Create a relative uinput mouse and virtual keyboard when raw mode is enabled and keep them alive
across handoffs. This avoids device enumeration on every crossing. Advertise accurate input
capabilities and exclude Splice's own virtual devices from local capture and source-claim logic.

Emit relative axes and input synchronization groups. Do not send raw movement through the absolute
pointer's position accumulator or the core's desktop sensitivity multiplier. Device removal and
session loss must release all held keys and buttons, including input held by a device that vanished.
If several physical devices hold the same key, one device releasing it must not release the other.

Validate native Linux input, Wayland games, XWayland, and games through Proton. Generic virtual HID
input does not promise compatibility with every game or duplicate every vendor-specific feature.
The existing desktop backends remain available and independently tested.

## Transport and module ownership

Use a dedicated ordered input connection per peer so clipboard bytes cannot block behind or ahead of
raw reports on that socket. Reuse tailnet authentication and bind the connection to the authenticated
peer, negotiated input mode, and control-session generation. Creating a raw socket alone does not
grant ownership or permission to inject input.

The stream uses TCP port 41719 and `TCP_NODELAY`, preserving report order. VirtualHere's successful local-network
behavior makes an ordered transport a reasonable first candidate. TCP still stalls on packet loss;
measure this explicitly. A datagram transport is a separate decision only if measurements justify
its loss-recovery and ordering complexity.

Each report carries device identity, session generation, sequence, source timestamp, and its input
data. Keep timestamps for timing diagnostics without comparing unsynchronized clocks as if they were
one-way latency measurements. Preserve reports at supported polling rates without a frame timer or
lossy event queue. Overload ends the session with a visible error and releases held state.

Use distinct types for desktop pixel movement and raw device movement. The module ownership
keeps these contracts separate:

| Module | Responsibility |
|---|---|
| `splice-platform` Mac raw capture | HID discovery, physical input, local suppression, and permission health |
| `splice-platform` Linux raw injection | Persistent relative mouse and keyboard, device state, and forced release |
| `splice-proto` | Distinct raw reports, capabilities, and session-bound messages |
| `splice-core` input session | One active owner, readiness, handoff, ordering, and failure recovery |
| `splice-core` edge policy | Dwell and resistance state independent of input transport |
| `splice-app` | Mode selection, focus lock, readiness, and visible errors |

The desktop implementation retains its current contract. Raw movement bypasses the desktop
`on_remote_motion` path, which multiplies sensitivity and predicts crossings in pixel coordinates.
Avoid duplicating clipboard, discovery, peer authorization, layout replication, or update control.

## Handoff and the membrane

Model handoff explicitly as local control, edge contact, target preparation, remote control, and
release or recovery. A generation identifies the active session so a late acknowledgement or input
report from an old target cannot activate a stale handoff.

Prepare the target before transferring ownership. During the edge gesture, the pointer meets a
boundary and outward movement increases progress. Only after the crossing policy and target readiness
are satisfied does Splice commit the handoff, synchronize held state, and forward subsequent input.
The movement used to push through the membrane is consumed by that gesture and must not become a
burst of movement or a camera jump on the target. Preparation time is separate from steady input latency.

Resistance measures continued outward movement, not physical force. Tangential motion adds nothing.
Pulling away cancels the attempt. Pausing lets resistance progress decay. Use elapsed time and movement
distance so changing polling rate does not change the gesture. Normalize the gesture's configurable
feel separately from game movement, and test it across display scales and mouse DPI settings.
The Splice panel shows resistance progress, preparation, and failed crossings. A native screen-edge
indicator is still required to complete the planned gesture UI.

Dwell is a separate timed policy for users who prefer waiting at an edge. `input.json` stores
mode selection, focus lock, and crossing policy. On first use, an existing nonzero
`edge_dwell_ms` initializes Dwell. Subsequent changes keep that configuration field synchronized.
Malformed settings are preserved and reported, never replaced with defaults.

Apply the policy consistently to local-to-remote, remote-to-local, and remote-to-remote transitions.
When a Mac controls either Linux computer, switching between the Linux destinations can keep the Mac
as the physical input source.

## Edge observations and gaming lock

Raw counts cannot predict the Linux cursor position because Linux acceleration, application grabs,
and game camera movement determine their meaning. Use destination edge observations for return and
onward crossings. Validate those observations separately with GNOME's portal path and KDE's supported
capture path. Injection must not make the destination incorrectly claim physical input ownership.

This is a concrete feasibility checkpoint. Raw forwarding into a game can work before automatic raw
return crossings do. If a desktop cannot provide the needed edge observations, advertise that
limitation explicitly and offer an explicitly selected shortcut-only focus lock. Do not pretend
predicted cursor coordinates are reliable or silently alter the user's crossing setting.

Gaming lock disables automatic edge switching for that session. Turning a camera cannot take the
mouse to another computer. A dedicated switch shortcut chooses another destination or returns to
the Mac; the existing emergency chord must restore local control even if the network is unavailable.
No automatic fullscreen or game detection is required for the first version.

## Implementation sequence and acceptance

1. Establish a reproducible comparison between direct attachment, VirtualHere, and current Splice.
   Use the same mouse, polling rate, DPI, network, destination settings, and test application.
   Record counts and cadence separately from input-to-display latency.
2. Prove Mac raw capture and Linux relative injection with a manually selected, focus-locked target.
   Confirm original motion counts, simultaneous keyboard input, local suppression, and emergency release.
3. Add the authenticated input connection and session handoff. Test target preparation, failed handoff,
   clipboard saturation, sleep, crashes, unplugging, permissions loss, and reconnect.
4. Add mode selection, destination edge observations, dwell, resistance, and visible progress.
   Cover all three-computer transitions without replacing existing desktop behavior.
5. Validate USB cables and receivers, then Bluetooth, using the same protocol and device model.
   Complete native GNOME and KDE game testing before calling the mode suitable for gaming.

Automated tests must cover exact signed motion counts, report ordering, different HID report layouts,
standard extra buttons and media keys, wheel resolution, simultaneous keys beyond six-key rollover,
multiple devices holding the same key, stale sessions, duplicate injection, and release on failure.
Exercise supported report rates under load, including 125, 500, and 1000 Hz, with higher rates measured
on suitable hardware rather than assumed supported.

Membrane tests cover tangential movement, retreat, pause, diagonal approaches, changed topology,
handshake failure, polling-rate independence, and modifier state across the transition. Existing
mesh, clipboard, scroll, reconnection, and desktop-motion regressions remain release requirements.

For performance acceptance, compare the new mode with direct attachment and VirtualHere on the same
local network. Check high-percentile latency, event cadence, count preservation, and actual FPS aiming.
The existing loopback test is useful but cannot establish hardware-to-game feel. No zero-latency or
universal game-compatibility claim is part of this plan.
