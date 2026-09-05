# Diagnose a Splice connection

1. Open **Diagnostics** in the Splice window.
2. Expand the affected peer.
3. Check its connection phase, last heartbeat, and last error.
4. Compare the version, commit, and protocol on the two computers.
5. Select **Save diagnostic report** to save the current state.
6. Use **Copy report path** to locate the report.

Reports are JSON files under `diagnostics` in Splice's configuration directory. Splice creates each
file with permissions `0600`. The panel reports write errors and displays the path on success.

Reports contain machine names, Tailnet addresses and identifiers, layout, backend status, build
identities, connection health, traffic counters, and update status. They exclude clipboard payloads,
typed keys, held-key state, and credentials. Review the machine and network information before sharing.

Connection attempt and disconnect counts cover the running engine's lifetime. Traffic measurements
cover the current or most recent connection. **Longest input queue wait** measures the delay before an
input frame reaches the socket writer. **Longest socket write** includes every frame type. Neither is
a measurement of end-to-end pointer latency. **Last error** remains visible after a successful reconnect.

The automated motion regression test measures delivery over real loopback TCP connections during four
concurrent 8 MiB clipboard transfers. It checks that all payloads arrive intact, the connection stays
healthy, the 95th percentile stays below 50 ms, and the slowest sample stays below 250 ms:

```sh
cargo test -p splice-core --release --test engine_e2e motion_latency_stays_bounded_during_concurrent_large_clipboard_transfers -- --nocapture
```

The separate input queue prevents queued clipboard chunks from delaying keys and pointer motion.
This test measures the transport under local load. Network latency and the desktop input backend add
their own delay on physical computers.

Clipboard frames are limited to 16 KiB. A separate regression uses a throttled 128,000-byte-per-second
writer and queues input after a clipboard write has started. It requires delivery within 350 ms.
Priority applies before each frame write. It cannot preempt bytes already in TCP or remove network
congestion. Socket writes have a deadline and failed connections report their errors.
