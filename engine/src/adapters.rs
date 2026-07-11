//! Tool adapters: where each AI coding tool keeps its session data.

use std::path::Path;

use anyhow::Result;

use crate::scanner::{scan_root, FileEntry};
use crate::tokenizer::Tokenizer;

#[derive(Debug, Clone, Copy)]
pub struct RootSpec {
    /// Home-relative root, `/`-separated. May point at a single file.
    pub home_rel: &'static str,
    /// First component of the logical path for files under this root.
    pub logical_prefix: &'static str,
    /// File extensions to include (case-insensitive). Empty = all files.
    pub exts: &'static [&'static str],
    /// Directory names skipped anywhere under this root (e.g. plugin caches).
    pub exclude_dirs: &'static [&'static str],
    /// True when `home_rel` names a single file rather than a directory.
    pub is_file: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    pub roots: &'static [RootSpec],
    /// Opt-in roots (potentially large); synced only when the user enables
    /// them — e.g. Claude plugins.
    pub optional_roots: &'static [RootSpec],
}

const fn root(home_rel: &'static str, logical_prefix: &'static str, exts: &'static [&'static str]) -> RootSpec {
    RootSpec { home_rel, logical_prefix, exts, exclude_dirs: &[], is_file: false }
}

const fn file_root(home_rel: &'static str, logical_prefix: &'static str) -> RootSpec {
    RootSpec { home_rel, logical_prefix, exts: &[], exclude_dirs: &[], is_file: true }
}

pub const CLAUDE_CODE: Adapter = Adapter {
    id: "claude-code",
    name: "Claude Code",
    roots: &[
        // Sessions, subagent transcripts, and auto-memory.
        root(".claude/projects", "projects", &["jsonl", "md"]),
        root(".claude/plans", "plans", &["md"]),
        root(".claude/tasks", "tasks", &[]),
        root(".claude/agents", "agents", &[]),
        root(".claude/skills", "skills", &[]),
        root(".claude/rules", "rules", &[]),
        file_root(".claude/history.jsonl", "meta"),
        file_root(".claude/settings.json", "meta"),
        file_root(".claude/settings.local.json", "meta"),
        file_root(".claude/CLAUDE.md", "meta"),
    ],
    optional_roots: &[
        // Plugins can be huge; never synced by default. The cache is
        // re-downloadable and excluded even when the user opts in.
        RootSpec {
            home_rel: ".claude/plugins",
            logical_prefix: "plugins",
            exts: &[],
            exclude_dirs: &["cache"],
            is_file: false,
        },
    ],
};

impl Adapter {
    /// True if any of the adapter's roots exist under `home`.
    pub fn detect(&self, home: &Path) -> bool {
        self.roots.iter().any(|r| join_home(home, r.home_rel).exists())
    }

    fn active_roots(&self, include_optional: bool) -> impl Iterator<Item = &'static RootSpec> {
        self.roots
            .iter()
            .chain(if include_optional { self.optional_roots.iter() } else { [].iter() })
    }

    pub fn scan(&self, home: &Path, tok: &Tokenizer, include_optional: bool) -> Result<Vec<FileEntry>> {
        self.scan_dir(home, ".claude", tok, include_optional)
    }

    /// Scan with the tool's default config dir replaced by `dir` (multi-account
    /// setups: `.claude-client1`, ...). Non-default dirs are namespaced under
    /// `profiles/<dir>/` in the store so accounts never collide.
    pub fn scan_dir(
        &self,
        home: &Path,
        dir: &str,
        tok: &Tokenizer,
        include_optional: bool,
    ) -> Result<Vec<FileEntry>> {
        // Everything belongs to the claude/ tool namespace; extra config
        // dirs (multi-account) nest under claude/profiles/<dir>/.
        let ns = if dir == ".claude" {
            "claude/".to_string()
        } else {
            format!("claude/profiles/{dir}/")
        };
        let mut out = Vec::new();
        for root in self.active_roots(include_optional) {
            let rel = root.home_rel.replacen(".claude", dir, 1);
            let abs = join_home(home, &rel);
            let mut entries = scan_root(&abs, root.logical_prefix, tok, root.exts, root.exclude_dirs)?;
            for e in &mut entries {
                e.logical = format!("{ns}{}", e.logical);
            }
            out.extend(entries);
        }
        Ok(out)
    }

    /// Enumerate config dirs under `home`: `.claude` plus any `.claude-*`
    /// profile dir that contains a `projects/` folder.
    pub fn detect_config_dirs(home: &Path) -> Vec<String> {
        let mut dirs = vec![".claude".to_string()];
        if let Ok(read) = std::fs::read_dir(home) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".claude-") && entry.path().join("projects").is_dir() {
                    dirs.push(name);
                }
            }
        }
        dirs.sort();
        dirs
    }

    /// Logical prefixes to consider for deletion-marking after a scan.
    pub fn logical_prefixes(&self, include_optional: bool) -> Vec<&'static str> {
        let mut v: Vec<&'static str> =
            self.active_roots(include_optional).map(|r| r.logical_prefix).collect();
        v.dedup();
        v
    }

    /// Map a logical path back to an absolute path on this machine, expanding
    /// encoded-home components. Returns None if no root claims the prefix.
    pub fn resolve(
        &self,
        logical: &str,
        home: &Path,
        tok: &Tokenizer,
        include_optional: bool,
    ) -> Option<std::path::PathBuf> {
        self.resolve_dir(logical, home, ".claude", tok, include_optional)
    }

    pub fn resolve_dir(
        &self,
        logical: &str,
        home: &Path,
        dir: &str,
        tok: &Tokenizer,
        include_optional: bool,
    ) -> Option<std::path::PathBuf> {
        let logical = logical.strip_prefix("claude/")?;
        let logical = if dir == ".claude" {
            if logical.starts_with("profiles/") {
                return None; // profile namespace belongs to a non-default dir
            }
            logical
        } else {
            logical.strip_prefix(&format!("profiles/{dir}/"))?
        };
        for root in self.active_roots(include_optional) {
            let prefix = format!("{}/", root.logical_prefix);
            let Some(rest) = logical.strip_prefix(&prefix) else { continue };
            let abs = join_home(home, &root.home_rel.replacen(".claude", dir, 1));
            if root.is_file {
                // Logical is "<prefix>/<filename>"; must match this root's file.
                let fname = root.home_rel.rsplit('/').next().unwrap_or_default();
                if rest == fname {
                    return Some(abs);
                }
                continue;
            }
            let mut abs = abs;
            for comp in rest.split('/') {
                abs.push(tok.expand_encoded(comp));
            }
            return Some(abs);
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
