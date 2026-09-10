# Protocol 6 input transport and boundary repair

Work in this document is dated 2026-09-10 and starts from commit
`64b467979f35b3e5bc35d34d8c86a5f36eb3c53e`. Protocol 6 builds are installed at
`/Applications/Splice.app` and on the KDE `gamedev` destination. Both report the expected
commit and dirty metadata, and the installed pair connected over UDP. Claude Fable 5.1 and
Kimi K3 approved the result after their findings were resolved and the affected regressions passed.

## Transport

Both input modes use authenticated UDP over the existing Tailscale route. Desktop input
uses UDP 41717; Raw uses UDP 41719. TCP 41717 carries identity, session ownership, layout,
clipboard and other control messages. TCP 41718 remains the updater port. Every endpoint
must use protocol 6 and permit these ports on the Tailscale interface. A blocked UDP
handshake reports an error; there is no TCP input substitution.

Each direction has a random 128-bit token exchanged over the authenticated control
connection. Receivers check the token, source IP and pinned source port before decoding.
Raw also checks the source's Tailscale identity against the control-session owner.
New peer connections receive new tokens. Raw sessions reuse a persistent listener and
receive separate session authorizations.

Datagrams are at most 1,200 bytes. Movement and wheels carry cumulative totals. A newer
packet recovers distance from a missing motion packet without waiting for retransmission.
Key, button and scroll-stop transitions remain in a bounded journal until acknowledged.
Every transition includes its capture timestamp and the cumulative position before it.
The receiver reaches that position before applying the transition. It defers later motion
if an earlier transition is missing, preventing lost clicks, double taps and collapsed drags.

The source validates device sequences before aggregation. Multiple devices holding the
same key remain a union. The destination reconstructs ordered injection reports, retains
native timing for healthy report delivery, and maps timestamps into its local monotonic
clock. There is no replay timer or intentional motion coalescing. Lost packets necessarily
lose intermediate path samples; later cumulative totals recover displacement and the
positions of retained transitions.

Pending input is retransmitted every 20 ms. Connected Desktop channels use 100 ms heartbeats, independently of session departures. Actual source capture age, pending-transition age and mapped
injection age remain bounded to 750 ms. The mapped age excludes the unknown fastest
network transit time. A normal Desktop departure waits for pending input acknowledgements
while control reads and heartbeats continue. A destination-initiated departure abandons
that journal because the destination has already released its held state. Normal Raw crossings
stop capture and drain queued reports and acknowledgements before TCP Leave, bounded to 750 ms.
Panic, source-claim changes and failure paths abort immediately. Raw has no separate UDP close
message that could race ahead of the control Leave.

## Boundary repair

Linux reports actual destination boundaries to the physical source. The source checks current
ownership, session, focus lock, live links and corner dead zones before returning or moving onward.
Native callbacks include observed edge geometry, so events queued before geometry changes are rejected.
Focus-lock changes propagate during a session. Raw entry places the cursor eight logical pixels
inside the destination and waits for subsequent motion before arming boundary observations.
The Raw observer has no 750 ms Desktop rearm delay; the core suppresses duplicate pending offers.

The native KDE test exposed an existing startup defect: SCTK enumerates initial Wayland seats
without calling the hotplug `new_seat` handler. The overlay kept no selected seat, rejected capability
callbacks and created no pointer subscription. Registering the initial seats fixes that path.
After correcting a separate fixture display-initialization error, the actual uinput pointer reached
all four KDE boundaries without local capture or entry bounce. The integrated fixture passed eight sessions: all four edges, each repeated twice without recreating
the same edge between consecutive sessions.

## Measurements

The original live Raw session logs showed roughly 0.5 ms average capture-to-enqueue delay,
0.1 ms source queue time and 0.2 ms socket-write time. Linux uinput writes averaged around
10 microseconds. Delivery jitter above the observed transit floor reached 90–130 ms at the
reported p99 histogram bounds and 171 ms maximum.

Matched 256-byte TCP/UDP echo probes at 125 and 1,000 Hz reproduced approximately 100 ms
p99 round trips over both Tailnet and LAN addresses. The accepted TCP socket and client
socket both had TCP_NODELAY in the corrected runs. Direct LAN routing did not materially
remove the tail. These are round-trip observations, not one-way or application latency.

Mac-to-access-point pings reached 93 ms; the Linux-to-access-point sample stayed below
10 ms. A separate probe measured Mac scheduling lateness below 8 ms maximum, with near-zero
correlation to RTT, and server processing below 27 microseconds maximum. This points to
the Mac Wi-Fi/AP leg as a separate contributor. It does not establish AWDL, power saving,
or any particular driver behavior as the cause.

