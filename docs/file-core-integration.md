# File transfer core contract

`splice_core::files` owns metadata, authorization, transport, receipt storage and local file-sharing policy. Native adapters own file selections, clipboard publication and drag gestures. The public types are in `crates/splice-core/src/files/mod.rs` and `crates/splice-proto/src/files.rs`.

## Commands and ownership

`EngineHandle::files()` returns a `FileHandle`. `request(command)` returns an asynchronous reply; `send(command)` is a fallible nonblocking enqueue. Both use the bounded command queue. Input handling never waits for a file transfer.

| Command | Meaning |
|---|---|
| `Offer { paths, recipient, origin }` | Enumerate local selected roots and send metadata to the chosen recipient. |
| `Receive { offer, destination }` | Explicitly accept a copy into `Cache` or `Directory(path)`. Returns a transfer ID. |
| `PrepareDrag { offer, entries }` | Authorize selected manifest roots for one native attempt. Metadata only. |
| `DropDrag { drag }` | Record an accepted copy drop. No payload starts. Await its acknowledgment. |
| `CommitDrag { drag, destination }` | Start the accepted drop's copy when the native destination requests content. Returns the same transfer for repeated calls on a live attempt. |
| `CancelDrag { drag }` | Retire the attempt and cancel its active receive. The shelf offer remains available. |
| `Cancel { transfer }` | Stop the selected transfer. |
| `Retry { transfer }` | Start again with a fresh grant and staging area after a new user action. |
| `Revoke { offer }` | Retire an unaccepted offer. |
| `ClearReceived { transfer }` | Clear the retained receipt when no consumer holds a lease. External Save results remain untouched. |
| `SetEnabled(enabled)` | Persist the local policy. Disabling cancels active work and invalidates pending native capture. |

For native access capabilities, call `offer_local(LocalSelection, recipient, origin)`. `SelectedRoot::Open { name, file }` retains a selected descriptor when a portal path may disappear. `SelectedRoot::Path` is for ordinary local selections. `LocalSelection::access` holds native access grants, such as a security-scoped URL lease. Accepted copies retain source access independently of later clipboard replacement.

`wait(transfer)` returns verified, durably published `ReceivedFiles { transfer, paths }`. `retain_received(transfer)` returns a cloneable `ReceivedLease`. Keep that lease while advertising native URLs, serving a FUSE view, publishing files to the clipboard or writing a native promise. `ReceivedLease::manifest()` supplies the transfer metadata even after its original offer has disappeared. Dropping all leases permits explicit Clear. There is no timeout-based deletion of completed receipts.

A pre-drop content request fails immediately. It cannot become authorization later. Hover, pickup and metadata inspection never start a content stream. Native AppKit promise callbacks already represent an accepted drop; the bridge prepares, records the drop, commits, receives into cache, and writes to the exact URL provided by AppKit.

## State and UI

`FileHandle::state()` exposes full `FileState`; `summary()` and `UiState::files` expose compact `FileSummary`. Full manifests stay out of ordinary arrangement UI snapshots. Summary offers include at most eight root descriptions plus total root and entry counts.

Transfers progress through Preparing, Committed, Receiving, Verifying and Ready, or terminate as Cancelled or Failed. A Save publishes roots separately without overwriting existing names. If a later root fails, earlier published roots remain listed in `paths` and must remain revealable.

Recovery diagnostics are separate from the enabled flag. Up to 32 messages are retained, with bounded message lengths and a total diagnostic count. Show these failures while preserving access to intact receipts. Do not treat a corrupt or unsupported journal as an empty store.

## Metadata and validation

Each manifest entry has an ID, optional parent ID, basename, kind, ordinary mode bits and `FileTimestamp { seconds, nanos }`. Kinds are File, Directory and Symlink. Symlinks carry a relative target and zero payload; their complete graph must resolve inside the selected root. Top-level symlinks, dangling or escaping links, special files and ambiguous case/Unicode-normalized names are rejected. Privileged mode bits are stripped.

Limits are 128 selected roots, 4,096 entries, 64 directory levels, 256 KiB of serialized metadata and 1 TiB of offered payload. The retained cache has a separate 64 GiB limit and 256 receipt-record limit. Completed data is not automatically evicted. Native adapters can impose additional admission bounds, with a visible error.

Ordinary permissions, executable bits and supported modification timestamps are preserved. Ownership, ACLs, extended attributes, resource forks, Finder tags and complete application-bundle metadata are outside this format.

## Clipboard routing

Native observers use the [ordered clipboard API](file-core-next-api.md). Generation assignment happens before asynchronous inspection. The latest selection coalesces at both service boundaries, so command pressure cannot lose invalidation or revive an old capture. Own-write invalidation preserves remote ordinary clipboard providers and newer remote file references.

The file owner, physical controller and recipient can be three different computers. The controller routes an opaque stamped selection reference to the owner when focus changes. The owner serves the recipient directly. Portal handles and local path representations are excluded from ordinary clipboard sharing.

Receive-to-clipboard is an explicit acceptance step. Native publication checks its captured clipboard generation and latest receive intent immediately before writing. Superseded publication leaves the completed receipt available in the shelf.

## Transport and storage

The file capability is `files-v2`, under protocol 7. Both peers must support the metadata schema. File control frames have a separate bounded queue; bulk content uses a separate TCP connection, authenticated against the Tailnet peer and bound to a single transfer grant. SHA-256 digests, declared lengths and source identities are verified. Disconnect, connection replacement and policy changes revoke access. Existing input transport remains independent.

Receives use private staging, durable journals and no-overwrite publication. Directory metadata is applied through verified descriptors, including recovery of an interrupted root publication. Cleanup preserves immutable flags and checks ownership and recorded filesystem identities. Clear can fail if external software changed or locked retained data. A failed clear is not an atomic rollback of a partially removed tree; remaining ownership and the error must stay recoverable. See [storage integration](file-permissions-integration.md) for the final cleanup design and platform limits.

## Validation

Core tests use mock input backends, loopback transport and disposable filesystem fixtures. They cover copy integrity, malformed/truncated payloads, source mutation, destination collisions, authorization revocation, sequential transfer capacity, clipboard routing and phase ordering, metadata, links, recovery and receipt pins.

Native acceptance is deferred to [the server checklist](file-handoff-validation.md). Passing core tests does not verify Finder or Wayland drag behavior.
