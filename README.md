# VibeSync

VibeSync is a small menu bar app that keeps your AI coding sessions in sync across all your computers. Start a Claude Code session on your desktop, open your laptop, and continue where you left off — history, plans, custom agents, skills, and settings included.

Your data lives in a place **you** control: a folder you already sync (iCloud Drive, OneDrive, Dropbox, Google Drive), your own cloud bucket (Cloudflare R2, Amazon S3, Azure), or a USB disk. There is no account, no middleman server — and anything sent to cloud storage is encrypted on your machine first.

<p align="center">
  <img src="assets/img/macOS-1.png" alt="VibeSync menu bar popover with sync status and tool toggles" width="30%" />
  <img src="assets/img/macOS-2.png" alt="Per-tool sync scopes for Claude Code" width="30%" />
  <img src="assets/img/macOS-3.png" alt="Settings with auto-sync, launch at login, and storage location" width="30%" />
</p>

### Why does this exist?

AI coding tools keep sessions on the machine where they happened. Vendors don't sync them, and some tools even delete transcripts after ~30 days. Your conversations, plans, and project memory are valuable — they shouldn't be stuck on one computer or quietly disappear.

### How it works

1. Start with any computer, for example computer A.
2. Computer A looks through your sessions and sends anything new to the storage you chose.
3. Computer B (and C, and D…) does the same — and downloads anything it doesn't have yet.
4. Auto-sync repeats this in the background (every 15 minutes by default, adjustable in Settings).

The rules every sync follows:

- **Nothing is ever lost.** Sync never deletes anything. If the same session changed on two computers, the newer version wins and the older one is kept beside it as a backup file.
- **Nothing comes back from the dead.** When a tool cleans up old sessions (Claude does after ~30 days), your storage keeps them forever — but they're never pushed back onto a computer that already cleaned them up.
- **Works across your computers with zero setup.** A session from `C:\Github\app` on your PC lands in `~/dev/app` on your Mac — same project, different folders, nothing to configure. Git projects are recognized by the repository itself; everything else follows your home folder layout automatically. If a computer doesn't have a project yet, that project's sessions just wait in your storage until it shows up — clone the repo and they're there.
- **Only changes transfer.** After the first sync, routine syncs finish in seconds.
- Adding another computer is just: install VibeSync, point it at the same place, enter the same passphrase, press Sync.

### The passphrase (cloud storage)

- Data in a cloud bucket is locked (encrypted) on your computer **before** it is uploaded. The cloud provider only ever holds files it cannot read.
- Every computer uses the same passphrase — that's how each one can unlock what the others uploaded.
- It's stored in your system's password vault (macOS Keychain / Windows Credential Manager), never leaves your computers, and nobody can recover your data without it — so write it down.

### What syncs

