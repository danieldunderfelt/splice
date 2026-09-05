# Multi-machine investigation, September 2026

The reported setup has a Mac on the left, Fedora KDE in the middle, and Fedora GNOME on the
right. The investigation reproduced code defects in workspace replication, input handoff,
clipboard requests, and connection health. The live transport failure came from a Private
Internet Access split-tunnel rule for the installed Splice executable. Removing that rule
restored both live peer connections. The source changes address the separate code defects.

## Reproduced defects and changes

| Defect | Change and regression evidence |
| --- | --- |
| Adopting a newer two-machine layout erased the connected middle machine. | Merge missing members from both documents before publishing the resulting layout. `newer_two_machine_layout_cannot_erase_connected_middle_machine` failed before the fix. |
| The original three-machine fixture deliberately omitted the second-to-third connection. Outbound loopback sockets used the wrong source identity. | Bind outbound sockets to the local Tailnet address. Tests now require every pair to connect and every machine to adopt the arrangement. |
| The listener reported Connected after writing Welcome, even when the remote machine never received it. | Require Ready after Welcome, with a five-second handshake deadline. The raw-socket regression refuses a client that never confirms. |
| Dropping an engine left its listener and sessions alive. Active input could retain the idle heartbeat schedule. | Network tasks stop when their event receiver closes. Activating a session reschedules heartbeats. Both regressions failed before the fixes. |
| Concurrent clipboard offers from different machines reused the same offer ID and overwrote pending callbacks. | Each fetch has a unique request ID bound to its peer. Tests fetch different payloads concurrently, repeat fetches from one peer, and reject responses from another peer. |
| Onward crossings lost held modifiers. Emergency release from the third machine did not release the active pair. | Replay held input on the next target, with modifiers first. Panic has a distinct workspace-wide frame. Three-machine tests cover both directions and emergency release. |
| Physical activity on a disabled third machine stole control. | Disabled machines cannot claim control. A regression preserves the active pair after third-machine physical activity. |
| KDE capture health stayed clear when no output could host a requested edge. | Report unmapped edges. The live compositor test arms both real edges and checks that an impossible edge produces a health error. |

Additional changes bound socket writes and outgoing queues, reject unmatched heartbeat replies,
cancel clipboard work on disable and disconnect, and move clipboard reads out of the input loop.
KDE waits for pointer-lock activation before emitting an edge crossing and replays keys held at
activation. Explicit backend preferences do not select another implementation when unavailable.
Failed backend creation is retried during capability probes.

Existing configuration files must parse and validate. Invalid files are preserved and reported.
Save failures appear in the UI and retry. Linux service startup holds an exclusive file lock
before replacing a stale IPC socket. Logs append across restarts.

## Confirmed live transport cause

The installed original executable repeatedly accepted a peer's Hello but left 201 reply bytes
unacknowledged. TCP retransmission counters increased. The Mac closed the connection after
approximately five seconds. These sockets therefore appeared connected in Splice without
usable application traffic.

A temporary copy of the unmodified release executable maintained both peer connections and
received heartbeat replies. Its SHA-256 matched the installed executable:

```text
2a1040f66c26f537f0d4c4c96fef656e42f84634456a2e45a6ac634c0e36f7dd
```

The Welcome bytes also matched. Further trials kept the executable bytes constant and changed
its filename, SELinux label, inode path, and `argv[0]`. The installed path failed even with a
different `argv[0]`. An alternate hard link to the same inode worked.

The process cgroup revealed the difference. PIA placed the installed path in
`net_cls:/piavpnexclusions`; working paths remained in `net_cls:/`. PIA's persisted
`splitTunnelRules` contained an explicit `exclude` entry for
`/home/daniel/.local/bin/splice`.

The routing prediction was observable without packet capture:

```sh
ip route get 100.67.83.26 from 100.88.64.53
ip route get 100.67.83.26 from 100.88.64.53 mark 0x3211
```

The unmarked lookup used `tailscale0`, table 52. The PIA-marked lookup used the Wi-Fi gateway
through table `piavpnrt`. The bypass rule sent Splice's replies outside the Tailnet route.

Only Splice's PIA rule was removed through `piactl -u applysettings`. The existing
`/usr/bin/tailscale` rule was retained. Restarting the original installed Splice executable
placed it in the normal network cgroup and restored nonzero heartbeat measurements from
both peers. Both connections stayed healthy throughout a 60-second check. The user then
confirmed that all three computers appear and both edges work from KDE. The original
arrangement remained intact. No renamed executable or VPN bypass was shipped in Splice.

This correction does not deploy protocol 2. The installed original build still runs on KDE,
so the separate source improvements require a coordinated update on all computers.

## Verification

`cargo test --workspace` passes 126 tests. The separate live KDE test passes. Clippy passes with warnings treated as errors. Release builds
pass all 28 engine integration tests and all 10 network integration tests.

The three-machine test checks all six directed connections for 6.25 seconds. The five-machine
test checks all 20 directed connections, an arrangement edited from the fifth machine, middle
machine shutdown, persisted membership, and reconnection to the same arrangement.

These checks cover application behavior over loopback and edge mapping on this KDE desktop.
They do not replace a physical keyboard and pointer test across the three operating systems.
Only the Linux Rust target is installed, so macOS compilation and runtime behavior remain
unverified. The repository-wide default rustfmt check remains non-clean. New modules and edited regions are
formatted without reformatting the rest of the codebase. Backend preference resolution is tested, but runtime creation failure and retry
are not exercised with a simulated failing backend.

## Deployment constraint

Every machine needs the same protocol-2 build. Older clients are rejected with an explicit
upgrade message. There is no protocol downgrade or configuration migration. Automatic peer
authorization continues to require the same Tailscale user account. This change does not grant
keyboard or clipboard access to other users of a shared Tailnet.
