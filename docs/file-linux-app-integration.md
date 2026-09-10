# Linux file service integration

The background service owns clipboard capture, file transfers, the retained-receipt journal and the FUSE mount. Opening or closing the GTK shelf does not change clipboard monitoring or completed transfer ownership.

## Process and native clipboard

`splice files` sends an IPC request to the running service. The service opens `/proc/self/exe file-shelf`, so the shelf uses the running service's executable image even after an atomic update. The private `SOCK_SEQPACKET` connection validates peer credentials, protocol version and descriptor counts. Production packages contain one executable. See [packaging](file-packaging-integration.md).

`file_service/clipboard.rs` wraps the existing native backend. Local changes invalidate the clipboard clock immediately, before asynchronous content inspection. Empty selections also invalidate it. Queued native publications recheck their generation, user intent and cancellation guard. A cancelled provider refuses subsequent fetches. Retired ordinary providers can finish already queued sends.

Portal export ownership begins at StartTransfer and ends exactly once after cancellation, failure or release. File publications pin the received files while the native provider can serve them. After a portal session outage or clipboard backend switch, the user must copy again. Unobserved local changes during the gap make automatic replay unsafe.

## Offers and receiving

The recipient menu starts with "Choose a computer". Selection uses the stable peer ID. If that computer disappears, the menu returns to the placeholder. It never silently retargets a drop.

Source drops enumerate descriptor-backed selections. Preparing and hovering over an incoming offer expose metadata only. A real drop followed by a fresh content read authorizes a transfer. A read made before the drop fails immediately and cannot become permission for a later transfer.

Receive to clipboard retains and journals the verified files before deciding whether its clipboard intent is still current. A newer local copy only suppresses publication. Save to runs the core directory receive with no-overwrite publication. Retried cache receives track the new transfer ID returned by core.

The helper prepares views on hover or drag demand, retains at most six idle prepared rows and releases them after 90 seconds. The service can retire the client's oldest armed view when necessary. Preparing an earlier row does not make later rows permanently unreachable. Drag lifecycle messages are control traffic. Outbound overload disconnects the shelf with a visible error and releases its transient state.

## Durable receipts

`file_service/content.rs` owns retained leases and `journal.rs` records receipts and their completed view IDs. Receipts load as Unavailable until the current engine re-pins them. Engine incarnations prevent stale attach or restore results from mutating current content. Detach releases leases without deleting journal history.

Ready cache receives missing from the shelf recover from `ReceivedLease::manifest()`. Recovery does not require the original offer to remain in memory. Receipt actions use the retained files and do not contact the original source.

Dropped receipt views survive shelf closure and service restart. Active readers pin their view. Clear first retires the receipt's views atomically against new opens, then releases the lease and asks core to clear. Only a successful core acknowledgement removes the journal record. Failure restores the views, re-pins remaining files and exposes the error. The row reports Clearing or Unavailable while an operation or recovery is pending.

The journal allows 128 receipts and does not evict them. Missing or invalid durable storage disables the bridge visibly. It never substitutes an in-memory journal or truncates damaged history. Receipt summaries are rebuilt only when their revision or clipboard publication changes.

## Native acceptance

The GTK shelf and real clipboard/portal interactions have not been exercised by this implementation session. Confirmed mock tests and disposable FUSE tests can run without moving the pointer. Use [the acceptance checklist](file-handoff-validation.md) with the separate server agent.
