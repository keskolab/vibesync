# VibeSync

VibeSync is a small menu bar app that keeps your AI coding sessions in sync across all your computers. Start a Claude Code session on your desktop, open your laptop, and continue where you left off — including your session history, plans, custom agents, and settings.

Your data is stored in a place **you** control: a folder you already sync (iCloud Drive, OneDrive, Dropbox, Google Drive), your own cloud bucket (Cloudflare R2, Amazon S3, Azure), or just a USB disk. Nothing goes through anyone else's servers, and anything sent to cloud storage is encrypted on your machine first.

<p align="center">
  <img src="assets/img/macOS-1.png" alt="VibeSync menu bar popover with sync status and tool toggles" width="30%" />
  <img src="assets/img/macOS-2.png" alt="Per-tool sync scopes for Claude Code" width="30%" />
  <img src="assets/img/macOS-3.png" alt="Settings with auto-sync, launch at login, and storage location" width="30%" />
</p>
<p align="center"><em>macOS — Windows screenshots coming with the Windows build.</em></p>

### Step-by-step:

1. Start with any computer, for example computer A.
2. Computer A looks through all your sessions and sends anything new to the selected storage you have chosen.
3. Computer B (and C, and D…) does the same — and also downloads anything it doesn't have yet.
4. Computer A now recognizes Computer B + C + D... have uploaded new things, so it will download the new things (every 15 min if you setup ```Settings -> AutoSync```).
5. Done.

#### Why does this exist?

AI coding tools keep their sessions on the machine where they happened. Vendors don't sync them, and some tools even delete transcripts after about 30 days. Your conversations, plans, and project memory are valuable — they shouldn't be stuck on one computer or quietly disappear.

#### How sync works:

- Works with any amount of computers
- You pick one place to keep your data — a folder you already sync (icloud, dropbox, google drive etc) or a cloud bucket you own (S3, R2, Azore Blob Storage etc).  
- Every computer running VibeSync talks to that same place: each one sends whatever is new on its side and sync back whatever it's missing. 
- The computers never talk to each other directly and there is no account or server in the middle by this app.

#### Passphrase (R2, S3, Azure only):
- When your data lives in a cloud bucket, it is locked (encrypted) on your computer ***before*** it is uploaded.   
- The passphrase you choose during setup is the private key to that data.  
- All your computers must use the same passphrase — that is how each one can unlock what your other computers have uploaded. 
- The cloud provider only holds encrypted files it cannot read. 
- The passphrase never leaves your computers and this app never see it — which also means nobody can recover your data if you lose it, so write it down if you need to. 

#### Details:

- Every session has its own unique ID, so files from different computers can never overwrite each other. The storage simply ends up holding everything and every computer catches up to that.

- File paths are fixed up for each computer: a session saved under `/Users/anna/dev/app` on a macOS shows up in the right place for `C:\Users\anna\dev\app` on a PC.

- If the same session changed on two computers, the newer version wins — and the older one is kept right next to it as a backup file. Nothing is ever thrown away silently.

- When a tool cleans up old sessions (Claude Code deletes them after about 30 days), your storage still keeps them forever — and VibeSync is smart enough not to push them back onto a computer that already cleaned them up. Your storage becomes a permanent history.

- **About "Session not found on disk" in the Claude app:** Claude sometimes keeps a session in its sidebar even after it has auto-deleted the conversation behind it — clicking one shows exactly that message. This is Claude's own behavior, not something VibeSync causes. VibeSync never syncs these empty leftovers, and it automatically removes any that it created itself on earlier syncs. Ones that Claude created you can clean up with the Archive or Delete buttons the app offers — the conversations themselves are still safe in your VibeSync storage.

- Adding another computer is just: install VibeSync, point it at the same place (and enter the same passphrase if you use cloud storage) and press Sync.

#### Getting started (development builds)

```sh
git clone https://github.com/JohnKesko/vibesync
cd vibesync

# run the engine tests
cargo test -p vibesync-engine

# run the app
cd app && npm install && npm run tauri dev
```

The app appears in your menu bar. Click it, open the Setup Assistant, choose where your sessions should live, and run your first sync.

#### Features

