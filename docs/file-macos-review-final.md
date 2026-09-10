Final Fable review `run_mtvslkmy4344797b36` is satisfied with the integrated Mac and clipboard implementation. No interactive tests were run.

# Final bounded verification: macOS native file adapter (root working tree, 2026-09-10 ~20:20)

## Context

Third pass over the current root working tree after core db12ecd integration and the parent promise
rewrite. Read-only. Checks: `cargo check -p splice-platform --all-targets`, `cargo check -p splice-app
--all-targets` (clean, pending method now present), platform pure unit tests (44 passed, `file_types`
skipped), core `clipboard_clock` and `files::clipboard` unit tests (7 passed), and the two mock-engine
e2e tests `ordered_clipboard_bridge_keeps_own_capture_and_rejects_late_resolution` and
`own_publication_invalidation_preserves_remote_providers_and_newer_file_reference` (2 passed). No GUI,
native, pilot, clipboard, input, or app execution.

## Prior findings: resolved

**P2 lease pin.** `crates/splice-app/src/file_shelf/promises.rs:10-15` `RootPromise` now holds only the
drag id, transfer id, cancellation token, and a `requested` flag. `requested` (`:60-87`) rejects a second
request per root, and the sole `ReceivedLease` moves into the native writer's `PromiseSource`; the native
callback drops it right after the completion handler (`macos/promise.rs:101`). Attempt metadata is removed
on `DragRetired` or a Cancelled end. Core `clear_received` sees zero pins once the parent bridge's own pin
is removed on Clear, so Clear on a promised-root receipt no longer fails.

**Core invalidate semantics.** `engine/clipboard_clock.rs` orders phases Invalidated < Changed <
Captured < Replaced per generation; `accept` admits strictly higher stamps, so Invalidated{g} then
Changed{g} then Captured{g} all land, while a Changed{g} after Captured{g} is refused. Own invalidation
(`engine/inner/files.rs:18-19, 53-66`) retires only a self-written `FileClipboardRef` and clears the
file-service capture; it never touches `clipboard_offers` or `pending_fetches`. Remote replacement uses
`replace_pending()` so the latest submitted generation is marked Replaced, not just the consumed one.
Both e2e tests above pin these behaviours.

## Mac ordering against the integrated core

- Poller: Invalidated{g} before any read, then ClipboardFiles{g} or Changed{g}, each recheck-guarded.
  Own writes emit Invalidated only. Native outbox coalesces Invalidated/Changed in one slot
  (`macos/files.rs:123`), delivered ahead of queued captures; test
  `latest_clipboard_content_replaces_its_pending_invalidation` covers the same-generation replacement.
- Bridge (`file_shelf/macos.rs:61-85`) forwards only when the live change count still equals the event
  generation; stale events are dropped before the core, and the core clock rejects them regardless.
- Epoch binding: a capture submitted after a disable keeps the epoch of its generation's first event
  and is discarded by `native_clipboard_event` with `invalidate_pending`, so a slow capture cannot
  resurrect a revoked selection.
- Reservation capacity doubled (`RESERVATION_CAPACITY`), closing the earlier note about a cancelled
  maximum drag blocking its replacement.
- `shelf.rs` change is a recipient popup placeholder at index 0 with index-1 mapping and duplicate
  hostname disambiguation; consistent in both directions.

## Remaining defects

None that are concrete on the Mac or clipboard path.

## Residual notes (not defects, no action requested)

- First local clipboard change after startup may broadcast twice: once through the legacy untagged
  path before the clock is ordered, once through `clipboard_changed`. Harmless duplicate ClipOffer.
- `ended(Cancelled)` removes the attempt before `DragRetired`; a late request then gets a clean
  "no longer available" error, which is the intended outcome.
