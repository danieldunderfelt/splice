# Raw timing repair and validation

Protocol 5 preserves native capture timestamps for Mac and Linux sources and maps them into
Linux's monotonic clock for immediate uinput injection. Report order and counts remain exact;
there is no replay timer or smoothing. Mouse identity, acceleration, DPI, TCP, and Tailscale
routing are unchanged. Both endpoints must run protocol 5.

## Timing contract

Mac IOHID supplies Mach report-arrival ticks. The native callback and transport share the same
Mach timebase conversion. Linux selects CLOCK_MONOTONIC for each evdev reader, including reopened
devices, and preserves SYN_REPORT timestamps. Synthetic held-state snapshots use current time.
Capture enqueue time is separate local metadata; native timestamps do not control sequence validation.

The receiver observes send/receive clock differences for reports and heartbeats. It uses the
minimum difference from the current and previous ten-second windows to map native timestamps.
Four initial heartbeat samples precede capture. The estimate includes an unknown fastest transit
floor; it does not measure absolute one-way latency or assume symmetric network paths. Rolling
windows track drift, and every packet supplies a causal upper bound before mapping.

The persistent virtual mouse and keyboard each retain their last emitted timestamp across sessions.
Before emission, timestamps advance by at least the progression of native capture time when
current time permits, so an earlier clock estimate does not flatten a run of reports. A native
high-water mark prevents interleaved older devices from adding elapsed time twice. Timestamps
are bounded by the last emitted value, the preceding two seconds, and current monotonic time.
Only native progression is reset for a new session; the last emitted timestamp stays with the
device. Timestamp adjustments are measured. All events in a batch share its timestamp;
Linux retains it for the SYN_REPORT appended by evdev. Teardown releases use current time.

Source capture age and receiver mapped age have a 750 ms limit. Expired input ends the session
and releases held state rather than playing back a stale queue. The receiver's estimate excludes
the unknown transit floor. The 1,024-report count limit and existing I/O/heartbeat deadlines also
remain enforced. Accurate timestamps cannot make a late packet arrive earlier.

## Reading the measurements

The normal Splice log includes `raw input timing` entries every ten seconds of activity and at
session end. Each entry has a `scope` and a JSON `measurements` field. No key codes, mouse values,
or clipboard contents appear in these measurements.

| Scope | Measurements | Interpretation |
|---|---|---|
| `send` | `capture_to_enqueue`, `source_queue`, `socket_write`, `heartbeat_rtt` | Local capture/decoding delay, source backlog, TCP write cost, and round-trip time |
| `receive` | `transit_above_floor`, `mapped_capture_age`, `receive_gap`, `receive_to_inject`, `inject_duration` | Delivery variation, estimated event age, arrival cadence, and target processing |
| `uinput` | `injected_age`, `write_duration`, `timestamp_adjustment` | Actual kernel-facing timestamp age, emit cost, and amount of timestamp bounding |

Every distribution includes count, mean, maximum, and p50/p95/p99 upper bounds in microseconds.
Percentile bounds use logarithmic buckets; they are not exact quantiles. Counts for
`timestamp_adjustment` count adjusted output batches. Keyboard and pointer batches are separate.
Idle gaps are included in receive-gap statistics. RTT includes both directions and peer scheduling.

High capture-to-enqueue time points toward source scheduling or decoding. High source queue time
points toward sender backlog. High transit-above-floor and RTT tails with low local costs point
toward delivery or remote scheduling. High injection/write costs identify target processing.
A constant network delay is part of the unknown floor and cannot be recovered from these clocks.

## Automated evidence, 2026-09-06

The accompanying lifecycle and migration fixes have regressions:

- Mac HID failures retain the operation they belong to and release capture under the capture-state lock.
  Discovery health remains in readiness state; an idle discovery error cannot terminate another session.
- Mac queries the current HID-system button state when Desktop capture starts, including a button
  held before the tap existed and releases missed during a tap interruption. Its snapshot therefore
  does not depend on a history of observed button edges. Keyboard snapshots keep their existing path.
- Linux raw injection records keyboard and button edges, including teardown releases, for remapper
  echo suppression. This matches the Desktop accounting path.
- Nonzero legacy dwell settings migrate into the supported 50–5,000 ms range; zero stays Immediate.
  Existing `input.json` takes precedence and invalid new settings remain errors.

The production transport regression imposes a 100 ms receiver stall while a separate source
thread generates reports. It checks report counts, preserved native timestamp span, bounded
mapped timestamps, and session cleanup. Clock tests cover positive and negative uptime offsets,
variable delay, drift, idle samples, window rollover, and fresh sessions. Ledger tests cover
interleaved device timestamps with strict global sequence checking.

