# Agent rules for Splice

This is a software KVM app with auto-discovery of Tailnet peers. It also includes clipboard sharing.

## Rules

- Never add placeholder or fallback behavior. All code must be production-ready.
- Never add code comments.
## User testing constraint — 2026-09-10

The user is working on the Mac. Do not run interactive tests, native drag pilots, synthetic mouse/keyboard events, GUI automation, or launch colorful test windows. Do not replace or restart the installed Splice app. The user will perform interactive validation with a separate agent on a Linux server. Continue implementation, compilation, static review, and non-interactive tests only. Existing pilot binaries must never run as part of ordinary cargo test.
