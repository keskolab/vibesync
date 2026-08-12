//! Claude Code presence detection.
//!
//! `~/.claude` existing is NOT proof of an install. VibeSync writes into that
//! tree whenever it applies a sync (`projects/`, `plans/`, `agents/`, ...), so
//! probing any of those paths reports a machine that merely *received* a sync
//! as a Claude Code host it never was — and the syncer then starts pushing
//! from it. Every other adapter already guards this ("only sync residue" in
//! codex/copilot/opencode); this module is the same guard for Claude Code.
//!
//! Detection looks for artifacts only a real Claude Code run leaves behind:
//! - `~/.claude.json` — the CLI's own config, written on first run and never
//!   synced (it isn't one of the adapter's roots).
//! - any entry under `~/.claude` that is not one of the adapter's synced
//!   roots — `sessions/`, `shell-snapshots/`, `session-env/`, `statsig/`,
//!   `backups/`, `.last-cleanup`, ...
//!
//! The synced set is derived from [`CLAUDE_CODE`] rather than hardcoded, so
//! adding a root to the adapter can't silently turn it into a false install
//! signal here.

use std::path::Path;

use crate::adapters::{RootSpec, CLAUDE_CODE};

/// Top-level names under `~/.claude` that VibeSync itself creates when it
/// applies a sync. Presence of these proves nothing about an install.
fn synced_names() -> impl Iterator<Item = &'static str> {
    let roots: &'static [RootSpec] = CLAUDE_CODE.roots;
    let optional: &'static [RootSpec] = CLAUDE_CODE.optional_roots;
    roots
        .iter()
        .chain(optional.iter())
        .filter_map(|r| r.home_rel.strip_prefix(".claude/"))
        .map(|rest| rest.split('/').next().unwrap_or(rest))
}

/// True when `name` (a direct child of `~/.claude`) is something VibeSync
/// wrote: a synced root, or the atomic-write/backup residue of one.
fn is_sync_residue(name: &str) -> bool {
    if name.ends_with(".vibesync-bak") || name.ends_with(".vibesync-tmp") {
        return true;
    }
    synced_names().any(|s| s.eq_ignore_ascii_case(name))
}

/// Is Claude Code actually installed on this machine?
pub fn detect(home: &Path) -> bool {
    // The CLI's own config: first-run marker, never synced. Strongest signal,
    // and it appears before any session exists.
    if home.join(".claude.json").is_file() {
        crate::dlog::debug(|| "detect claude-code: installed (~/.claude.json)".to_string());
        return true;
    }
    let dir = home.join(".claude");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        crate::dlog::debug(|| {
            format!("detect claude-code: NOT installed ({} missing)", dir.display())
        });
        return false;
    };
    let found = rd
        .flatten()
        .any(|e| !is_sync_residue(&e.file_name().to_string_lossy()));
    crate::dlog::debug(|| {
        format!(
            "detect claude-code: {} ({})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            dir.display()
        )
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn bare_home_is_not_installed() {
        assert!(!detect(home().path()));
    }

    #[test]
    fn synced_transcripts_alone_are_not_an_install() {
        // The exact false positive: this machine never had Claude Code, it
        // only received a sync, which creates ~/.claude/projects/.
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude/projects/proj")).unwrap();
        std::fs::write(h.path().join(".claude/projects/proj/s.jsonl"), b"{}").unwrap();
        std::fs::create_dir_all(h.path().join(".claude/agents")).unwrap();
        std::fs::write(h.path().join(".claude/CLAUDE.md"), b"# hi").unwrap();
        std::fs::write(h.path().join(".claude/settings.json.vibesync-bak"), b"{}").unwrap();
        assert!(!detect(h.path()));
    }

    #[test]
    fn config_file_alone_is_an_install() {
        // Claude Code installed and run, but no session started yet — the
        // window the old `.claude/projects` probe reported as "not installed".
        let h = home();
        std::fs::write(h.path().join(".claude.json"), b"{}").unwrap();
        assert!(detect(h.path()));
    }

    #[test]
    fn non_synced_entry_is_an_install() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude/sessions")).unwrap();
        assert!(detect(h.path()));
    }

    #[test]
    fn real_install_alongside_synced_content() {
        let h = home();
        std::fs::create_dir_all(h.path().join(".claude/projects")).unwrap();
        std::fs::create_dir_all(h.path().join(".claude/shell-snapshots")).unwrap();
        assert!(detect(h.path()));
    }
}