| Tool | What follows you |
|---|---|
| Claude Code | Sessions, subagents, memory, plans, tasks, history, agents, skills, rules, settings — and synced sessions appear in the Claude desktop sidebar. Extra accounts (`~/.claude-work`) sync separately. Plugins only if you opt in |
| VS Code Copilot Chat | Chat history per project, visible in the Chat panel on every machine |
| Codex | Session transcripts; every machine's session list shows all of them |
| OpenCode | Sessions sync at the database level, with a backup taken before the first write |
| Zed | Agent threads (best synced while Zed is closed) |
| Copilot CLI | Standalone `copilot` sessions |
| All tools | Global skills in `~/.agents/skills` ([Agent Skills spec](https://agentskills.io)) |

Works on macOS and Windows, in any mix. Each app has its own on/off switch, per-area scopes, a "+N new" badge when a sync brings something in, and the main window shows when the next auto-sync will run.

### Getting started (development builds)

```sh
git clone https://github.com/JohnKesko/vibesync
cd vibesync

# run the engine tests
cargo test -p vibesync-engine

# run the app
cd app && npm install && npm run tauri dev
```

The app appears in your menu bar. Open the Setup Assistant, choose which tools to sync and where your sessions should live, and run your first sync.

<details>
<summary><b>Every file VibeSync touches</b> — full transparency list</summary>

Nothing outside this list is read or written. `~` is your home folder (`C:\Users\<you>` on Windows). Tools that aren't installed or are switched off aren't touched at all.

**Your AI tools' data:**

| Tool | Files | What VibeSync does |
|---|---|---|
| Claude Code | `~/.claude/projects/` (sessions, transcripts, memory) | Syncs |
| Claude Code | `~/.claude/plans/`, `tasks/`, `agents/`, `skills/`, `rules/` | Syncs |
| Claude Code | `~/.claude/history.jsonl`, `settings.json`, `settings.local.json`, `CLAUDE.md` | Syncs |
| Claude Code | `~/.claude/plugins/` | Only if you opt in; caches never |
| Claude Code | `~/.claude-<profile>/` (extra accounts) | Same as above, per profile |
| Claude Code | Desktop app sidebar registry ¹ | Adds/heals entries for synced sessions; natives backed up first |
| VS Code | `.../Code/User/workspaceStorage/<id>/chatSessions/` ² | Syncs Copilot chats per project |
| VS Code | `.../Code/User/workspaceStorage/<id>/state.vscdb` ² | Updates one key (the chat index) so synced chats show in the panel |
| Codex | `~/.codex/sessions/`, `~/.codex/session_index.jsonl` | Syncs; merges the index so every machine lists all sessions |
| OpenCode | `~/.local/share/opencode/opencode.db` | Merges synced sessions in (insert/update-newer only, never deletes); one-time backup `opencode.db.vibesync-bak` before the first write |
| OpenCode | `~/.local/share/opencode/project/` | Syncs each project's `storage/` records (current OpenCode layout) |
| OpenCode | `~/.local/share/opencode/storage/` | Syncs (legacy records) |
| Zed | `.../Zed/threads/threads.db` ³ | Syncs thread rows, newest wins |
| Copilot CLI | `~/.copilot/session-state/` | Syncs |
| Copilot CLI | `~/.copilot/config.json`, `settings.json`, `logs/` | Never touched — auth/trust stays local |
| All tools | `~/.agents/skills/` | Syncs global skills |

¹ macOS: `~/Library/Application Support/Claude/claude-code-sessions/`; Windows Store app: inside the Claude package under `%LOCALAPPDATA%\Packages\`.
² macOS: `~/Library/Application Support/Code/User/workspaceStorage/`; Windows: `%APPDATA%\Code\User\workspaceStorage\`.
³ macOS: `~/Library/Application Support/Zed/`; Windows: `%APPDATA%\Zed\` or `%LOCALAPPDATA%\Zed\`.

**VibeSync's own files** (macOS: `~/Library/Application Support/com.keskolabs.vibesync/`, Windows: `%APPDATA%\com.keskolabs.vibesync\`):

| File | What it holds |
|---|---|
| `config.json` | Settings and storage location. Credentials/passphrase live in the OS keychain — this file holds a `@keychain` marker instead. Never uploaded |
| `state.json` | Fingerprints of already-synced files, so only changes transfer |
| `git_roots.json` | Which local folder each git project lives in on this machine |
| `new_items.json` | The "+N new" counts shown on each app's card |
| `applied_registry.json`, `registry-backup/` | Sidebar entries VibeSync added, and backups of the originals |
| `store_list_cache.json` | Cloud listing cache so routine syncs make a handful of requests instead of thousands |
| `debug.log` | Only when the Settings toggle is on: per-sync phase timings for troubleshooting |
| `hash_cache.json`, `ghost_cache.json` | Speed caches: file fingerprints across app launches; known-stale sidebar entries so they aren't re-downloaded every sync |

**Inside your storage** (all under `v1/files/`, each file with a small `.meta` sidecar; encrypted before upload on cloud backends): `claude/`, `vscode/ws/`, `codex/`, `opencode/`, `zed/threads/`, `copilot/session-state/`, `shared/skills/`.

</details>

### License

VibeSync is **GPL-3.0** — free to use, modify, and share; derivative distributions must remain open under the GPL.

This project is **open source, but not open contribution**: pull requests are not accepted, so the codebase remains single-author (this keeps dual-licensing possible). Bug reports, feature requests, and *adapter intel* (where tool X stores its sessions on platform Y) are very welcome as issues.

**Commercial licensing** (closed-source embedding or distribution) is available — contact the author.

Copyright © 2026 JohnKesko