Apple documents socket service classes in [QA1934](https://developer.apple.com/library/archive/qa/qa1934/_index.html)
and warns that they do not guarantee delivery or extra bandwidth. Its
[peer-to-peer networking guidance](https://developer.apple.com/documentation/technotes/tn3213-moving-from-multipeer-connectivity-to-network-framework)
notes that peer-to-peer Wi-Fi can affect performance. An active AWDL interface alone is
insufficient evidence to attribute these spikes to AWDL. No network interface or system
power setting was changed for the measurements above.

A further 24 paired UDP runs compared default, responsive-data and signaling socket
service classes over Tailnet and LAN at 125/1,000 Hz. The class settings were accepted
by Darwin, but showed no consistent latency or loss improvement. Splice therefore retains
the default service class. The separate outer Tailscale socket appeared as best effort;
this is an observation, not proof of how individual packets were marked.

[Apple's sendto documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/sendto.2.html)
identifies ENOBUFS as a possible temporary interface-queue failure. Splice treats that
specific error as a dropped UDP datagram, counts it in aggregate warnings, and lets the
normal cumulative/journal recovery proceed within its existing deadline.

## Validation

- All 61 engine integration tests passed in debug and release on macOS, including three-machine
  transitions, held keys, Raw/Desktop switches, restart, and clipboard load.
- The 125/500/1,000 Hz Raw test preserved every report while transferring an 8 MiB clipboard.
- A real UDP relay deliberately dropped 20% of data packets, reordered and duplicated
  packets, and dropped acknowledgements. At 1,000 Hz, every button transition arrived once,
  in order, at its recorded cumulative position; final displacement matched.
- Native timestamp spacing survived an injected 100 ms receiver stall without paced replay.
- Recovery covered 100 fast key taps, missing button-down packets, stale packets, expired
  transitions, idle sessions and disconnect release.
- Token, source-port, datagram-size, prior-connection-token and Raw WhoIs checks passed.
- A full clipboard queue could not delay Desktop key transitions or fractional movement.
- All 14 peer-network integration tests and 96 core library tests passed on Mac and Linux.
- Full workspace tests passed on both platforms; Linux had 296 passing tests before the final
  drain regression, and its affected suites passed all 171 tests afterward. Both platforms passed
  workspace/all-target Clippy with warnings denied. Packaging passed five tests.
- Mac native HID discovery passed. The installed Mac bundle passed strict signature verification
  using the existing Splice Dev identity.
- A manual Raw handoff with 100 queued button transitions retained every transition and produced
  no source or destination input error. Stale local EdgeHit events cannot bypass the Raw entry gate.
- Follow-up tests cover destination-initiated departure without a later disconnect and
  abandonment of a refused Desktop session.
- Baton reviews covered transport ordering, authentication, lifecycle, boundary gating and
  handoff draining. Fable approved the integrated change and final drain fix. Kimi approved
  after independently verifying all three of its findings were resolved and running the three
  focused regressions. Native KDE and installed-app validation were performed separately.

One initial Linux full-suite run exceeded the 50 ms clipboard-load test threshold: p95 was 71.6 ms.
The isolated rerun measured p95 2.3 ms and maximum 12.4 ms, and the subsequent full-suite rerun
passed without a code change. The cause of that single full-load excursion was not established.

Local logs are under `build/udp-review-evidence/`. Matched probe data, exact commands and
primary-source research are copied to `build/udp-review-evidence/network/`. Native KDE validation
moves only the pointer and restores the installed service on exit. The installed KDE service reports
healthy capture, injection, clipboard and activity-monitor backends, with a crossable link to the Mac.
After restoring its window, `app-splice.service` remained active and the Mac reconnected.
Its initial sampled direct-connection RTT was 6.8 ms. A temporary UDP 41719 echo probe completed
in 7.5 ms, confirming that the Raw port is reachable; its socket was closed afterward. These individual
observations are not a latency distribution.

The other Fedora peer still runs an incompatible older protocol and refuses SSH on port 22, so it
was not updated. The matching Linux executable is available locally at `build/splice-linux-x86_64`.
GNOME, physical end-to-end feel and game acceptance are not established by the KDE
pointer fixture. No claim of VirtualHere-equivalent input-to-display latency is made.

Installed-version backups are `/tmp/Splice-before-udp-20260910.app` on Mac and
`~/.local/bin/splice.before-udp-20260910` on gamedev. User input-mode settings were retained.
