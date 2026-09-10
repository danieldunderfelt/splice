# Re-review after fix commit 7fcd465 (2026-09-10)

Read-only. `cargo test -p splice-core -p splice-proto` passes in this worktree (62 + 4 + 14 + 16 tests, 0 failures), mock platform and loopback only.

## Prior findings, status

| # | Finding | Status | Evidence |
|---|---|---|---|
| 1 | Owner copy never routed to local controller | Fixed | `engine/inner/files.rs:182-189` maps `Focus::Local` to self; e2e "remote owner routes home" |
| 2 | Transfer records never evicted | Fixed | `service.rs:345-373` retire at 192 toward 128; `ClearReceived`; recovery clears pathless at 128 (`storage.rs:121`) |
| 3 | Drag leases pinned 24 h, global cap | Fixed | `service.rs:375-403` retire on terminal transfer, per-peer 8 |
| 4 | Full file queue killed control session | Fixed | `session.rs:582-584` drop inbound; outgoing file frames return false instead of close; tests in `net/file_tests.rs` and session tests |
| 5 | Accept error killed listener | Fixed | `transport.rs:116-125` backoff; test |
| 6 | Any Panic cancelled committed receives | Fixed | `commands.rs:209-227` only `transfer.is_none()`, remote scoped by peer |
| 7 | File MIME silenced text sync | Fixed | `inner.rs:1660-1661` filters file MIMEs, keeps text; `engine_e2e` test |
| 8 | Offers resurrect on reconnect | Fixed | `service.rs:638-658` revokes on generation/epoch mismatch; epochs in `sync_file_policy` |
| 9 | Remote ref ignored layout enablement | Fixed | `inner/files.rs:112-113` |
| 10 | Symlink dereference | Documented product limit | integration doc line 56 |
| 11 | Orphan staging dir | Fixed | `storage.rs:187-204` reclaim empty owned 0700 stage; tests |

## Remaining findings

### A. Medium. Recovery is fail-stop: one bad or unremovable journal disables file sharing until manual cleanup
`storage.rs:78-88` and `:120`, `:125`: any parse error, `cleanup_staging` error, or `clear_received` error inside `initialize` propagates and `service::run` returns Err, so the whole service reports disabled. Concrete path: `ClearReceived` on a cache receipt whose file was locked in Finder (UF_IMMUTABLE) writes `clearing: true` (`storage.rs:236-237`) then fails at `remove_dir_all`; `Completed::Cleared` marks the Ready receipt Failed (`service.rs:437-440`); every restart retries the clear, fails, and no file sharing runs. Fix: quarantine a journal that fails to load or clear (rename to `<id>.json.bad`, mark the record Failed with the error) and continue; do not flip a Ready receipt to Failed when clearing fails, keep it Ready with an error string.

### B. Medium, interface. Engine cannot tell a native file copy's own ClipboardChanged from a newer copy
`inner.rs:1662` clears the file clipboard on every `PlatformEvent::ClipboardChanged`, and that event carries no generation (`splice-platform/src/lib.rs:152`). If the adapter calls `capture_file_clipboard` before the poller emits the ClipboardChanged for the same native change count, the capture is immediately revoked. The contract does not state the required order. Fix: add `generation: Option<u64>` to the platform event and skip the clear when it equals the captured generation, or state in the contract that the adapter must emit ClipboardChanged before capture for the same generation.

### C. Low. `file_route` dedupe suppresses re-offer after reconnect invalidation
Offers are revoked on reconnect by epoch (`service.rs:647-648`), but the controller keeps `file_route == (stamp, recipient)` (`inner/files.rs:194,219`), so crossing to the same recipient again never re-sends `FileRoute`. The owner's `RouteClipboard` is already idempotent (`commands.rs:92-103`). Fix: clear `file_route` on `PeerEvent::Disconnected` or on any epoch change, or drop the dedupe and rely on owner-side idempotence.

### D. Low. File control frames share the outgoing bulk queue with ClipChunk and fail rather than wait
`session.rs:64,84-89`: during a large text/image clipboard transfer the bulk queue can be full; `Commit`, `Grant`, `Dropped` then return "file control queue unavailable" (`service.rs:219-222`) and the user action fails. Fix: a separate small bounded channel for file control frames, or spawn a `send_to_wait` with a short timeout for file control from the service.

## Product and interface gaps (documented, not defects)

- Symlinks are materialized as targets; permissions, executable bits, timestamps not preserved (doc line 56).
- Per-peer active transfers are 2 and prepared leases 8; concurrent AppKit promise writers beyond that fail rather than queue. Adapter must use a serial operation queue, or one multi-root drag with a single commit. Worth stating explicitly in the contract.
- Global 64 source-handle budget: a selection spanning more than 64 distinct parent directories fails.

## Verified new behavior

- Capability ownership: `SelectedRoot::Open` uses the descriptor as the capability, symlinks resolved descriptor-relative (`manifest.rs:175-242`), access carrier released on cancel and terminal (tests at `manifest.rs:561-615`, e2e "engine clipboard source access released").
- Receipt pins: `retain_received` weak/strong pin, clear refused while pinned or clearing, pins dropped with record (`service.rs:263-343`).
- Compaction: retire sends and pathless receives, never pinned or path-bearing receipts; recovery mirrors it.
- Cancellation/retry: cancel flag polled every 25 ms in `Guard::protect`; retry issues new id, token, staging and revalidates source; zero-byte and disable tests still pass.
- Queue/listener isolation: control session survives 100 dropped file frames; listener survives EMFILE.