| Feature | What it does | Status |
|---|---|---|
| Session sync | Claude Code transcripts, subagent sessions, and auto-memory follow you between machines | Done |
| Full environment sync | Plans, tasks, command history, custom agents, skills, rules, and settings sync too | Done |
| Multiple accounts | `~/.claude-client1`-style profile folders are auto-detected and synced separately | Done |
| Plugin sync (opt-in) | Plugins can be large, so they only sync if you switch them on — caches are never synced | Done |
| Storage: any folder | Pick any folder — iCloud Drive, OneDrive, Dropbox, Google Drive, USB, network share | Done |
| Storage: your own bucket | Cloudflare R2, Amazon S3, or Azure Blob (paste a container SAS URL) | Done |
| Client-side encryption | Cloud-stored content is encrypted on your machine with [age](https://age-encryption.org) before upload | Done |
| Only changes sync | After the first sync, only new or changed files are transferred | Done |
| Nothing is ever lost | Sync never deletes anything; conflicting files are backed up, deleted sessions never resurrect | Done |
| Auto-sync | Background sync every 15 minutes, plus launch at login | Done |
| Setup assistant | Guided first-run: pick storage, test the connection, done | Done |
| Claude desktop sidebar | Synced sessions appear in the Claude desktop app's session list | Done |
| VS Code Copilot Chat | Chat history syncs per project folder and appears in the Chat panel | Done |
| Codex | Session transcripts sync; every machine's session list shows all of them | Done |
| Zed | Agent threads sync between machines (best with Zed closed) | Done |
| OpenCode | Session records sync as an archive; in-app visibility being verified | Done |
| Global skills | `~/.agents/skills` ([Agent Skills spec](https://agentskills.io)) shared across all tools and machines | Done |
| Windows build | Same app, same store, mixed Mac + Windows fleets | Done |
| More tools | Copilot CLI, others on request | Planned |

#### Every file VibeSync touches

Transparency matters when a tool reads your AI sessions. This is the complete list — nothing else is read or written. `~` is your home folder (`C:\Users\<you>` on Windows).

**Your AI tools' data (only for tools that are installed and switched on):**

| Tool | Files | What VibeSync does |
|---|---|---|
| Claude Code | `~/.claude/projects/` (sessions, transcripts, memory) | Syncs |
| Claude Code | `~/.claude/plans/`, `~/.claude/tasks/`, `~/.claude/agents/`, `~/.claude/skills/`, `~/.claude/rules/` | Syncs |
| Claude Code | `~/.claude/history.jsonl`, `~/.claude/settings.json`, `~/.claude/settings.local.json`, `~/.claude/CLAUDE.md` | Syncs |
| Claude Code | `~/.claude/plugins/` | Syncs only if you opt in; caches never |
| Claude Code | `~/.claude-<profile>/` (extra accounts) | Same as above, per profile |
| Claude Code | `~/.claude.json` | Adds/heals sidebar entries for synced sessions (App sidebar toggle) |
| VS Code | `.../Code/User/workspaceStorage/<id>/chatSessions/` ¹ | Syncs Copilot chats per project |
| VS Code | `.../Code/User/workspaceStorage/<id>/state.vscdb` ¹ | Updates one key (the chat index) so synced chats show in the Chat panel |
| Codex | `~/.codex/sessions/` | Syncs |
| Codex | `~/.codex/session_index.jsonl` | Merges so every machine lists all sessions |
| OpenCode | `~/.local/share/opencode/storage/` | Syncs |
| OpenCode | `~/.local/share/opencode/opencode.db` | Read-only, for the stats shown in the app — never written |
| Zed | `.../Zed/threads/threads.db` ² | Syncs thread rows (newest wins) |
| All tools | `~/.agents/skills/` | Syncs global skills ([Agent Skills spec](https://agentskills.io)) |

macOS: `~/Library/Application Support/Code/User/workspaceStorage/`
macOS: `~/Library/Application Support/Zed/`

Windows: `%APPDATA%\Code\User\workspaceStorage\`
Windows: `%APPDATA%\Zed\` or `%LOCALAPPDATA%\Zed\`

**VibeSync files:**
macOS: `~/Library/Application Support/com.keskolabs.vibesync/`
Windows: `%APPDATA%\com.keskolabs.vibesync\`

| File | What  |
|---|---|
| `config.json` | Your settings, storage location, and credentials/passphrase. Stays locally — never uploaded |
| `state.json` | Fingerprints of already-synced files, so only changes transfer |
| `new_items.json` | The unseen "+N new" counts shown on each app's card |
| `applied_registry.json` | Which sidebar entries VibeSync added, so it can clean up only its own |

**Storage Provider:** 
Everything under `v1/files/`, each file paired with a small `.meta` sidecar; encrypted before upload on cloud backends

| Folder | Content |
|---|---|
| `claude/` | Claude Code files, mirroring the folders above (`claude/registry/` holds sidebar entries, `claude/profiles/` extra accounts) |
| `vscode/ws/` | VS Code Copilot chats, one folder per project |
| `codex/sessions/`, `codex/index/` | Codex transcripts and each machine's session index |
| `opencode/storage/` | OpenCode session records |
| `zed/threads/` | Zed agent threads |
| `shared/skills/` | Global skills |

##### License & contributions

VibeSync is **GPL-3.0** — free to use, modify, and share; derivative distributions must remain open under the GPL.

This project is **open source, but not open contribution**: pull requests are not accepted, so the codebase remains single-author (this keeps dual-licensing possible). Bug reports, feature requests, and *adapter intel* (where tool X stores its sessions on platform Y) are very welcome as issues.

**Commercial licensing** (closed-source embedding or distribution) is available — contact the author.

Copyright © 2026 JohnKesko
