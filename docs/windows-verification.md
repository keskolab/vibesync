# Windows live verification — all adapters

**Machine:** `WPF5AAKS5` (Windows 11 Enterprise, user `you`) — first full Windows
fleet test. **Date:** 2026-07-11. **Store:** Cloudflare R2 (EU), 8 343 objects at time of
test. **Fleet peers:** `Andreas-M1-Pro.local`, `Andreass-M4-Mac-mini.local`.

Verification method: every claim below was observed live — adapter `detect()`/counts run
through the engine itself (scratch harness linking `vibesync-engine`), the real R2 store
listed with per-object source attribution, and on-disk / in-app artifacts inspected.
No synthetic data was written into any tool's own stores.

## Environment discoveries that shaped everything

1. **Claude Desktop on Windows is an MSIX/Store package** (`Claude_pzs8sxrjxfjjc`).
   Windows virtualizes `%APPDATA%` for it: the sidebar registry physically lives at
   `%LOCALAPPDATA%\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code-sessions\`,
   and the `%APPDATA%\Claude\...` path is only visible from *inside* the package context.
   Unpackaged processes (VibeSync) see an empty real Roaming — `exists() == false` for a
   path that "obviously exists" when probed from a Claude Code shell.
2. **Codex Desktop is also MSIX** (`OpenAI.Codex_2p2nqsd0c76g0`, Start-menu name
   "ChatGPT"). `~/.codex` is under `USERPROFILE`, which MSIX does **not** virtualize, so
   file-layer sync is unaffected — but the app's session list is **db-first** (see Codex).
3. `~/.claude`, `~/.codex`, `~/.copilot`, `~/.agents` (USERPROFILE paths) are shared
   between packaged and unpackaged worlds. Only AppData known folders get virtualized.

## Per-adapter verdicts

| Adapter | Detection | Paths | Round-trip | In-app visibility |
|---|---|---|---|---|
| Claude Code | PASS | PASS (after fix) | PASS (83 entries applied) | PASS pending app restart |
| VS Code Copilot | PASS | PASS | PASS (live cross-machine apply) | PASS (index merge verified) |
| Codex | PASS | PASS | PASS (files + index union) | **FAIL — Windows app is db-first** |
| Copilot CLI | PASS | PASS | PASS (push); pull untestable | **Likely FAIL — picker is db-indexed** |
| Zed | PASS | PASS | **Blocked — push was never wired (fixed)** | Blocked (no threads fleet-wide) |
| OpenCode | PASS | PASS | PASS (files) | Known limitation (db-first, by design) |
| Shared skills | PASS | PASS | PASS | n/a |

### Claude Code — PASS (two fixes landed)

- **Root causes found:** (a) `registry_dir()` was macOS-only and silently no-opped
  elsewhere; (b) even pointed at `%APPDATA%\Claude\...`, the path doesn't exist for
  unpackaged processes because the Store build virtualizes it (discovery #1).
- **Fix:** `registry_dir()` now probes `dirs::config_dir()/Claude/claude-code-sessions`
  and, on Windows, `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\...`.
  Registry failures now write `registry_last_error.txt` instead of silently mapping to
  zeros (that silence cost most of a debugging session).
- **Evidence:** 83 Mac entries applied into the package store (85 files = 2 native + 83
  applied, all 83 tracked in `applied_registry.json`, natives backed up first). Spot-check:
  single-line JSON, integer epoch-ms timestamps, backslash-normalized `cwd`
  (`C:\Users\you\Development\7_rust\claude-sync`), transcript resolves on disk.
  All 246 transcripts + 5 plans present under `~/.claude/projects`. The app's session
  service caches its scan — entries appear after a Claude Desktop restart.
- Cross-platform path shape is now normalized on apply (`normalize_separators`):
  Windows-style paths get `\`, POSIX paths get `/`, covering both sync directions.

### VS Code Copilot — PASS, full four-layer live proof

- Detection: `installed=true`, 194 chat files across 39 workspaces (engine
  `light_counts()`), read from the real `%APPDATA%\Code\User\workspaceStorage`.
- Store: 954 `vscode/ws` objects (516 Mac-origin). 72 distinct Mac workspaces, 49 local,
  zero folder overlap → Mac sessions correctly **parked**, none misplaced.
- **Live cross-machine test:** opened VS Code on `~/.agents/skills` (a folder that also
  exists as a Mac workspace). Next sync applied the M4-origin session
  `668d8cd4-…jsonl` (1 591 B, title "Thinking Effort") + its `chatEditingSessions`
  sidecar into the new workspace hash dir `787f5c90…`, and merged it into
  `state.vscdb → chat.ChatSessionStore.index` — the exact key the history panel lists
  from. Pixel-level panel check pended a locked workstation, but the mechanism (index
  entry) is the one validated on macOS Gate A.
- Fixed a pre-existing Windows-only test failure: `cross_machine_workspace_mapping`
  built `file://` URIs from raw backslash tempdir paths, producing invalid JSON escapes.

