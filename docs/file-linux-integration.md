# Linux file shelf and FUSE integration

`splice-files` supplies the GTK shelf and a read-only FUSE view over incoming offers and retained receipts. The application links this crate and runs the shelf in its internal process mode. The standalone development binary is not part of production packaging.

## Local protocol

The helper protocol is version 4. It is local IPC, separate from network protocol 7 and the `files-v2` capability. Records are bounded to 64 KiB. Offers, receipts and on-demand Reveal paths are paged under 48 KiB with a `more` flag. Display summaries keep the true root count, at most three abbreviated names and a bounded origin label. Names shown in the UI cannot introduce new label lines. Full transfer manifests retain the original names.

The helper sends source drops, preparation and release requests, drag lifecycle events, explicit receive/save actions, receipt copy/reveal/clear actions, cancellation, retry and dismissal. The service responds with peer and row snapshots, view readiness, progress and errors. `ReleaseView` releases an unused prepared row. There is no received-row SaveReceipt operation; received files can be copied, revealed or dragged locally.

The service emits Connected before it permits the socket reader to deliver Hello. Invalid framing and descriptor counts close the connection. Received descriptors become owned handles before any zero-length-record handling, so malformed packets cannot leak them. Lifecycle traffic is admitted independently of the new-work limit. If an outbound queue cannot deliver it, the helper disconnects visibly instead of silently losing drag state.

## Deferred views

A view has a fresh ID and one drag gesture. Metadata is available before a drop, including modes, sizes and modification times. Pre-drop content reads fail with EIO. DropPerformed opens the native gate; the first subsequent content read waits for the corresponding core acknowledgement and commits once. Cancelling before content is requested sends no bytes.

Prepared views are bounded and acquired on demand. Idle prepared rows are released, and later rows remain reachable. A dropped view without content expires after 24 hours. Receipt content and completed view IDs are durable until explicit Clear. Shelf exit and engine detach retire transient views while preserving completed receipt history.

Clear coordinates view retirement with FUSE opens under one lifecycle lock. Existing readers prevent clearing. A failed core Clear restores durable views and receipt ownership. Negative and fractional Unix timestamps are preserved, including timestamps between 1969-12-31 23:59:59 and the Unix epoch.

## Host requirements and checks

Host builds require GTK4 4.10 or newer, FUSE3 libraries and the mount utility. Deferred FUSE drags are not supported by the current Flatpak sandbox configuration. See [packaging](file-packaging-integration.md), [the service contract](file-linux-app-integration.md) and [acceptance](file-handoff-validation.md).

The server's extracted development SDK is `/tmp/splice-files-linux-kimi/devel/root`. Noninteractive compilation uses its pkg-config directories and library search path. It does not install packages or modify the running application. Unit and disposable FUSE tests cover metadata, pre-drop denial, commit-once behavior, cancellation, reader retirement and IPC framing. Real GTK drag/drop and native portal behavior remain for the separate server acceptance session.
