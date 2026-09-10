# macOS file shelf adapter

The macOS adapter is the native half of file sharing described in
[`docs/research/file-transfer-and-drag-drop-plan.md`](research/file-transfer-and-drag-drop-plan.md).
It lives in `crates/splice-platform` and owns AppKit interaction. The app bridges its
`splice_platform::file_shelf` contract to `splice_core::files`, which owns transport, storage and
policy. See [the core contract](file-core-integration.md).

## Lifecycle integration changes

`FileEvent::DragRetired { attempt }` is a reliable terminal event emitted after the session,
all providers, and all accepted callbacks release their shared attempt lease. The parent must
remove attempt metadata on this event, including roots that never received a callback. Do not
retire successful attempts merely because each root has finished; provider ownership permits
late callbacks. Native admission bounds live attempts at 32 and reserves terminal event capacity.

`FileEvent::ClipboardInvalidated { generation }` reliably reports the latest clipboard replacement,
including Splice's own writes, non-file changes and file captures rejected by admission. The app uses
file-only invalidation that preserves remote text/image providers and newer remote file references.
It ignores stale generations using the live pasteboard count. Empty paths are never used as a capture.
`ClipboardChanged { generation, mimes, inline_text }` carries genuine ordinary changes on the same
generation clock. Same-generation invalidation cannot revoke an already accepted file capture.

`PromiseWriter::cancellation()` returns independent `PromiseCancellation` state with no reply sender.
Dropping all writers now wakes the native receive even while the callback holds cancellation state.
Cancellation and publication compete through one atomic gate after fsync. Winning publication makes
later cancellation too late, without making the AppKit thread wait for filesystem work.

Clear received copy explicitly revokes earlier local URL exports for that receipt. An app still
reading an earlier export may fail. Clear releases idle export pins; clipboard and current drag
pins remain independent. The parent's existing suppress-repin, snapshot removal, snapshot-drop,
main-queue barrier and core Clear sequence is still required. No confirmation dialog is added.

## What the adapter does

- Shows a focus-friendly file shelf: a nonactivating, floating `NSPanel` that joins all Spaces, never
  becomes key unless a control needs it, and is driven by the existing `NSApplication` run loop via the
  main dispatch queue. It appears when a new incoming offer arrives. Hide is respected until the next new
  incoming offer; progress updates never reopen a hidden shelf.
- Lets the user pick a recipient explicitly, then offer files by dropping them on the shelf or through
  an `NSOpenPanel`. Only copy drops are accepted; a drag whose source mask lacks `Copy` is refused.
- Lists offers and every transfer as its own row. Receipt rows show which roots they cover, progress,
  and their own Copy, Reveal, Cancel, Retry, and Clear actions. A Ready receipt never claims the whole
  offer, an older receipt never hides a newer failure, and partially published Save results stay
  revealable.
- Starts a fresh native drag from a row. Available file offers drag with `NSFilePromiseProvider`, so the
  receiving app chooses the destination and bytes move only when the drop is accepted. Ready receipts
  drag as local `NSURL`s for both files and folders, but only when the parent supplied a pinned
  `ReceivedSelection`.
- Reports every external clipboard change through `FileEvent::ClipboardChanged` or
  `FileEvent::ClipboardFiles`, with an explicit generation. It also emits the legacy
  `PlatformEvent::ClipboardChanged` for daemon consumers; the app's ordered engine bridge ignores
  those duplicates. File selections never publish their pathname representation as ordinary text.
- Publishes received files to the pasteboard only when the receive intent is still the newest and the
  change count still equals the generation captured at Receive, checked after URL preparation and
  immediately before clearing.
- Keeps `NSURL` security-scoped access alive for as long as the parent holds the returned lease, for
  sources, Save destinations, and promise destinations alike.

The adapter never reads file contents on hover, pickup, drag start, or pasteboard inspection. Bytes
are requested only from Receive, Save…, or an accepted native promise write.

## Public API

Everything below is in the contract module unless noted.

