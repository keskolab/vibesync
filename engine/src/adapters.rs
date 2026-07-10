//! Tool adapters: where each AI coding tool keeps its session data.
//!
//! v1 ships Claude Code; Codex, OpenCode, and VS Code Copilot Chat follow.
//! Paths are home-relative and per-OS where they differ. Adapters whose
//! storage needs more than path globs (Claude's desktop registry, VS Code's
//! workspace-hash dirs) add dedicated logic in later milestones — this
//! descriptor covers the plain file-sync part.

use std::path::Path;

use anyhow::Result;

use crate::scanner::{scan_root, FileEntry};
use crate::tokenizer::Tokenizer;

#[derive(Debug, Clone, Copy)]
pub struct RootSpec {
    /// Home-relative root, `/`-separated (joined per-OS at runtime).
    pub home_rel: &'static str,
    /// First component of the logical path for files under this root.
    pub logical_prefix: &'static str,
    /// File extensions to include (case-insensitive).
    pub exts: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    pub roots: &'static [RootSpec],
}

pub const CLAUDE_CODE: Adapter = Adapter {
    id: "claude-code",
    name: "Claude Code",
    roots: &[
        RootSpec {
            home_rel: ".claude/projects",
            logical_prefix: "projects",
            exts: &["jsonl"],
        },
        RootSpec {
            home_rel: ".claude/plans",
            logical_prefix: "plans",
            exts: &["md"],
        },
    ],
};

impl Adapter {
    /// True if any of the adapter's roots exist under `home`.
    pub fn detect(&self, home: &Path) -> bool {
        self.roots.iter().any(|r| join_home(home, r.home_rel).exists())
    }

    pub fn scan(&self, home: &Path, tok: &Tokenizer) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        for root in self.roots {
            let abs = join_home(home, root.home_rel);
            out.extend(scan_root(&abs, root.logical_prefix, tok, root.exts)?);
        }
        Ok(out)
    }

    /// Map a logical path back to an absolute path on this machine, expanding
    /// encoded-home components. Returns None if no root claims the prefix.
    pub fn resolve(&self, logical: &str, home: &Path, tok: &Tokenizer) -> Option<std::path::PathBuf> {
        for root in self.roots {
            let prefix = format!("{}/", root.logical_prefix);
            if let Some(rest) = logical.strip_prefix(&prefix) {
                let mut abs = join_home(home, root.home_rel);
                for comp in rest.split('/') {
                    abs.push(tok.expand_encoded(comp));
                }
                return Some(abs);
            }
        }
        None
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DetectedTool {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
}

pub fn all() -> &'static [&'static Adapter] {
    &[&CLAUDE_CODE]
}

/// Probe this machine's home directory for every known tool.
pub fn detect_all() -> Vec<DetectedTool> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    all()
        .iter()
        .map(|a| DetectedTool { id: a.id, name: a.name, installed: a.detect(&home) })
        .collect()
}

fn join_home(home: &Path, rel: &str) -> std::path::PathBuf {
    let mut p = home.to_path_buf();
    for comp in rel.split('/') {
        p.push(comp);
    }
    p
}