### Codex — round-trip PASS; Windows Desktop UI FAIL (db-first)

- Detection PASS (`~/.codex` with real install content). Paths PASS (`~/.codex/sessions`,
  date-partitioned rollout files — no tokenization needed).
- Round-trip PASS: all 4 M4-origin session files exist locally; this machine's 1 local
  session pushed; `session_index.jsonl` is the correct union of both machines' indexes
  (2 Mac + 1 local entries) and this machine published `codex/index/WPF5AAKS5.jsonl`.
- **In-app FAIL:** the Windows Codex app (0.144.0-alpha.4) keeps its session list in
  `~/.codex/state_5.sqlite` (`threads` table: title/preview/recency columns, 40 sqlx
  migrations). It contains only the locally-created thread; the app did not read
  `session_index.jsonl` on launch (NTFS last-access unchanged). The macOS-validated
  index-drives-the-list assumption does not hold on this Windows build. Same class as
  OpenCode: needs a db merge, pending a live injection test. **Do not write
  `state_5.sqlite` until then.**

### Copilot CLI — push PASS; pull/UI untestable here, picker likely db-indexed

- Detection PASS (`~/.copilot` with config/logs). Paths PASS. 114 objects pushed from
  this machine (all 33 `session-state/` dirs).
- The store contains **zero Mac-origin copilot sessions** — the Macs have never pushed
  any — so "resume a Mac session" cannot be tested from this side of the fleet.
- **Structural finding:** Windows CLI (winget build) has `~/.copilot/session-store.db`
  with a `sessions` table listing only 2 sessions vs 33 on-disk session-state dirs, plus
  an FTS index. The `--resume` picker is almost certainly db-driven → file-layer synced
  foreign sessions would not be listed. Same remediation class as Codex/OpenCode.

### Zed — adapter was dead in the water; push wiring fixed

- Detection PASS: `%LOCALAPPDATA%\Zed\threads\threads.db` found via the existing
  candidate chain (`data_local_dir`). Local db has 0 threads (agent never used here).
- **Bug found & fixed:** `engine::zed::push()` existed but was never called in
  `sync_now` — only `zed::apply()`. No machine in the fleet has ever published a thread
  (`zed/` namespace: 0 objects). With the fix, a post-fix sync ran clean (0 pushed —
  nothing to push — 0 errors). Full round-trip verification requires the fix on a Mac
  that has real threads, or a thread created here first: **pending**.

### OpenCode — PASS (file layer, per design)

- Detection PASS (`~/.local/share/opencode` with db/log/repos). 1 430 store objects
  (690 M1-origin), 1 430 local files — full set. M1-origin records spot-checked on disk.
  UI visibility is a documented limitation of db-first OpenCode builds; per the ground
  rules, `opencode.db` was not touched.

### Shared skills — PASS

- 5 323 `shared/skills` objects (3 sources); M4-origin files spot-checked present under
  `~/.agents/skills` (832 skill dirs local).

## Fixes in this change

- `app/src-tauri/src/syncer.rs`: platform-aware `registry_dir()` incl. MSIX package
  store probing; registry errors surfaced to `registry_last_error.txt`; missing registry
  dir reported instead of silent no-op; **Zed push wired into the sync flow**.
- `engine/src/registry.rs`: `normalize_separators()` applied post-expansion (both
  directions) + tests.
- `engine/src/vscode.rs`: Windows-only test fix (URI backslashes).
- Full engine suite green on Windows: 36 passed, 0 failed.

## Follow-ups

1. **Codex db merge** (`state_5.sqlite` threads table) — Windows/newer builds are
   db-first; index-only merge is invisible there. Needs live injection test first.
2. **Copilot CLI db merge** (`session-store.db`) — same class; also explains why the
   picker may not list old local session-state dirs.
3. **Zed round-trip** — re-test once a machine with real threads runs the fixed build.
4. **Claude sidebar freshness** — the desktop app caches its session scan; entries show
   after restart. Worth checking whether touching a marker file forces a re-scan.
5. **Post-wipe re-push attribution** — after this machine's fresh onboard, its first
   sync re-pushed thousands of pulled objects (store `source` now says WPF5AAKS5 for
   content that originated on Macs). Harmless but noisy; consider content-hash dedupe
   before put.
