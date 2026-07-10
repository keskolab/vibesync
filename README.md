# Code Sync

Your AI coding sessions — Claude Code, Codex, VS Code Copilot Chat, and more — available on every machine you use. Private, encrypted, yours.

Code Sync is a lightweight menu bar / tray app that syncs the *local session data* of AI coding tools between your machines, using storage **you** control: iCloud, OneDrive, Dropbox, Google Drive, your own S3/R2 bucket, or an external disk. Everything is encrypted on your machine before it leaves it.

> **Status: early development.** macOS + Windows are the v1 targets. Nothing here is ready for use yet.

## Why

AI coding tools keep their sessions on the machine where they happened. Start a session on your desktop, open your laptop — it's not there. Vendors don't sync it; some tools even auto-delete transcripts after ~30 days. Code Sync makes your session history portable, durable, and visible everywhere — including inside each tool's own UI where possible.

## How it works

- A small resident app watches each tool's session storage and syncs changes to your chosen backend.
- All content is compressed and encrypted client-side (age) — cloud providers only ever see ciphertext. The iCloud backend uses your iCloud keys instead; nothing to remember.
- Sync is additive by design: nothing is ever deleted from your archive, and locally deleted sessions are never resurrected.
- Per-tool adapters know each tool's storage quirks (path encodings, registries, workspace hashes) and remap machine-specific paths so sessions work on every machine.

## License & contributions

Code Sync is **GPL-3.0** — free to use, modify, and share; derivative distributions must remain open under the GPL.

This project is **open source, but not open contribution**: pull requests are not accepted, so the codebase remains single-author (this keeps dual-licensing possible — see below). Bug reports, feature requests, and *adapter intel* (where tool X stores its sessions on platform Y) are very welcome as issues.

**Commercial licensing** (closed-source embedding or distribution) is available — contact the author.

## Development

```sh
# engine tests
cargo test -p codesync-engine

# run the app (dev)
cd app && npm install && npm run tauri dev
```

Copyright © 2026 JohnKesko