Native Linux checks on `gamedev` passed using exclusively grabbed generated fixture pointers:

- Capture retained an evdev timestamp supplied 100 ms before reading, independently of enqueue time.
- The actual relative injection path retained a 100 ms-old timestamp through REL events and SYN_REPORT.
- Button press and teardown release were recorded for remapper echo suppression.
- Teardown used current time and the next session retained the virtual pointer's timestamp bound.
- Switching between source epochs far ahead and behind the previous source preserved the supplied
  mapped timestamp, including sessions that ended with no held inputs to release.

The destination remapper already owned the generated keyboard, so the emission test deliberately
uses only its isolated pointer. It does not inject keyboard events or alter remapper settings.
The combined capture fixture is also isolated before emitting input. These tests do not establish
physical mouse-to-display latency, keyboard forwarding, or perceived smoothness.

An initial release transport probe preserved all reports at 125/500/1000/8000 nominal Hz. In the
1000 Hz stall case, about 100 reports were delivered in a burst, while their mapped timestamp span
remained exactly 99.004 ms, equal to the native span. No paced replay was added. This run overlapped
other validation work, so its tail latency is not a clean before/after performance comparison.

A repeat after compilation measured 1,000 Hz loopback p50 62 microseconds and p99 233 microseconds.
Its forced-stall sample preserved exactly 98.997 ms of native spacing in the mapped timestamps.
These are isolated transport measurements, not hardware-to-display latency or matched VirtualHere
measurements. The benchmark's source generator also experienced scheduling outliers.

The signed Mac app reported version 1.2.0, protocol 5, and a dirty checkout. Its designated
requirement matches the installed Splice Dev app. A five-second Launch Services HID probe received
209 reports from the USB receiver's keyboard collection, with no decoding or timestamp errors and
maximum callback delay 319 microseconds. Other collections were idle during that probe. It neither
suppressed local input nor established physical mouse forwarding.

The full Mac workspace test run passed 222 tests with one native test ignored. Strict workspace
Clippy passed on Mac and Linux. The Linux workspace's input and application tests passed; two
updater tests hit `Text file busy` under parallel execution, and all 25 updater tests passed when
rerun serially. The updater source is unchanged. Linux Clippy initially exhausted the temporary
build-cache quota; a fresh cache on the regular filesystem completed successfully.

After review changes, the affected core, platform, and protocol suites passed again: 178 tests on
Mac with one ignored, and 191 on Linux with seven ignored. Both native Linux timing tests and
strict workspace Clippy passed again. The additional native source-epoch regression also passed.
The final Mac release-mode engine suite passed all 45 tests, and all five packaging tests passed.

Claude Fable 5.1 and Kimi K3 reviewed the implementation through Baton and approved the production
changes. Review caught the clock-estimate backstep issue, missing teardown activity accounting,
and gaps in receiver-expiry and clock-rollover coverage; these were corrected and rechecked.
One later claim about missing session reset was retracted after checking the existing `begin()`
reset, and the native epoch-switch regression now guards that behavior. Review approval covers
the code; it does not establish physical performance parity.

Local probe sources and detailed logs are under `build/raw-input-review-evidence/`. They are ignored
artifacts; the production regressions and native tests are in the repository.

## Reviewed builds

The final reviewed sources produced `build/Splice.app` for Apple Silicon and
`build/splice-linux-x86_64` for Linux. Both report version 1.2.0, protocol 5, and dirty commit
`d603bc2f7ad661ccba7e0f7e2ee189fb8eb93e5a`. The Mac bundle passes strict signature verification
with the existing Splice Dev identity. These are local test builds, not a published release.
Installed apps remain unchanged. Install both endpoints together before testing; protocol 5
refuses protocol 4 peers.

## Physical acceptance

Use the same mouse and fixed polling/DPI settings for VirtualHere and Splice, on the same desktop
and network path. User-selected mouse settings may differ; matching physical-device settings is
not required for this repair. Compare movement while idle, during simultaneous typing, during
clipboard transfer, and while switching computers. Collect timing summaries from both endpoints.

Physical comparison remains necessary before claiming this matches VirtualHere. If local timing
costs are low but delivery dominates, first evaluate a direct LAN data connection authenticated
through the Tailscale control session. Evaluate UDP with explicit ordering, loss recovery, held-state
release, and authorization only if the TCP measurements justify it. Neither transport change is
part of this repair.
