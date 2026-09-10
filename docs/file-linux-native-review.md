Found 12 issues in `1839fd0` against `3f34852`. App/core bridge omissions are excluded.

1. **P1 — Symlink validation permits escaping the selected tree.** [manifest.rs:157](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/manifest.rs:157)  
   Reproduce with `alias -> .` and another link targeting `"alias/" × 10 + "../" × 10 + "etc/passwd"`. Lexical normalization accepts it as `<root>/etc/passwd`; filesystem resolution reaches `/etc/passwd`, including through the destination FUSE view.  
   Fix: validate targets by resolving the manifest’s symlink graph, with traversal limits and containment checked after every expansion.

2. **P1 — A peer update silently changes the chosen recipient.** [helper.rs:167](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/helper.rs:167)  
   Select B, then receive `Peers` containing only A before dropping. Rebuilding the model automatically selects A; restoration of B fails silently, so `SourceDrop` names A. The service cannot reject this as a stale B selection. GTK uses a selection model with [autoselection enabled by default](https://docs.gtk.org/gtk4/property.SingleSelection.autoselect.html).  
   Fix: retain the explicitly chosen peer ID independently and invalidate it when that peer disappears. Reject drops until another recipient is chosen.

3. **P1 — Cancellation can precede the commit callback and still allow materialization.** [mount.rs:571](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/mount.rs:571), [mount.rs:308](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/mount.rs:308)  
   Pause a read after `gate.on_read()` returns `Commit`; concurrently cancel the view. `on_cancel(view, true)` runs before `on_commit(view)`, after which the read still queues work. Workers generally neither check retirement before materializing nor before replying.  
   Fix: serialize commit/cancel notifications per view, reject retired queued work, and check retirement before publishing results.

4. **P1 — The Linux IPC module breaks macOS workspace builds.** [lib.rs:2](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/lib.rs:2)  
   The new workspace member exports `ipc` unconditionally, but it uses Linux-only `SOCK_CLOEXEC`, `SO_PEERCRED`, `MSG_CMSG_CLOEXEC`, and `libc::ucred`. A compiler-only probe confirmed all four are unavailable on this Mac. Thus the existing macOS workspace CI cannot compile this commit.  
   Fix: gate the Linux transport module with `cfg(target_os = "linux")`.

5. **P2 — Rejected IPC records leak received descriptors.** [ipc.rs:334](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/ipc.rs:334)  
   Send a record exceeding 64 KiB with an attached FD, or send 65 FDs. `recvmsg` installs descriptors that fit, but the truncation branches return before wrapping or closing them. Closing the connection does not close those descriptors.  
   Fix: take ownership of every delivered `SCM_RIGHTS` FD before handling truncation or zero-length records, then reject and drop them.

6. **P2 — Worker shutdown processes queued transfers, and failed mounting leaks workers.** [mount.rs:97](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/mount.rs:97), [mount.rs:247](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/mount.rs:247)  
   Drop a mount with reads queued: every successful dequeue still calls `serve_read`; shutdown is checked only after an empty-queue timeout. Separately, make `spawn_mount2` fail: already-started workers retain `MountInner`, its sender, and the receiver indefinitely because no `Mount::drop` sets shutdown.  
   Fix: own workers with a cleanup guard covering initialization failures, check shutdown before dispatch, cancel active work, and answer queued reads with `EIO`.

7. **P2 — Retiring an older view removes a newer armed view.** [helper.rs:540](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/helper.rs:540), [helper.rs:584](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/helper.rs:584)  
   Deliver two offer snapshots before `ViewReady`: both request a view. Replies V1 and V2 overwrite `armed[offer]` while retaining both index entries. `RetireView(V1)` then removes V2. V1 also receives no cleanup when overwritten.  
   Fix: track pending requests, retain one armed view per offer, explicitly retire superseded views, and remove an armed view only when its ID matches.

8. **P2 — IPC backpressure can freeze GTK or grow memory without bound.** [helper.rs:398](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/helper.rs:398), [helper.rs:434](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/helper.rs:434)  
   Stop service-side reads and fill the socket: a GTK callback blocks inside synchronous `sendmsg`, preventing input and cancellation processing. Conversely, incoming messages use an unbounded channel, and the timer drains without a work budget. Sustained progress updates can exhaust memory or monopolize GTK.  
   Fix: use bounded asynchronous outbound handling, bounded/coalesced incoming updates, and a limited batch per GTK iteration.

9. **P2 — Native copies lose executable and writable permission bits.** [mount.rs:379](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/mount.rs:379)  
   Offer a `0755` script: FUSE reports `0400`, ignoring the recorded mode. Consumers preserving source permissions create a non-executable, read-only copy. Directories similarly become `0500`.  
   Fix: expose the supported manifest permission bits, excluding privileged bits. Keep write and execution restrictions in the existing mount options.

10. **P2 — Portal exports exceed common D-Bus FD limits.** [fileportal.rs:73](/Users/daniel/Work/splice-file-work/linux/crates/splice-platform/src/linux/fileportal.rs:73)  
    Export 17 files on a session bus limited to 16 descriptors per message. `begin` sends them all in one `AddFiles` call, so the export fails. The portal documentation explicitly requires [batching larger FD lists](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileTransfer.html#org-freedesktop-portal-filetransfer-addfiles).  
    Fix: batch `AddFiles` calls and stop the already-created transfer if any batch fails.

11. **P2 — Enumeration silently changes non-UTF-8 filenames.** [manifest.rs:130](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/manifest.rs:130)  
    Select a normally named directory containing a filename with byte `0xff`. Enumeration succeeds but replaces that byte with U+FFFD in the manifest. Link targets receive the same lossy conversion.  
    Fix: preserve names losslessly throughout the protocol, or explicitly reject unsupported names and targets before accepting the selection.

12. **P2 — The entry limit does not bound enumeration memory.** [manifest.rs:125](/Users/daniel/Work/splice-file-work/linux/crates/splice-files/src/manifest.rs:125)  
    Select a directory containing millions of children. `collect::<Vec<_>>()` loads and sorts every child before recursive calls enforce the 20,000-entry limit.  
    Fix: enforce the remaining entry budget while iterating, before collecting and sorting.

Validation was read-only: static tracing, upstream GTK/portal checks, a compiler-only probe, an in-memory symlink-resolution check, shell syntax checks, and `git diff --check`. No files changed; the pre-existing `AGENTS.md` modification remains.

Manual validation gaps remain for actual Wayland drag/drop/cancel ordering and late readers on KDE/GNOME, sandboxed portal round trips, and clipboard survival after closing/reopening the shelf. No GUI, pilot, synthetic input, installation, or interactive test ran.


## Core API integration update

Parent has received the next core API. Read `/Users/daniel/Work/splice/docs/file-core-next-api.md` and `/Users/daniel/Work/splice/docs/file-clipboard-integration-notes.md` before finalizing Linux integration. New core manifest metadata is `mode`, `modified: FileTimestamp`, and `EntryKind::Symlink { target }`; optional file capability becomes FILES_V2. ALL genuine clipboard changes need the ordered `EngineHandle::clipboard_changed(generation, mimes, inline_text)` channel, not only file captures. Own publication invalidation must preserve ordinary remote fetch handles. Core agent is implementing types now; parent will integrate the final committed snapshot.


## Packaging architecture change

The parent removed the experimental two-executable updater changes. Production now starts the shelf using `/proc/self/exe file-shelf` and enables `splice-files`'s `helper` library feature on the Linux app dependency. `main.rs` dispatches that internal mode to `splice_files::helper::run()`. The existing signed archive/installer/updater remain single-executable. The parent changed `file_service/shelf.rs::HelperProcess::spawn` accordingly; preserve this when integrating your final commit. Standalone `splice-files` remains a development/pilot executable, not a production dependency.

## Core snapshot available for final compilation

Core/proto metadata and recovery commit `4315eada9812ec27187d564dd2b0c24d62220405` is now integrated into the parent root. Use these committed core/proto files for Linux compilation. The core-only agent is finishing `invalidate_file_clipboard` and reliable coalescing of native generations; its public synchronous signatures remain unchanged.

The native `FileEntry.mtime: i64` currently carries only seconds. Preserve `FileTimestamp.nanos` too through native IPC/manifest/FUSE attributes and local receipt copies. FUSE supports nanosecond timestamps; dropping the fraction in the core-to-native mapper would lose metadata on deferred drags even though direct core Save preserves it. Use an explicit bounded nanosecond field or native timestamp type and reject invalid fractions. Root Mac publication already copies mode and full supported mtime from the verified receipt.

## Local receipt Save safety

The new app `file_service/copy.rs` must not use a preflight existence check followed by `std::fs::copy`. That call overwrites a destination created after preflight, including a later root created while an earlier large root is being copied. Source metadata checks followed by path-based `read_dir`/`copy` also follow replacements. Use descriptor-relative no-follow reads, exclusive private staging and no-replace publication, retain the receipt throughout, preserve directory/file/link mode and full mtime, and clean only the operation's owned staging on failure. Do not publish partially copied files. Pure regressions must prove existing/newly-created destination data stays unchanged, symlink replacements cannot escape the source, and read-only folder modes remain copyable and clearable. If this cannot be implemented safely in the bridge, omit this extra receipt Save button and use the already-required local Copy/Reveal/drag actions; incoming-offer Save through core remains supported.

Core filesystem review found read-only folder modes can prevent rename into a different parent and cleanup of private staging. A dedicated storage fix is underway in `core-permissions`; do not duplicate it in core. Account for the same condition in any local receipt copy path.

When the FileTransfer portal is available but exporting a received selection fails, return that failure and retain the receipt for retry. Do not silently publish URI-only data after a failed portal export. URI-only publication is appropriate when the detected desktop explicitly lacks the portal. This also follows AGENTS.md's no-fallback rule.

## Bound the final helper snapshots

Check worst-case serialized `ServiceToHelper::Receipts` and `Offers` against the IPC packet limit. Core retains up to 256 receipts and supports 128 roots per offer. Sending every full name and every full receipt path in one history snapshot can exceed 64 KiB, disconnecting the shelf precisely when the user needs Clear. Keep display summaries bounded and fetch paths only for the chosen action, or use a bounded paging protocol. Preserve access to every retained receipt. Add a pure serialization regression at the supported limits; increasing a constant without bounding the actual schema is insufficient.
