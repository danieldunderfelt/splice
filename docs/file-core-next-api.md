# File metadata and native clipboard API

These APIs are implemented. [The core contract](file-core-integration.md) describes commands, state and receipt ownership.

`ManifestEntry` carries ordinary `mode` bits and `modified: FileTimestamp { seconds, nanos }`. `EntryKind::Symlink { target }` represents a validated relative link inside a selected folder. Links have no payload, and native adapters must not dereference them to request content. The file capability is `files-v2`, under protocol 7.

Native clipboard observers use one increasing generation clock for every genuine change, including ordinary text, images and an empty clipboard. Assign the generation before asynchronous selection inspection. Submit through these synchronous `EngineHandle` methods:

- `invalidate_file_clipboard(generation)` retires earlier local file capture before inspection, and reports Splice's own clipboard publications without destroying the remote ordinary clipboard provider.
- `clipboard_changed(generation, mimes, inline_text)` reports a genuine ordinary or empty change.
- `capture_file_clipboard(selection, generation)` retains the selected roots and their native access capabilities.

The latest pending notification is coalesced at both the engine and file-service boundaries. Older generations and duplicates have no effect. Within one generation, invalidation precedes ordinary change, which precedes captured files. Captured files suppress that generation's ordinary pathname representation. There is no full-queue error to retry. Invalid arguments and a stopped engine remain errors.

Once an observer uses the ordered bridge, legacy untagged clipboard events are ignored by the engine. Do not mix legacy file-selection commands with ordered events. Own-write invalidation preserves a newer remote file reference and remote text/image fetch handles. A policy disable invalidates pending capture even across a brief disable and re-enable.

`FileState` and `FileSummary` expose recovery diagnostics independently of the enabled flag. Show these errors and keep intact receipts accessible. Core stores up to 32 diagnostics, with messages capped at 1,024 UTF-8 bytes in full state and 96 in summaries. `recovery_error_count` includes omitted diagnostics.

`ReceivedLease::manifest() -> &Manifest` exposes the retained transfer's own validated metadata. Use it when restoring or discovering completed receipts after the original offer is gone. The metadata shares ownership across lease clones and is local only; ordinary UI snapshots remain compact. Parent has implemented this API in the root checkout.
