# File handoff validation

Interactive tests are intentionally deferred. The user is working on the Mac and will run these with a separate Linux server agent. This implementation session must not launch native test windows, inject mouse or keyboard input, activate applications, or restart the installed Splice app.

## Non-interactive checks

Run the file service tests with mock platform backends. These exercise real loopback transport and disposable files without capturing physical input. macOS loopback aliases 127.0.0.2 through 127.0.0.5 were configured earlier in this session.

```sh
cargo test -p splice-core --test files_e2e
cargo test -p splice-core --test engine_e2e
cargo test -p splice-proto
cargo test -p splice-app --bin splice ipc::tests
```

Native platform tests must be selected explicitly after checking that they do not start an application or inject input. Do not run native pilot executables on the Mac.

## Native validation on the Linux server

The candidate source is on `gamedev` at `/home/daniel/splice-files-acceptance-20260910`. It is a source snapshot of the uncommitted feature on base `3f348529c137b1d6906e328e941c21816b7a527f`, not a separate Git checkout. Its build uses `SPLICE_BUILD_COMMIT` set to that full hash and `SPLICE_BUILD_DIRTY=true`. The installed service and `~/.local/bin/splice` have not been changed.

The compiled candidate is `/home/daniel/splice-lifecycle-target/debug/splice`. The build is for acceptance, with debug symbols. It has not been launched. Its SHA-256 is `87bf82eac16607153b1e1e6e6497aceebb88022383b5fbb6c322df917fc6bd33`. Both endpoints need network protocol 7; the shelf's internal protocol is version 4. Use host packages for FUSE drag testing, since the Flatpak sandbox has not been qualified for deferred views.

Final noninteractive logs are `/tmp/splice-acceptance-clippy.log`, `/tmp/splice-acceptance-fuse.log`, `/tmp/splice-acceptance-service.log`, `/tmp/splice-acceptance-storage.log` and `/tmp/splice-acceptance-build.log` on the server. The extracted SDK is `/tmp/splice-files-linux-kimi/devel/root`, and Cargo artifacts are outside `/tmp` in `/home/daniel/splice-lifecycle-target`.

Use disposable fixtures in a separate test directory. Include an empty file, a nested directory with empty children, a large file, a Unicode name, a percent sign, a filename containing a newline, an executable file, a fixed modification timestamp, and relative symbolic links inside a folder. Compute fixture hashes and record permissions and timestamps before testing. Keep the normal Splice configuration and installed binary recoverable when installing a candidate.

Record the Splice commit and protocol on each endpoint, compositor and file manager versions, transport address, exact action, result, and payload counters. Do not infer GNOME support from a successful KDE test.

| Action | Required result |
|---|---|
| Drop local files into Splice and choose a recipient | Recipient sees filenames and sizes. No content bytes are sent. |
| Pick up an offered file and hover over a file manager | Metadata is readable. No content bytes are sent. |
| Read a prospective drag path before dropping | Read fails immediately. It cannot become a deferred transfer authorization. |
| Cancel pickup with Escape | No content transfer starts. The offer can be picked up again. |
| Drop onto a folder and request content | Exactly one transfer starts. Destination receives verified bytes. |
| Repeat drops whose destination reads immediately, including after a hover metadata probe | The accepted drop succeeds even when helper IPC and FUSE callbacks arrive close together. A content read issued before the drop never gains authorization later. |
| Cancel after the drop but before the first content read | No transfer starts. The cancelled view cannot authorize a later read. |
| Make a new local clipboard copy during a transfer | Transfer completes into retained storage. It does not overwrite the newer clipboard. |
| Receive to clipboard and close the shelf | Paste still works from the real local files. |
| Save where a same-named file already exists | Existing file is unchanged. Failure is visible. |
| Change a source after making an offer | Transfer fails clearly instead of silently sending a different version. |
| Disconnect or disable sharing during a transfer | Transfer stops, staging remains unpublished, input releases correctly. |
| Copy files repeatedly, replacing earlier clipboard contents | Old unaccepted offers retire without exhausting the offer registry. |
| Copy text or an image after copying files, and repeat in the opposite order | The latest clipboard selection wins. A delayed file inspection cannot restore an older selection. |
| Copy remote text or an image, then paste after the local native clipboard notification | The notification does not destroy the remote data provider. |
| Receive executable files and nested relative links | Ordinary permissions and supported timestamps survive. Links remain links and resolve inside the received tree. |
| Clear a completed promise receipt after the destination finishes writing | Clear succeeds once active clipboard or drag consumers release the receipt. It does not require starting another drag. |
| Close and reopen the shelf, restart Splice, then disconnect the sender | Completed receipts remain usable for local copy and drag. |
| Lose the selected receiving computer while another computer remains online | The shelf asks for a new recipient instead of silently selecting another computer. |
| Prepare more than eight different offer and receipt rows, then return to an earlier row | Every available row can prepare a fresh drag. |
| Retry a failed receive repeatedly | The row and Cancel action follow the actual new attempt, including when its random ID sorts before an older attempt. |
| Cancel Receive to clipboard, then drag that same offer into a folder | The row follows the new native-drag transfer and offers Cancel while it is running. |
| Make the receipt journal unwritable, then drop a received row | The drop reports an error and its path never becomes readable. Existing retained receipts stay recorded. |
| Clear while a receipt is still being read, then retry after the reader closes | Active reading prevents Clear. A failed cleanup retains its row and recovery metadata. |
| Drop a selection whose local path list exceeds the shelf IPC limit | The shelf rejects the drop visibly before queueing it. The connection remains usable. |
| Interrupt the shelf's IPC writer during a drag | Both ends detect disconnect and release transient drag state. |
| Move across boundaries while transferring a large file | Desktop and Raw input keep responding, including the return boundary. |

For Linux clipboard capture, close the shelf before copying in the file manager. The background service must still capture the file selection. Test both URI representations and the local FileTransfer portal representation when available. No portal key or source path may be sent to another computer.

For three computers, use separate owner, controller, and recipient. Copy a file on A while controlling it from B, then move to C. A must serve C directly. Repeat by returning to B instead of C. Replacing the clipboard before accepting should invalidate the previous unaccepted offer; a transfer already accepted must retain its source access.

For received native drag paths, test delayed readers after the shelf closes and after restarting the service. Do not remove retained files while a clipboard, portal export, native promise, or FUSE view still refers to them.

## Mac acceptance still requiring a scheduled session

Compiling AppKit code does not verify Finder behavior. When the user explicitly schedules Mac interactive testing, verify a real NSDraggingSession and Finder destination, multiple promised files, exact destination URLs, one completion callback per file, cancelled drags, and folder receipt followed by dragging the local folder. Require at least one receiver and compare final bytes; a callback count alone is not success.

The Mac tests must remain opt-in. Ordinary compilation and unit tests must never move the user's cursor.
