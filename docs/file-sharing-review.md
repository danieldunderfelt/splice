# Read-only review: Splice file transfer core (2026-09-10)

## Context

Review of the in-progress file-offer and transfer service in the `core` worktree against
`docs/research/file-transfer-and-drag-drop-plan.md`. No files were edited. Unit tests
(`cargo test -p splice-core -p splice-proto --lib files`, 14 tests) and the mock-platform
end-to-end suite (`cargo test -p splice-core --test files_e2e`, 2 tests) pass on this Mac.

## Findings, severity ranked

### 1. High. Remote owner copy never routes back to the local controller
`crates/splice-core/src/engine/inner/files.rs:165` returns from `route_file_clipboard` whenever
focus is `Focus::Local`, and `remote_file_route` on the owner (line 128) only fires when asked.
Scenario: A controls B, user copies files on B, crosses back to A. A has the opaque ref
(writer B) but sends no `FileRoute`; B sees `claim.writer == A` so its own router exits at
line 161. No offer ever reaches A. Fix: when focus is `Local` and `reference.stamp.writer !=
self`, send `FileRoute { recipient: self.self_info.id }` to the owner, and track it in
`file_route` like the remote case. Owner-side checks already accept `recipient == from`.

### 2. High. Transfer records are never evicted; service hard-stops after 256 lifetime transfers
`crates/splice-core/src/files/service.rs:228` refuses every commit once `transfers.len()`
reaches `storage::MAX_RECORDS`. There is no `transfers.remove` anywhere, no Dismiss/Clear
command, and failed drag commits and sends count too. `storage.rs:66` also fails `initialize`
outright if more than 256 journals exist on disk, which would kill the service at startup.
Fix: retire terminal send records and terminal receive records with empty `paths` when the
limit is approached (mirror the `offer_capacity` retain), add an explicit clear-received
command that deletes the journal and cache directory, and make recovery skip or archive
excess journals instead of failing.

### 3. High. Drag leases pin source selections for 24 h after a successful transfer
`service/control.rs:81` (source) and `service/commands.rs:312` (recipient) create leases
with `expires = now + OFFER_LIFETIME_MS`; nothing retires a lease when its transfer reaches
a terminal state. `MAX_DRAGS` is a global 32 shared across peers. `docs/file-core-integration.md`
tells macOS to prepare each root separately, so a 40-file Finder drag, or 32 completed drags
in a day, exhausts the pool with "native drag lease limit reached", and each source lease
keeps open directory descriptors. Fix: remove the lease (both sides) in `Completed::Sent`,
`Completed::Received` and `Result`/`fail`, or make `CancelDrag` mandatory post-completion
in the contract and document it. Consider a per-peer cap.

### 4. Medium-high. A full file control queue tears down the input session
`crates/splice-core/src/net/session.rs:577` breaks the session loop with "file control queue
unavailable" when the 32-slot `Bridge` wire channel is full or unset. A burst of 33 file
frames from an authorized peer disconnects the control link and therefore input. Ordinary
frames use the unbounded events channel (line 588). Fix: drop or `Reject` the file frame and
keep the session alive; never let file traffic close the control connection.

### 5. Medium-high. One transient accept error permanently disables file sharing
`crates/splice-core/src/files/transport.rs:103` breaks the listener loop on any `accept`
error (EMFILE, ECONNABORTED). The `accepted` channel then closes and `service.rs:134` bails
with "file listener stopped", cancelling all transfers with no restart path until the engine
restarts. Fix: log, sleep briefly, and continue on accept errors.

### 6. Medium. Any peer's Panic cancels every local native drag and its accepted receive
`crates/splice-core/src/engine/inner.rs:1286` sends `CancelDrags` on `Frame::Panic` from
any peer; `commands.rs:157` then cancels committed drag transfers, including ones with a
third machine. The plan says Panic revokes uncommitted attempts and focus changes keep
accepted transfers running. Fix: on Panic only retire leases with `transfer.is_none()`
(or dropped-but-uncommitted); keep committed receives.

