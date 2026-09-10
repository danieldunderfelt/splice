# File sharing review closure

Final source review completed on 2026-09-10. Native acceptance is deferred to the separate server session. The implementation remains in the working tree; delegated commits below identify integrated component changes.

| Area | Final review and result |
|---|---|
| Mac shelf, promises and clipboard | Fable, `run_mtvslkmy4344797b36`, satisfied. Later nonblocking FIFO source rejection passes its regression. |
| Durable core storage and recovery | Astra, `run_mtvv1nz4471141f3da`, satisfied with `3718bdb` and parent claim recovery/completion repairs. |
| Linux native clipboard | Fable, `run_mtvvlsre51835ee93b`, satisfied with `d20ab35` and `e1edb05`. Portal outages and backend switches require a fresh copy. |
| Linux receipts, views and IPC | Astra's review of the integrated `ce4e0fc` candidate closed the earlier lifecycle findings and identified three follow-ups. IPC failure cleanup was approved in `run_mtvw45qg4648311d78`; durable journal-before-drop ordering was approved in `run_mtvw8ekk169794cd55`. |
| Current transfer selection | Fable implemented `f91e551` and `cff89b6`. Astra approved the shared native/button tracker in `run_mtvwdk128c8c9a6718` and the parent's stale-snapshot pruning correction in `run_mtvwfckb27a0b15d35`. |

Kimi's Linux implementation and `30b7acb` follow-up supply the GTK/FUSE integration, bounded paged metadata, explicit recipient behavior, retained receipts and disabled operation when durable storage is unavailable. Parent and Fable repairs were integrated before the final Astra reviews. The optional duplicate received-row SaveReceipt operation was removed; incoming Save to folder, received Copy, Reveal and native drag remain.

The final Linux candidate passes strict workspace Clippy, 44 file/FUSE tests, 34 app service tests, 32 storage tests and a complete application build. The final Mac workspace passes strict Clippy, and the opt-in tray harness compiles without running. The broader core/protocol tests and focused Mac evidence are recorded in [implementation status](file-implementation-status.md).

No reviewer approval substitutes for real Finder, Dolphin or GNOME drag/drop acceptance. The remaining work is the user-deferred [native acceptance checklist](file-handoff-validation.md), followed by any fixes it demonstrates.
