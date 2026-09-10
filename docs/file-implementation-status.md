# File handoff implementation status

Implementation and review repairs are integrated. The candidate is ready for the separate server acceptance session. Native drag/drop and clipboard acceptance remain unverified.

The user has deferred all interactive testing to a separate Linux server agent. This session must not open native test windows, inject input, perform native clipboard operations, or install/restart Splice on either platform. The installed Mac app remains untouched. The existing Mac tray harness requires the `native-ui-tests` Cargo feature and has only been compiled.

## Integrated work

- Core transfer service, protocol 7 and `files-v2`, descriptor-backed source selection, clipboard owner/controller routing, metadata, durable receipts and explicit clearing.
- Mac AppKit shelf, exact-path promised files, received-file drags, native clipboard capture and generation-guarded publication. The final independent Mac review is satisfied.
- Linux GTK shelf, deferred read-only FUSE views, portal integration, received-file actions and durable view journal. Kimi's `30b7acb` and Fable's `ce4e0fc` are integrated with parent recovery, presentation and IPC repairs.
- Files settings, IPC and tray/menu entry points. Linux packages ship one executable; the service launches its own executable image in internal shelf mode.

## Completed review fixes

Storage integrates `3718bdb` and the parent claim-completion fixes. Astra is satisfied with the final cleanup and recovery behavior. The 34 storage tests and the live failed-Clear relocation regression pass.

The final clipboard repair is `d20ab35` plus `e1edb05`. Fable is satisfied with cancellation, native session ordering and the fresh-copy requirement after a portal outage or backend switch.

Astra confirmed the integrated Linux fixes for attach incarnations, failed Clear recovery, on-demand view preparation, orphan receipt recovery and bounded summaries. Its IPC and durable-drop follow-ups are satisfied. Fable's `f91e551` and `cff89b6` connect button receives, retries and native CommitDrag replies to one transfer tracker. The parent fixed stale-snapshot pruning, and Astra approved the final source. All reported review findings are addressed within the approved scope.

Review closures are recorded in [file-review-closure.md](file-review-closure.md). Earlier review documents remain as historical findings, not descriptions of the final candidate.

## Noninteractive evidence

The integrated Mac workspace compiles for all targets. The opt-in tray harness compiles without being executed. Four IPC framing tests and 13 Mac promise/receipt lifecycle tests pass, including exact bytes, no-overwrite publication, cancellation, executable bits, mtime, maximum-length names and nonblocking rejection of named pipes.

The broad integrated core/protocol run passed 248 tests. Later storage repairs passed 34 Mac storage tests, nine file-transfer tests and the targeted live failed-Clear regression. On Linux, strict workspace Clippy, 44 file/FUSE tests, 34 app service tests, 32 storage tests and the app build pass. The current Mac workspace also passes strict Clippy for all targets. No native acceptance, package installation or installed-app restart is claimed.

The feature remains uncommitted in the working tree on base `3f348529c137b1d6906e328e941c21816b7a527f`. The existing user change to `AGENTS.md` is preserved. A source snapshot and compiled Linux candidate are available at the locations in [the server checklist](file-handoff-validation.md). The installed Mac and Linux applications were not updated for this handoff.

Use [the server checklist](file-handoff-validation.md) for native testing and [file sharing](file-sharing.md) for the user workflow. It uses separate shelf drop and pickup gestures; automatic boundary drag continuation is not implemented.
