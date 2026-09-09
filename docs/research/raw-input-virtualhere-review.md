# Raw input, VirtualHere, and the keep-or-revert decision

This records the investigation before the timing repair. See the subsequent
[protocol 5 implementation and validation](../raw-input-timing-validation.md) for the current behavior.

Reviewed 2026-09-06 against `83aa6f7` through `d603bc2` and the current working tree. The branch adds roughly 9,000 lines across 64 files, including raw input, crossing policies, UI, and tests. The reported symptoms are poor movement speed and stutter on Linux desktops, using both macOS and Linux sources. The user reports excellent Mac-to-Linux results with VirtualHere on this network.

## Decision

The evidence supports one bounded repair and comparison pass. It does not support accepting the current implementation as finished, promising an easy fix, or starting a USB passthrough rewrite.

Splice's transport and Linux output can process mouse reports quickly. The unresolved issues are preserving the meaning and timing of those reports through the complete path. Device settings demonstrably change when Linux sees Splice's generic mouse. Timing information is discarded, and a stalled receiver replays reports in a burst. There are also reproducible lifecycle and settings bugs.

If matching VirtualHere requires a new USB transport, broad device emulation, or a continuing series of device exceptions, that exceeds the user's "fix it easily" condition. Revert or withdraw Raw at that point. Keep Desktop available while assessing Raw. A good raw implementation should also behave well on a desktop; testing desktop movement is a valid acceptance test.

This investigation changes documentation and adds isolated diagnostic artifacts. It does not alter production code, install a new Splice build, or change either computer's mouse settings.

## What VirtualHere does

