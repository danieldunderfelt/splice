# Clipboard integration

The ordered clipboard bridge is implemented in `engine/clipboard_clock.rs`, `files/clipboard.rs`, and the Mac and Linux native observers. See [the API contract](file-core-next-api.md).

A clipboard generation is assigned before asynchronous file inspection. The old local selection is invalidated immediately. A later result retains the original generation, so it cannot replace a newer copy. Splice's own publication only invalidates local file capture; it preserves the remote text/image provider and any newer remote file reference.

Receive-to-clipboard separately captures the local clipboard generation and a receive intent. Publication checks both immediately before writing the native clipboard. A successfully received copy remains retained when publication is superseded or fails.

Mac promise cancellation uses independent `PromiseCancellation` tokens. Per-root bookkeeping holds transfer IDs, not a second receive lease. The native writer owns the receive lease until its completion callback, so completed promises do not prevent Clear merely because AppKit still retains a provider. `DragRetired` removes attempt metadata after all providers and callbacks have finished.

Linux file selections resolve URI or local portal data into retained source descriptors. Portal handles and source path representations do not travel in ordinary shared clipboard data. Clipboard publication retains its portal export and received-file lease after the shelf closes.

Regression tests cover phase permutations, burst coalescing, capability release, queued generation suppression, disabled-sharing epochs, remote provider survival, and stale receive publication. Native desktop acceptance remains in [the server checklist](file-handoff-validation.md).

Linux native publication uses `ClipboardGuard` and a native acknowledgment. It rejects queued writes after generation, intent or cancellation changes. Cancelled providers stop serving data even after native acknowledgment; generation changes alone do not interrupt an ordinary source's already queued sends. Portal exports own cleanup from creation through every cancellation and failure path.

A portal session gap cannot reveal clipboard copies made while disconnected. Splice therefore removes automatic replay and requires a fresh offer after the observer is ready. Switching clipboard backends also invalidates cached offers and requires a fresh copy. Neither event authorizes replay over an unobserved local copy.
