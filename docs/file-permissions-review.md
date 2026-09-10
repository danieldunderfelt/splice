# Storage review of 683e147

Astra review `run_mtvsjun2881ceb81e7`. Findings are under repair; this is not a completion report.

Not satisfied. Three P2 findings remain:

1. **Permission repair can chmod an external replacement inode.** At [storage.rs:410](/Users/daniel/Work/splice-file-work/core-permissions/crates/splice-core/src/files/storage.rs:410), `fchmodat` resolves the name after the ownership/identity check. I deterministically replaced a checked `000` directory with a hard link to an external `0444` file. The external file became `0700`; only then did `openat` reject it as non-directory. This requires concurrent local namespace mutation, not a remote manifest alone. Private permissions exclude other users but do not exclude same-UID writers. **Fix:** verify an inode-bound capability before chmod. Retain descriptors where possible; another pathname check cannot close this race. Plain macOS `O_EVTONLY` failed to open `000` in my probe.

2. **Clear still fails on readable but non-searchable directories.** At [storage.rs:673](/Users/daniel/Work/splice-file-work/core-permissions/crates/splice-core/src/files/storage.rs:673), opening a `0600` child succeeds, but recursive preflight fails when `Directory::names` opens `"."`. The new permission-error handling covers only the initial child open, so removal never reaches permission repair. Reproduced with the extracted functions. **Fix:** account for missing search permission before recursion, or preserve and handle permission errors throughout preflight. Add a `0600` Clear regression.

3. **Final deletion is not bound to the verified inode.** At [storage.rs:469](/Users/daniel/Work/splice-file-work/core-permissions/crates/splice-core/src/files/storage.rs:469), `unlinkat` resolves the name again. A deterministic replacement immediately before that call deleted the empty replacement, retained the original elsewhere, and returned success. This is a remaining ownership-contract gap. **Fix:** prevent namespace replacement throughout removal, through enforced serialization or an appropriately isolated deletion namespace. Rechecking immediately before unlink still leaves a race.

Checks and limits:

- All **19 existing storage unit tests passed**. Compiled temporary probes used extracted helpers with deterministic mutation hooks. No repository edits or prohibited operations.
- Normal publication preserves directory modes/mtimes through the retained FD. Static symlink targets and external Save trees survived the existing tests. Failed Clear retained receipt/path ownership; immutable flags were not cleared.
- The documented crash window remains: recovery can report Ready with total bytes while the published root retains staging mode/mtime. Recovery does not repair that metadata.
- Linux compatibility is conditional. [glibc 2.31 rejects the flag](https://raw.githubusercontent.com/bminor/glibc/glibc-2.31/sysdeps/unix/sysv/linux/fchmodat.c); [2.32 implements it through `O_PATH` and `/proc`](https://raw.githubusercontent.com/bminor/glibc/glibc-2.32/sysdeps/unix/sysv/linux/fchmodat.c). [2.39 uses `fchmodat2` when available](https://raw.githubusercontent.com/bminor/glibc/glibc-2.39/sysdeps/unix/sysv/linux/fchmodat.c), so `/proc` is not universally required. Linux was source-reviewed, not executed.