VirtualHere redirects USB devices. Its Linux client uses the kernel's USB/IP client driver, including `vhci_hcd`. Linux then handles an imported USB device through its normal device stack. The Linux USB/IP architecture runs the peripheral's driver on the importing computer. This preserves the device description available to that driver, instead of requiring Splice to map every HID control into its own event vocabulary. [VirtualHere client](https://www.virtualhere.com/usb_client_software), [client driver requirements](https://www.virtualhere.com/client_configuration_faq), [Linux USB/IP architecture](https://docs.kernel.org/usb/usbip_protocol.html).

VirtualHere's documented default transport is TCP on port 7575. It also offers encrypted TCP and a reliable UDP connection through EasyFind. TCP itself therefore does not explain the quality difference. The public documentation does not establish VirtualHere's complete wire format, socket options, request scheduling, or buffering implementation. Using Linux's USB/IP driver does not establish that every VirtualHere network message uses the stock USB/IP wire format. [VirtualHere network transports](https://www.virtualhere.com/node/2447).

The developer describes forwarding mouse messages promptly, without prediction, in a high polling rate discussion. That is evidence for avoiding deliberate input delay. It does not prove a one-report-per-IP-packet rule or immunity to network stalls. A separate KVM/IP emulator product received a mouse rate-limiting fix; that change cannot be assumed to describe ordinary USB mouse redirection. [Developer discussion](https://www.virtualhere.com/node/4810), [client changelog](https://www.virtualhere.com/node/955).

On macOS, USB redirection involves taking access to the peripheral. VirtualHere's server changelog includes exclusive-access fixes and return-to-driver behavior. Its exact macOS implementation is not public in the material reviewed. This ownership model differs from Splice's simultaneous HID monitoring and desktop-event suppression. [Mac server](https://www.virtualhere.com/osx_server_software), [server changelog](https://www.virtualhere.com/node/958).

A Bluetooth keyboard paired to the Mac is not itself a USB peripheral. Forwarding a USB Bluetooth adapter transfers access to the adapter and its client-side Bluetooth stack. That is a different ownership and pairing model from forwarding the paired keyboard's HID reports. VirtualHere documents Bluetooth adapter use, with device-specific caveats. [Bluetooth discussion](https://www.virtualhere.com/node/2857).

The installed Mac VirtualHere server is version 4.8.6. I did not acquire physical devices with it during this review. There is no simultaneous VirtualHere performance trace to compare with today's probes.

## What Splice does

```text
Mac:   HID report -> Splice HID decoder ---------+
                                                |
Linux: evdev events -> SYN_REPORT group ---------+-> RawReport
                                                    |
                                          bounded source channel
                                                    |
                                         dedicated TCP_NODELAY
                                                    |
                                           raw ownership ledger
                                                    |
                                    generic relative uinput mouse
                                                    |
                                            Linux input stack
```

The decoder sends signed counts, keys, buttons, and wheel units. It does not forward a USB device, its descriptor, its output reports, or its driver configuration. All physical pointing devices share one destination mouse identity: `BUS_VIRTUAL`, vendor `0x5350`, product `3`, name `Splice Virtual Raw Mouse`. The protocol's device number tracks held state, rather than creating a separate Linux device. See [HID decoding](../../crates/splice-platform/src/raw/hid.rs), [raw protocol](../../crates/splice-proto/src/raw.rs), [transport](../../crates/splice-core/src/raw_transport.rs), and [Linux output](../../crates/splice-platform/src/linux/raw.rs).

Desktop captures motion already processed by the source desktop, applies link sensitivity, and uses absolute pointer placement on Linux. Raw preserves device counts and lets the receiving input stack process a relative mouse. Equal physical movements need not produce equal desktop distances under these two configurations. That is a settings and fidelity problem to solve, not evidence that good relative input is impossible.

## Movement findings

### Device identity and settings are lost

The protocol has no device registration carrying resolution, capabilities, or identity. The Linux builder supplies a fixed identity and no `MOUSE_DPI` property. Libinput assumes 1000 DPI when that property is absent. This can change normalization compared with a correctly identified physical mouse. It does not prove every non-1000-DPI mouse differs from direct attachment: the physical device may also lack an accurate DPI entry. Its actual DPI and settings must be measured. [Libinput normalization](https://wayland.freedesktop.org/libinput/doc/latest/normalization-of-relative-motion.html).

There is concrete local evidence for a settings mismatch. On `gamedev`, `~/.config/kcminputrc` has `PointerAcceleration=-0.400` for `[Libinput][1133][50504][Logitech USB Receiver Mouse]`. There is no Splice mouse entry or pointer-default override in that file. KWin loads device configuration using vendor, product, and name, so that saved receiver setting does not transfer to Splice's generic identity. This establishes the missing setting inheritance, not the exact speed of the earlier session. [KWin configuration lookup](https://raw.githubusercontent.com/KDE/kwin/master/src/backends/libinput/connection.cpp).

`gamedev` currently runs KDE. No Splice process or persistent Splice raw device was active when inspected. `libinput list-devices` showed physical input devices, including a Logitech G502 and NuPhy keyboard, plus keyd virtual devices. Its displayed acceleration defaults belong to that diagnostic's libinput context; they do not establish KWin's active configuration. The other online Linux peer refused SSH, so its GNOME settings remain unchecked.

A hard-coded DPI value for every source cannot solve this. Nor can one combined virtual mouse preserve different settings for two physical mice. A complete event-based design needs an explicit resolution/calibration policy and appropriate device identity. Changing raw counts to match Desktop's sensitivity silently would defeat the raw-count contract.

### The pipeline discards capture timing

Both sources set `captured_us` when enqueueing a decoded report. Linux discards the original evdev timestamp. Mac registers the callback without a timestamp, although the installed SDK exposes `IOHIDManagerRegisterInputReportWithTimeStampCallback`. The receiver uses `captured_us` only to reject decreasing timestamps. `InputEvent::new` supplies zero timestamps to uinput.

Consequently, source stalls are hidden in the recorded capture times, and received reports acquire injection timing. The actual transport reproducer confirms that a 100 ms stall produces a burst of about 100 reports afterward. Libinput's adaptive acceleration uses deltas and timestamps to estimate velocity, so changing their spacing can change acceleration as well as visible cadence. The resulting distortion is a credible shared explanation for both sources. Its size in the user's physical sessions is not yet measured. [Libinput acceleration](https://wayland.freedesktop.org/libinput/doc/latest/pointer-acceleration.html).

There is a possible repair that does not require a jitter buffer: preserve capture timestamps and map them into the receiver's monotonic clock domain. Current Linux uinput accepts positive monotonic timestamps within the preceding ten seconds, rejecting future timestamps. The old example comment about ignored timestamps must not be generalized to all timestamps. Cross-machine epoch differences, drift, multiple devices, and decreasing network delay still need correct handling. Accurate timestamps can improve input processing; they cannot make late movement arrive earlier. [Current uinput implementation](https://raw.githubusercontent.com/torvalds/linux/master/drivers/input/misc/uinput.c).

A native test on `gamedev`, Linux `7.1.13-200.fc44.x86_64`, verified this behavior. It supplied a timestamp 100 ms before the current monotonic time, emitted immediately, and read the identical timestamp from both the relative event and SYN_REPORT. No 100 ms sleep or replay buffer was involved.

The channel limit is 1,024 reports, not an age limit. At 1,000 reports per second that represents about a second of input in that channel alone. Kernel socket buffers can hold more. The 750 ms I/O/heartbeat checks detect failure, but do not promise interactive report freshness. Neither a larger queue nor paced replay is a general latency fix. Coalescing changes report boundaries and potentially acceleration; batching system calls while preserving all report boundaries is a different operation.

### Measurements narrow the suspects

| Probe | Result | What it establishes |
|---|---|---|
| Actual Rust transport, 125 Hz, Mac loopback | 250 reports; enqueue-to-mock-inject p99 0.471 ms | This transport has no inherent frame-sized delay in this condition |
| Actual Rust transport, 500 Hz, Mac loopback | 1,000 reports; p99 0.582 ms | Same |
| Actual Rust transport, 1,000 Hz, Mac loopback | 2,000 reports; p99 0.364 ms; maximum 0.764 ms | Low isolated transport overhead |
| Actual Rust transport, nominal 8,000 Hz, Mac loopback | 16,000 reports; p99 0.051 ms | Capacity experiment only; generator spacing is imperfect |
| Same transport, injected 100 ms receiver stall at 1,000 Hz | Maximum age 101.373 ms; 103 reports in a run with spacing below 100 µs | Backlog is delivered in a burst; source timing does not pace injection |
| Actual Linux `Devices::emit`, 1,000 Hz | 2,000 SYN_REPORT groups; sums +2,000 X / -2,000 Y; write p99 3.160 µs, maximum 43.559 µs | The isolated uinput writer is fast and preserves these counts |
| 32-byte TCP_NODELAY echo, direct LAN, after builds | 4,000 messages; RTT median 6.604 ms, p99 98.793 ms | Current end-to-end TCP delivery has significant tails |
| Same echo, Tailscale direct connection, after builds | 4,000 messages; RTT median 7.114 ms, p99 97.005 ms | A DERP relay is not required for the observed tails |

The network experiment was repeated after the builds because the first sample overlapped compilation. Both samples showed similar tails. The source generator's p99 spacing was about 1.2 ms. RTT includes both network directions and echo-process scheduling; it is not one-way mouse latency. These are not VirtualHere measurements and do not invalidate the user's successful comparison.

The Linux output probe exclusively grabbed only its own newly created fixture mouse before emitting generated motion. It did not grab a physical device or move the desktop pointer. The transport sink omitted native capture, compositor processing, and rendering. These measurements therefore do not establish hardware-to-display latency or physical smoothness.

The evidence does not justify calling synchronous uinput writes the dominant delay. They remain synchronous work on a Tokio worker, but measured writes were microseconds. Likewise, Linux's 250 ms sysfs discovery scan runs on the capture thread and deserves timing instrumentation, but its delay is unmeasured and cannot explain a Mac-source-only path. The 50 ms `poll` timeout and Mac's 20 ms run-loop slice are readiness waits, not mandatory per-report delays.

## Other confirmed code issues

### Medium: old Mac errors can end a new session

At [macos/raw.rs](../../crates/splice-platform/src/macos/raw.rs), `begin` discards `_operation`; `fail` emits `PlatformEvent::RawError(String)` at line 189. [inner.rs](../../crates/splice-core/src/engine/inner.rs) handles that event at line 532 without the operation check used for Linux's `RawCaptureFailed`.

An old HID failure and its transport-close notification can race with a target switch through separate engine event sources. After the next session starts, the delayed unscoped error ends that session. An isolated engine regression established a new session to `ccc`, delivered the old error, and failed with `Local` instead of `Remote(ccc)`.

The repair is to retain the operation supplied to Mac capture and use the existing scoped failure event for active-session failures. Global discovery/permission health still needs a separate readiness representation. This requires no wire-protocol redesign and does not establish the cause of continuous stutter.

### Medium: Mac handoffs omit held mouse buttons when entering Desktop

The new [handoff_remote](../../crates/splice-core/src/engine/inner/raw.rs) path at line 505 preserves a source ledger only for Linux Desktop sessions. For Mac, it ends capture and calls `start_desktop`. [TapState::begin](../../crates/splice-platform/src/macos/tap.rs) replays held keyboard keys, but the tap has no corresponding held-button snapshot. Raw mode also bypasses the core Desktop ledger.

Hold a mouse button while switching from a Raw destination to a Desktop destination: the old destination receives release, and the new destination receives no press to continue the drag. The physical release may subsequently be filtered as an unheld release by the core ledger. Keyboard modifiers have a replay path and should not be reported as the same bug. The new manual Desktop-to-Desktop handoff can also lose buttons; the existing automatic Desktop crossing uses a different path that preserves the ledger.

This is confirmed by tracing both capture and engine state, rather than by a physical handoff test. Repair needs a proper Mac held-state snapshot at the mode boundary.

### Medium, conditional: delayed remapper button echoes can steal ownership

[Linux raw injection](../../crates/splice-platform/src/linux/raw.rs) records injected keys at lines 93–97 but omits mouse buttons. The Desktop uinput path records both. [The activity monitor](../../crates/splice-platform/src/linux/activity.rs) uses that record to suppress matching remapper echoes for one second, beyond its general 150 ms recent-injection window.

If a destination remapper re-emits a mouse button after more than 150 ms without intervening injection, the raw path can misclassify it as physical activity. The engine then claims local ownership and ends the driven session. This requires a delayed remapper echo; ordinary pointer movement is not evidence of this bug. Record button codes and release events consistently with Desktop. This is a source-traced finding, not a reproduced remapper incident.

### Medium, uncommon upgrade case: valid old settings can prevent startup

[InputSettings::load](../../crates/splice-core/src/input_settings.rs) copies nonzero legacy `edge_dwell_ms` into Dwell and then enforces 50–5,000 ms. The existing configuration loader accepts values outside that range. A valid `config.json` with `edge_dwell_ms=25` and no `input.json` therefore fails the new migration. `Inner::new` propagates that error, affecting Desktop startup as well.

A public-API probe reproduced successful legacy configuration loading followed by the migration error. This concerns manually configured old values, not the normal zero default. Migrate accepted legacy values explicitly instead of treating them as newly malformed input.

## Compatibility policy and checked non-findings

The unused Air75 usage and receiver button declarations no longer block discovery. The Mac duplicate IOHID subscription defect is also already fixed in the working tree and has a native regression. Neither should be counted as an outstanding discovery from this review. [Prior Mac evidence](../raw-input-macos-validation.md).

An asserted unsupported control still ends the entire session by design. Splice supports eight mouse buttons and a selected key mapping; it cannot "just forward" an unknown control through that vocabulary. Linux's HID stack maps more controls and can ignore individual unhandled usages. The eight-button ceiling is a Splice choice, not a universal HID or Linux limit. Broad compatibility needs richer capabilities or forwarding at the HID layer, rather than a series of device-name exceptions. [Linux HID mapping](https://raw.githubusercontent.com/torvalds/linux/master/drivers/hid/hid-input.c).

Other deliberate limits include stopping Linux capture on relevant hotplug, requiring a Wayland capture session on Linux, and omitting automatic return-edge observations in Raw. They weaken seamless KVM behavior but are documented product limits. Losing track of held input, queue overflow, revoked suppression, or a dropped kernel report still requires explicit release; indiscriminately swallowing those errors is unsafe for input state.

The audit found no supported case of simultaneous Desktop and Raw motion injection. Both TCP endpoints enable `TCP_NODELAY`. The target checks peer identity, control-session authorization, a random ticket, and session generation. Packet length is bounded before allocation. The ledger combines held keys/buttons across devices and releases them on disconnect. Linux filters Splice virtual devices from physical capture. Existing count, high-resolution wheel, ownership, handoff, and cleanup tests pass.

The claim that Linux permission errors remain permanently latched was rejected: failed device opens retry on the periodic scan and successful opens clear the failure. No raw-input PR discussion was available; the checkout is `main`, and `gh pr list --state open --head main` returned none.

## A bounded repair pass

1. Establish a matched physical comparison using one mouse, one fixed DPI/polling setting, one Linux destination, and the same path. Compare direct attachment, VirtualHere, Desktop, and Raw. Inspect the actual compositor settings and hwdb properties for each device. Use an explicitly configured Splice-only flat profile as a diagnostic to separate acceleration distortion from delivery delay, then restore the user's chosen profile.
2. Add aggregate timing measurements at native capture, enqueue, send, receive, and injection. Record queue age, report gaps, and write duration without logging keyboard contents. Preserve native timestamps separately from the globally ordered report sequence. Do not subtract unrelated machine clocks as one-way latency.
3. Correct the missing device calibration/settings policy and, if the timing comparison supports it, preserve event timestamps at injection with explicit clock mapping. Keep immediate delivery and report boundaries. Move or batch work only where measurement identifies a bottleneck.
4. Fix the scoped Mac error, held-button handoff, echo accounting, and migration defects with regressions. Re-run the physical comparison, including switching, unplugging, simultaneous typing/motion, and clipboard load.

Success means Raw feels comparable to the user's working VirtualHere setup and has corresponding count/cadence evidence. Passing loopback tests or reducing average latency alone is insufficient. If this pass exposes a need for a broad rewrite, stop and revert Raw under the user's stated criterion.

If transparent HID compatibility becomes the actual requirement, Linux UHID is an architectural option between generic uinput and full USB redirection. It accepts HID device descriptions and reports through a userspace transport. A real implementation also needs device lifecycle, feature/output-report round trips, permissions, and source ownership. USB-specific driver behavior is not automatically preserved. That is a separate project, not the proposed small repair. [Linux UHID](https://docs.kernel.org/hid/uhid.html).

## Evidence and reproduction

- Mac `cargo test -p splice-platform -p splice-core -p splice-proto --locked --offline`: 164 passed, one native test ignored.
- Linux source archive, same worktree: 165 core/platform tests passed, five native tests ignored. Two selected native descriptor/capability checks also passed. Tests ran in `/tmp/splice-raw-review-20260906`; the installed Linux app was not replaced.
- [Transport probe source](../../build/raw-input-review-evidence/transport-probe/src/main.rs) includes the production `raw_transport.rs` directly, with a measurement sink and test identities. Run its Cargo project in release mode using `--bin splice-raw-transport-probe`.
- [Transport results](../../build/raw-input-review-evidence/transport-probe-results.jsonl), [network probe](../../build/raw-input-review-evidence/tcp-cadence.py), [network results after builds](../../build/raw-input-review-evidence/tcp-cadence-idle-results.jsonl).
- [Linux injection probe](../../build/raw-input-review-evidence/linux-injection-probe.rs) and [result](../../build/raw-input-review-evidence/linux-injection-probe.log). Append only to an isolated copy of `linux/raw.rs`, then run the named test. It grabs only its own generated fixture device before emitting input.
- [Native timestamp probe](../../build/raw-input-review-evidence/linux-timestamp-probe.rs) and [result](../../build/raw-input-review-evidence/linux-timestamp-probe.log). Run in the same isolated manner; it verifies timestamp preservation on a generated fixture mouse.
- [Negative Mac-error regression](../../build/raw-input-review-evidence/review-regressions.rs) and [failure](../../build/raw-input-review-evidence/review-probes.log). Append to an isolated copy of `engine_e2e.rs`; failure is the expected reproduction of the defect.
- [Settings probe](../../build/raw-input-review-evidence/transport-probe/src/bin/settings-probe.rs) and [result](../../build/raw-input-review-evidence/settings-probe.log).

Diagnostic sources and logs under `build/` are local ignored evidence, not production additions. This report records their conclusions and limits.

Independent Baton reviews and research were checked against source and experiments. One review returned only progress text and was unusable. Other returned claims were rejected where evidence contradicted them, including universal DPI mismatch, permanent permission failure, one-write-per-packet assumptions, and the claim that uinput cannot accept timestamps. The recommendations above do not rely on those claims.
