This review covers core/proto commit `4315ead`. Findings are being repaired in an isolated checkout.

# Review of 4315ead (core/proto file metadata + receipt recovery), diff from 7fcd465

Read-only review. No edits, no commits, no interactive or native tests. Snapshot taken with `git show 4315ead:…`.
Scope: `crates/splice-proto/src/files.rs`, `crates/splice-core/src/files/{manifest,storage,service,offers,transport}.rs`,
`service/{commands,control}.rs`, `tests/files_e2e.rs`, checked against `docs/file-core-integration.md`.
`docs/file-sharing-review-2.md` does not exist at this commit.

## Verdict

One material defect (Medium), two Low items. Everything else in the review checklist is satisfied.

## Finding 1 (Medium): preserved directory modes without owner-write break publish, cleanup, and Clear

Root cause: 4315ead now applies the source directory mode to staged directories *before* publication
(`storage.rs` `Staging::complete`, `apply_metadata` for `EntryKind::Directory`), but every later
step that must move or delete those directories runs as plain rename/unlink with no mode reset,
and `preflight_clear` actively refuses directories lacking `0o300`.

Trigger: offer any tree with a directory whose mode lacks owner write or search, e.g. `r-xr-xr-x`
(0o555) directories from extracted archives, package installs, `site-packages`, read-only Git
exports. Existing tests only use 0o750/0o751 roots, so this is untested.

Concrete failures, all on both Linux and macOS (macOS probe: `rename` of a 0555 directory into a
new parent fails with EACCES; Linux `vfs_rename` requires `MAY_WRITE` on a directory changing parent):

1. Root directory with mode 0o555: `Directory::publish` (renameat2/renameatx_np) fails with EACCES
   for every receive of that offer, Save and Cache alike. `storage.rs:784`.
2. After that (or any other) failure, `Staging::fail` -> `cleanup` -> `remove_dir_all(stage)`
   fails because unlinking inside a 0o555 directory needs write on the directory. Result:
   `cleanup_failed`, `.splice-<id>.partial` residue left in the user's chosen destination (or in
   `received/<id>`), and `recover_journal` -> `cleanup_staging` fails again on every restart,
   emitting a recovery error each start. `storage.rs:821-831`, `storage.rs:387-410`.
3. Cache receipt with any nested 0o555 directory publishes fine, but `ClearReceived` can never
   succeed: `preflight_clear` requires `mode & 0o300 == 0o300` on every directory
   (`storage.rs:526-531`), and even without that check `remove_dir_all` would fail. The receipt
   stays Ready forever and its `total_bytes` are permanently charged against `CACHE_QUOTA`
   (`commands.rs` retained fold). Only manual chmod outside the app frees it, contradicting the
   contract that explicit Clear reclaims cache data.
4. Untracked cache directories (journal beyond the 256 view limit or quarantined) containing a
   non-searchable directory make `inventory_cache` fail, which reserves the full quota and blocks
   all cache receives until manual repair. `storage.rs:337-346`.

Suggested correction (small, keeps the "metadata before publication" property for everything
except the root directory bit that the OS itself forbids):

- In `Staging::complete`, for each root that is a directory, open its fd in the stage, apply
  child metadata as today, rename via `publish`, then apply the root's own mode/mtime through the
  already-open fd (`fchmod`/`futimens`). fds survive rename, so the identity is unambiguous and no
  path is re-resolved. Non-root directories can keep the current order.
- In every owned-tree removal (`Staging::cleanup`, `cleanup_staging`, `clear_journal`), walk the
  tree fd-relative with `O_NOFOLLOW|O_DIRECTORY`, and `fchmod(0o700)` each directory whose
  `st_uid == geteuid()` before `remove_dir_all`. Replace the `0o300` rejection in `preflight_clear`
  with that repair; keep the immutable/append flag rejection.
- Optionally apply the same repair in `inventory_cache` before descending.

Tests to add (pure filesystem, `storage.rs` unit tests):

- Manifest with root dir 0o555 containing a 0o500 subdirectory and a 0o444 file:
  `complete()` succeeds for Directory and Cache destinations, destination modes match,
  `initialize()` sees Ready.
