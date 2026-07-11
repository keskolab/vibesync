# Code Sync

Code Sync is a small menu bar app that keeps your AI coding sessions in sync across all your computers. Start a Claude Code session on your desktop, open your laptop, and continue where you left off — including your session history, plans, custom agents, and settings.

Your data is stored in a place **you** control: a folder you already sync (iCloud Drive, OneDrive, Dropbox, Google Drive), your own cloud bucket (Cloudflare R2, Amazon S3, Azure), or just a USB disk. Nothing goes through anyone else's servers, and anything sent to cloud storage is encrypted on your machine first.

> **Status: early development.** macOS and Windows are the v1 targets. Claude Code is the first supported tool; Codex, VS Code Copilot Chat, and Zed are next.

## Why does this exist?

AI coding tools keep their sessions on the machine where they happened. Vendors don't sync them, and some tools even delete transcripts after about 30 days. Your conversations, plans, and project memory are valuable — they shouldn't be stuck on one computer or quietly disappear.

## Features

| Feature | What it does | Status |
|---|---|---|
| Session sync | Claude Code transcripts, subagent sessions, and auto-memory follow you between machines | ✅ Working |
| Full environment sync | Plans, tasks, command history, custom agents, skills, rules, and settings sync too | ✅ Working |
| Multiple accounts | `~/.claude-client1`-style profile folders are auto-detected and synced separately | ✅ Working |
| Plugin sync (opt-in) | Plugins can be large, so they only sync if you switch them on — caches are never synced | ✅ Working |
| Storage: any folder | Pick any folder — iCloud Drive, OneDrive, Dropbox, Google Drive, USB, network share | ✅ Working |
| Storage: your own bucket | Cloudflare R2, Amazon S3, or Azure Blob (paste a container SAS URL) | ✅ Working |
| Client-side encryption | Cloud-stored content is encrypted on your machine with [age](https://age-encryption.org) before upload | ✅ Working |
| Only changes sync | After the first sync, only new or changed files are transferred | ✅ Working |
| Nothing is ever lost | Sync never deletes anything; conflicting files are backed up, deleted sessions never resurrect | ✅ Working |
| Auto-sync | Background sync every 15 minutes, plus launch at login | ✅ Working |
| Setup assistant | Guided first-run: pick storage, test the connection, done | ✅ Working |
| Claude desktop sidebar | Synced sessions appear in the Claude desktop app's session list | 🔜 In progress |
| More tools | Codex, VS Code Copilot Chat, Zed, OpenCode, Copilot CLI | 🔜 Planned |
| Windows build | Same app, same store, mixed Mac + Windows fleets (WSL included) | 🔜 Planned |

## How syncing works with any number of computers

Think of your storage location as a shared archive that every computer reads from and writes to:

1. **Computer A** scans its local sessions, compresses (and for cloud storage, encrypts) them, and uploads anything the archive doesn't have yet.
2. **Computer B** (and C, and D…) does the same — and also downloads anything in the archive that it doesn't have locally.
3. Every session has a globally unique ID, so files from different machines never collide — the archive is simply the union of everything, and every machine converges on the full set.
4. Machine-specific paths are translated automatically: a session recorded under `/Users/anna/dev/app` on a Mac lands in the right place for `C:\Users\anna\dev\app` on a PC.
5. If the same session was changed on two machines, the newer version wins — and the older one is kept next to it as a backup file, so nothing is ever overwritten silently.
6. If a tool deletes old sessions locally (Claude Code cleans up after ~30 days), the archive keeps them forever — and sync won't push deleted sessions back onto a machine that already cleaned them up. The archive doubles as a permanent, searchable history.

Adding a new computer is just: install Code Sync, point it at the same storage (same folder, or same bucket + passphrase), and press Sync. Everything arrives.

## Getting started (development builds)

```sh
git clone https://github.com/JohnKesko/codesync
cd codesync

# run the engine tests
cargo test -p codesync-engine

# run the app
cd app && npm install && npm run tauri dev
```

The app appears in your menu bar. Click it, open the Setup Assistant, choose where your sessions should live, and run your first sync.

## License & contributions

Code Sync is **GPL-3.0** — free to use, modify, and share; derivative distributions must remain open under the GPL.

This project is **open source, but not open contribution**: pull requests are not accepted, so the codebase remains single-author (this keeps dual-licensing possible). Bug reports, feature requests, and *adapter intel* (where tool X stores its sessions on platform Y) are very welcome as issues.

**Commercial licensing** (closed-source embedding or distribution) is available — contact the author.

Copyright © 2026 JohnKesko