### Handle

`Platform.files: Option<FileAdapter>` is `Some` on macOS.

```rust
pub struct FileAdapter {
    pub shelf: Arc<dyn FileShelf>,
    pub events: tokio::sync::mpsc::Receiver<FileEvent>,
}

pub trait FileShelf: Send + Sync {
    fn sync(&self, snapshot: ShelfSnapshot);
    fn set_visible(&self, visible: bool);
    fn publish_clipboard_files(&self, selection: ReceivedSelection, generation: u64, intent: ReceiveIntent) -> Result<u64>;
    fn reveal(&self, paths: Vec<PathBuf>);
}
```

`sync` stores the newest snapshot and schedules at most one main-queue closure at a time, so a burst of
progress updates coalesces. `set_visible` and `reveal` enqueue main-queue work. `publish_clipboard_files`
runs synchronously on the caller's thread. It prepares one `NSURL` per path, then under the adapter's
write lock verifies that `intent` is still the newest Receive/Copy intent the shelf issued and that the
general pasteboard change count still equals `generation`, then clears and writes. The adapter keeps the
`ReceivedSelection` (and therefore the parent's pin) until the poller observes a different change count.
On success it returns the new change count.

The public receiver holds 256 events. The pending outbox separately bounds ordinary actions,
and controls at 256 each, plus 512 reserved lifecycle events. Two maximum-sized drags fit while AppKit still owns the previous drag pasteboard. Lifecycle reservations count against that
last bound before an operation starts; live providers cannot consume ordinary action capacity.
Cancel, Clear and Dismiss have independent pending capacity and identical pending controls coalesce.
When admission fails the initiating action is refused and the shelf records a status error, respecting
Hide. Drag startup reserves Started, Ended, Retired and one Finished per root before creating the
session. `Overflow { dropped }` accumulates refused admissions and is retried as soon as receiver
capacity returns, even with no further native activity. `ClipboardInvalidated` and `ClipboardChanged` share
a single coalescing slot, so replacing the clipboard always invalidates an old offer even if capture
admission fails. Already queued events preserve order; invalidation and overflow precede pending work.

The app retains only a promised root's transfer ID and cancellation token after handing its
`ReceivedLease` to the native writer. That lease ends after the callback finishes, independently of
AppKit retaining the provider. This lets Clear succeed without requiring another drag. Exact-path
publication uses a private temporary file, preserves ordinary mode and mtime, fsyncs, then publishes
with a no-overwrite rename and an atomic cancellation gate.

### Snapshot the parent pushes

```rust
pub struct ShelfSnapshot {
    pub self_id: MachineId,
    pub enabled: bool,
    pub recipients: Vec<Recipient>,
    pub names: Vec<MachineName>,
    pub offers: Vec<ShelfOffer>,
    pub transfers: Vec<ShelfTransfer>,
    pub error: Option<String>,
}

pub struct ShelfTransfer {
    pub id: TransferId,
    pub offer: OfferId,
    pub peer: MachineId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub roots: Vec<u32>,
    pub bytes: u64,
    pub total_bytes: u64,
    pub paths: Vec<PathBuf>,
    pub received: Option<ReceivedSelection>,
    pub error: Option<String>,
}

pub struct ReceivedSelection {
    pub transfer: TransferId,
    pub paths: Vec<PathBuf>,
    pub access: Option<Arc<dyn Send + Sync>>,
}
```

Snapshots and transfers are local values and no longer derive serde. Equality of `ReceivedSelection`
compares the pin by `Arc` identity. The parent should call `FileHandle::retain_received` for every Ready
receive transfer before advertising it and put the lease in `received`, so a fresh mouse gesture can start
a local URL drag without an asynchronous request. `roots` comes from the parent's own drag bookkeeping;
core transfer records do not carry it.

Rows are built by `macos::shelf::build_rows`, a pure function with unit tests: one row per offer,
followed by one row per transfer of that offer, then orphan receipts. Rows are updated in place by key.

### Events the adapter emits

```rust
pub enum FileEvent {
    SourceSelected { paths, recipient, gesture, lease },
    ClipboardInvalidated { generation },
    ClipboardChanged { generation, mimes, inline_text },
    ClipboardFiles { paths, generation, lease },
    Receive { offer, clipboard_generation, intent },
    Republish { transfer, clipboard_generation, intent },
    Save { offer, directory, lease },
    Dismiss { offer },
    Cancel { transfer },
    Retry { transfer },
    ClearReceived { transfer },
    DragStarted { attempt, offer, roots },
    PromiseRequested { attempt, root, destination, writer },
    PromiseFinished { attempt, root, result: Result<PathBuf, String> },
    DragEnded { attempt, outcome },
    DragRetired { attempt },
    Overflow { dropped },
}
```

Expected bridge, per event:

| Event | Parent action |
|---|---|
| `SourceSelected` | `FileHandle::offer_local(LocalSelection { roots: paths, access: Some(lease) }, recipient, Selection)`. The lease also owns promised-import staging cleanup. |
| `ClipboardInvalidated` | Invalidate the previous file clipboard offer when this generation is still current. This requires a parent/core invalidation path independent of successful selection capture. |
| `ClipboardFiles` | `EngineHandle::capture_file_clipboard(LocalSelection { .. access: Some(lease) }, generation)`. |
| `Receive` | `Receive { offer, destination: Cache }`, `wait`, `retain_received`, then `publish_clipboard_files(ReceivedSelection { transfer, paths, access: Some(lease) }, clipboard_generation, intent)`. |
| `Republish` | `retain_received(transfer)` then `publish_clipboard_files` with the same intent and generation rules. |
| `Save` | `Receive { offer, destination: Directory(directory) }`. Hold `lease` until the transfer reaches a terminal state; it carries the chosen directory's security scope. |
| `Dismiss` | `Revoke { offer }`. |
| `Cancel` / `Retry` | `Cancel` / `Retry`. |
| `ClearReceived` | Suppress repinning, remove the parent pin, push `received: None`, drop the old snapshot, await a queued main-thread barrier, then issue core ClearReceived. Repin on failure. Native Clear already releases the clicked row's idle pin and idle export pins; current clipboard and drag pins remain independent. |
| `DragStarted` | Register offer/root authorization metadata only. Do not prepare or fetch payload until PromiseRequested. |
| `PromiseRequested` | `PrepareDrag { offer, entries: [root] }`, then `DropDrag` (await the acknowledgement), `CommitDrag { .. Cache }`, `wait`, `retain_received`, then `writer.complete(Ok(PromiseSource { path, access: Some(lease) }))`. On failure `writer.complete(Err(message))`. To abort after the network is done, `writer.cancel()`; the native copy checks it between chunks. |
| `PromiseFinished` | Record the destination result for that root. `Ok(path)` is the exact published URL. `Err` is what AppKit's completion handler also received. |
| `DragEnded { Cancelled }` | `CancelDrag` for every drag id of that attempt. |
| `DragEnded { Copied }` | Nothing; promise writes for accepted roots arrive as `PromiseRequested`. |
| `DragRetired` | Remove attempt authorization metadata and per-root writer bookkeeping. No callback can arrive after this event. |
| `Overflow` | Show how many native actions were refused. Accepted terminal events and the latest clipboard invalidation remain deliverable. |

Local URL drags of pinned receipts emit no events. An active drag holds its own `ReceivedSelection`.
After an accepted drop, the adapter pins that receipt until Clear explicitly revokes earlier URL
exports. There is no elapsed-time assumption about reader completion. At most 32 distinct exported
receipts can be pinned; additional exports are refused until an earlier receipt is cleared. Clipboard
publication ownership is separate and is released when its generation is replaced, including by
Splice's own text/image writes.

### Promise write path

The write callback runs on a private `NSOperationQueue`, starts security-scoped access on the supplied
URL, and blocks there until the parent completes the writer. It opens the source without following links or waiting on a named pipe, then copies the verified regular file in 1 MiB
chunks into a private sibling staging file (`.splice-<process-random>-<attempt>-<root>.part`, created with
`create_new`), checking cancellation between chunks, verifies the byte count, syncs, and publishes with
`renamex_np(RENAME_EXCL)` to the exact supplied basename. After sync and before rename, an atomic
cancellation/publication gate decides the winner. Cancellation removes staging; publication claims
the commit point and makes later cancellation too late. This gate never waits for filesystem work
on the AppKit thread. An existing destination is never truncated or
replaced; on any failure only the owned staging file is removed. The AppKit completion handler is called
exactly once with `nil` or an `NSError`. Nothing wraps the write in `NSFileCoordinator`:
`NSFilePromiseReceiver` already coordinates the delegate callback (AppKit SDK,
`NSFilePromiseProvider.h`). The parent's pin is released only after the completion handler returns.

Each provider's `userInfo` strongly retains its delegate because the SDK delegate property is weak.
Each delegate holds the shared attempt lease, and an executing callback explicitly retains the
delegate through completion and Finished delivery. The session registry releases its provider
references at drag end. External providers and callbacks keep the attempt admitted and its metadata
valid until their final release emits `DragRetired`. There is no age eviction or retired-delegate
cache. Admission permits at most 32 live attempts, also subject to lifecycle event capacity. Each
provider accepts one write callback; duplicate requests receive an error without another payload
request. The native callback retains cancellation state alone while waiting for the parent reply.

### Source-side promises

Dropping promised items (for example from Mail or Photos) on the shelf receives them through
`NSFilePromiseReceiver` into `<data_dir>/promised-imports/<epoch>-<n>/` on a private queue. The
expected count is read from each receiver's `fileNames` after `receivePromisedFilesAtDestination` is
called, as the SDK documents; `fileTypes` is not used as a count. Reader callbacks that arrive before the
inventory is known are counted and the import completes exactly once.

A staging guard exists at admission, before background directory creation. The import reserves one
success event and an admission permit. At most eight live callback groups and 64 receivers per drop
are admitted. Native callbacks share the guard and keep the admission permit until they release their
pending state. The parent's successful offer lease holds only cleanup, so completed imports free
admission. Successful SourceSelected delivery transfers a guard reference to the parent's source lease. Completion queues prompt removal from the native receiver
registry. Failure, an empty inventory, dropped callbacks, or a 30-minute import abandonment deadline
produce exactly one terminal status instead. Abandonment removes registry ownership but never deletes
staging while a callback still owns it. Late callbacks after a terminal outcome are ignored. Directory
creation and final cleanup run on background workers. The abandonment timer holds only a weak reference
and is not a claim that an external writer completed.

## Threading and bounds

`macos::create` may be called from any thread. The AppKit objects are created lazily on the first
main-queue closure and live in a main-thread `thread_local`. Background callbacks only touch `Send`
state and hop back onto the main queue. Import abandonment uses the platform's tokio handle. While a
drag from a row is active, snapshots update existing rows in place and structural layout (row
insertion, removal, panel resizing) is deferred until the session ends so the source view survives.

## Validation status

The [final independent Fable review](file-macos-review-final.md) is satisfied. It verified platform and app compilation, 44 selected pure platform tests, seven core clipboard clock/mailbox tests and two mock-engine clipboard scenarios. Parent also verified the complete 13-test promise suite after adding nonblocking named-pipe rejection, including the extension-to-file-type lookup.

The Mac workspace compiles for all targets. The opt-in tray harness was compiled without execution. No native provider, panel, Finder operation, pilot binary, native clipboard operation, input injection, installed-app change or app restart was used for these checks. Filesystem tests operate on disposable files.

Finder acceptance, actual panel layout and focus behavior, chooser presentation, security-scoped native selections and promised-source imports remain unverified. These need a separately scheduled desktop session under [the acceptance checklist](file-handoff-validation.md).