- Same manifest, second root collides so `complete()` fails: `fail()` succeeds and no
  `.splice-*.partial` remains in the destination.
- Cache receipt with nested 0o555 dir: `clear_received` succeeds and `received/<id>` is gone.
- Recovery case: journal with `stage_identity` set and a staged 0o555 root, `initialize()`
  removes the stage without a recovery error.

## Finding 2 (Low): recovery scan mutates the directory it is iterating

`storage.rs:204-292` iterates `read_dir(records)` while renaming corrupt journals to `*.bad` and
while `save()` renames temp files into the same directory. POSIX leaves it unspecified whether
entries added during iteration are returned, so on some filesystems (ext4 htree order in
particular) the freshly quarantined `.bad` file can be reported a second time in the same run,
doubling `error_count` and the diagnostic. `corrupt_journal_is_quarantined_with_persistent_visible_diagnostics`
asserts `error_count == 1`, so this would surface as a flaky Linux test rather than data loss.
Fix: collect the `read_dir` entries into a `Vec` (bounded by `MAX_RECORDS * 4`) before processing.
Same pattern in the `received` inventory loop, which removes empty directories mid-iteration.

## Finding 3 (Low): recovered Ready receipts keep stale `bytes`

`recover_journal` (`storage.rs:161-163`) promotes an interrupted journal to Ready when every root
identity is found, but leaves `record.bytes` at the last journaled value (0 unless the crash was
after `complete`'s final save). The UI then shows Ready with partial byte counts, and
`payload_bytes_received` under-counts. Set `record.bytes = record.total_bytes` alongside the state.
Add an assertion to `crash_after_publish_before_result_recovers_published_roots`.

## Satisfied (checked, no action)

- Link traversal (`proto files.rs validate_links`): components validated, absolute and trailing
  slash rejected, `..` at a root rejected, intermediate non-directory rejected, expansion of
  nested links follows kernel semantics (expand before applying `..`), 40-expansion and
  262,144-component budgets, exact-name lookup so case/normalization aliases cannot resolve to a
  different entry, cycle DFS over tree + link edges. Cross-root links fail at the root `..`.
- Receiver never follows links: all staging operations are fd-relative with `O_NOFOLLOW`, and
  non-final components use `O_DIRECTORY`. Symlinks are created after all payload files and their
  metadata is set with `AT_SYMLINK_NOFOLLOW`. Even a validator hole could not write outside the
  stage.
- Metadata target: file/directory metadata applied via the opened fd, symlink metadata via
  parent fd + name with no-follow. Children before parents by reversed manifest order, which is
  sound because `validate` guarantees parents precede children.
- Source TOCTOU: `walk` uses no-follow `fstatat`, re-stats after enumeration (ctime covers link
  target swaps), `open` uses `O_NOFOLLOW` per component, `verify` compares dev/ino/size/mode/
  mtime/ctime, and `validate` re-reads link targets before and after streaming.
- Destructive cleanup: stage removal guarded by dev/ino identity; `remove_empty_intent_stage`
  uses non-recursive `remove_dir` with uid and 0o700 checks; Save destinations are never deleted;
  published-root identity is verified before cache removal; `remove_dir_all` is symlink-safe.
- Crash/recovery ownership: durable clearing intent, per-record error isolation, corrupt and
  pre-metadata journals quarantined and retained, failed Clear restores Ready + paths, cleanup
  failures excluded from history retirement and quota forgiveness.
- Quota: cache admission sums live records plus untracked inventory with saturating arithmetic;
  no automatic eviction paths exist beyond pathless failed receipts.
- Journal identity: filename, staging name, cache directory, roots, and paths cross-checked on load.
- e2e: symlink round trip, corrupt receipt + failed clear isolation are covered. No e2e covers
  non-writable directory modes (see Finding 1).

Note, not a defect: any absolute symlink anywhere in a selected tree fails the entire offer
(Python venvs, Homebrew trees, some app bundles). This matches the documented contract but will
be a common user-visible failure; consider a per-link skip-with-diagnostic in a later round.