### 7. Medium. Text clipboard sync silently stops whenever a file MIME is present
`crates/splice-core/src/engine/inner.rs:1647` and `:1672` return before any handling when
`is_file_mime` matches. On Linux `text/uri-list` accompanies many ordinary copies (browsers,
Dolphin link copies). Those copies are no longer synced as text and the stale file ref is not
cleared either. Fix: keep the text path and only skip the file-bearing representations, or
clear the file clipboard and defer to `FileClipboardSelection` for the file part.

### 8. Medium. Offers to a disabled or disconnected peer resurrect on re-enable/reconnect
`service.rs:474` rewrites `offer.generation` to the peer's new connection generation on every
enforce, and `enforce` only revokes offers on local disable or clipboard-off. An offer made,
then the peer disabled in the layout or its master turned off, becomes usable again later
without a new user action. Plan row "authorization revoked" asks for invalidation. Fix: revoke
offers whose peer leaves `policy.peers` rather than migrating their generation.

### 9. Low. `remote_file_clipboard` ignores layout enablement
`inner/files.rs:99` accepts a `FileClipboardRef` from any connected FILES_V1 peer, even one
disabled in the layout, and clears the local file clipboard and revokes local unaccepted
clipboard offers in response. Add `machine_enabled(from)`.

### 10. Low. Symlinks are dereferenced, not recreated
`manifest.rs:325` replaces a confined symlink with its target path, so the receiver gets a
copy of the target content under the link name (e2e test asserts this). The plan says create
links without following. Safe, but a directory symlink to a sibling duplicates content up to
`MAX_ENTRIES`. Document the deviation or emit a `Symlink` entry kind later.

### 11. Low. Orphan staging directory if crash lands between the two journal saves
`storage.rs:214` saves without `stage_identity`, then `mkdir`, then saves again. A crash in
between leaves `.splice-<id>.partial` in the user's destination forever. Save identity once,
after mkdir, and treat a missing identity as "nothing to clean".

## Invariants verified (tests pass on this machine)

- Zero payload bytes on offer, hover, prepare, drop-without-read, cancelled drag, and
  forged bulk handshake (`files_e2e.rs:213-275`, `offers.rs` grant tests).
- Bulk connections are Tailnet-authenticated, token-bound to peer, generation, transfer and
  entry set, single-use, expiring (`transport.rs:76-157`, `offers.rs:38-66`).
- Disconnect or policy change fails live grants and stops source bytes (`files_e2e.rs:572-590`,
  `Guard::protect` 25 ms recheck).
- Explicit no-overwrite publication via `RENAME_NOREPLACE`/`RENAME_EXCL`, per-root results,
  crash recovery, cancelled/corrupt/truncated/extended streams never publish
  (`storage.rs` and `transport.rs` tests).
- Source mutation, replacement, parent-swap, escaping symlink, FIFO and cycle rejection
  (`manifest.rs` tests, `files_e2e.rs:422-438`).
- Clipboard: owner routes directly to third machine, controller never sees names or bytes;
  replaced clipboard revokes unaccepted offer but keeps the dropped lease
  (`files_e2e.rs:603-685`, `731-901`).
- Retired offers do not count toward capacity: `offer_capacity` prunes non-Available records
  when full and per-peer count is Available-only (`service.rs:245-269`). Parent's suspicion
  is not current code.
- Disabled policy persists fail-closed and survives restart (`storage.rs:488-520`,
  `files_e2e.rs:687-727`).
- Manifest bounded at 256 KiB postcard, 4096 entries, 128 roots, depth 64; UI records omit
  manifests for transfers (`proto/files.rs:65-111`).

## Integration status

This is the first review, not final approval. Fixes are in progress in Baton run `run_mtvod4ye0b04d71d45`. Native integration will receive a separate review.

The parent reproduced finding 7 after integration: `ordinary_clipboard_cannot_serve_file_urls_or_portal_keys` fails because the safe text representation never arrives. The same test passed before the initial core implementation was integrated.
