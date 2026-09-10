# File sharing integration checklist

Implementation and review are complete for the candidate described in [the status record](file-implementation-status.md). The deferred interactive acceptance checklist is [file-handoff-validation.md](file-handoff-validation.md).

The integrated design uses retained source descriptors, direct owner-to-recipient transfer, generation-aware clipboard routing, bounded UI summaries, explicit cache clearing, and native receipt leases. Files preserve ordinary modes, supported modification timestamps and confined relative links. Exact-path Mac promises publish from verified retained copies without overwriting a destination.

The file shelf uses separate drop and pickup gestures. It does not create screen-edge target windows or automatically attach files to a cross-computer pointer. The Linux shelf runs from the same executable as the service, using an internal GTK process mode. Mac uses an AppKit panel.

Core storage, Mac, native clipboard and Linux lifecycle reviews are satisfied. The integrated candidate passes the recorded noninteractive checks. Native GUI, clipboard, pointer, installed-app and package-install tests are deferred to the user's separate server agent. Ordinary tests must remain noninteractive, and the Mac tray harness requires the explicit `native-ui-tests` feature.
