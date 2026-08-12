//! Git-origin project identity mapping.
//!
//! The same project lives at different absolute paths on different machines
//! (`C:\Temp\vibesync` here, `~/Development/7_rust/vibesync` there). `${HOME}`
//! tokenization only unifies home-relative layouts; the stable cross-machine
//! identity is the **normalized git origin URL**. Paths inside a repo with an
//! origin tokenize to `${GIT:<identity>}` (identity's `/` written as `:` so
//! the token stays a single path component), and each machine expands the
//! token to *its own* clone location.
//!
//! Like `${HOME}`, the token is canonical in the store: `tokenize(expand(x))
//! == x`, so every machine produces identical logical keys for the same
//! project and content merges instead of duplicating. A machine that has no
//! clone of the repo cannot expand the token; callers leave such items in the
//! store untouched (transcripts park, sidebar entries ghost-guard away) until
//! the repo appears and a later sync materializes them.
//!
//! Each machine learns `identity -> local repo root` from the cwds of its own
//! Claude sidebar entries and persists them (`git_roots.json` in the app data
//! dir). One root per identity: first mapping wins, replaced only if its path
//! disappears from disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `${GIT:github.com:owner:repo}` — identity with `/` swapped to `:`.
pub const GIT_TOKEN_PREFIX: &str = "${GIT:";
/// `${PROJ:name}` — user-chosen fleet-wide project name, configured per
/// machine ("this local folder is project <name>"). Manual mappings outrank
/// git identities: an explicit mapping is intent, and it covers repos with
/// no remote as well as folders that aren't repos at all.
pub const PROJ_TOKEN_PREFIX: &str = "${PROJ:";

pub fn git_token(identity: &str) -> String {
    format!("{GIT_TOKEN_PREFIX}{}}}", identity.replace('/', ":"))
}

/// Split `${GIT:...}rest` into (identity, rest). None if not a git token.
pub fn parse_git_token(s: &str) -> Option<(String, &str)> {
    let inner = s.strip_prefix(GIT_TOKEN_PREFIX)?;
    let end = inner.find('}')?;
    Some((inner[..end].replace(':', "/"), &inner[end + 1..]))
}

pub fn proj_token(name: &str) -> String {
    format!("{PROJ_TOKEN_PREFIX}{name}}}")
}

/// Split `${PROJ:name}rest` into (name, rest). None if not a project token.
pub fn parse_proj_token(s: &str) -> Option<(&str, &str)> {
    let inner = s.strip_prefix(PROJ_TOKEN_PREFIX)?;
    let end = inner.find('}')?;
    Some((&inner[..end], &inner[end + 1..]))
}

/// Project names live inside tokens and travel across machines: keep them to
/// a safe, unambiguous charset.
pub fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// True if the string still carries a token this machine could not expand
/// (repo not cloned / project not mapped here) — such paths must park, never
/// materialize literally on disk.
pub fn has_unresolved_token(s: &str) -> bool {
    s.contains(GIT_TOKEN_PREFIX) || s.contains(PROJ_TOKEN_PREFIX)
}

/// Normalize a git remote URL to a stable identity: `host/path`, lowercased,
/// credentials/ports/`.git` stripped. Returns None for URLs with no stable
/// host+path shape (e.g. local filesystem remotes).
pub fn normalize_origin(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        // scheme://[user@]host[:port]/path
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?, path)
    } else if let Some((left, path)) = url.split_once(':') {
        // scp-like [user@]host:path — a single letter before ':' is a
        // Windows drive, not a host.
        if left.len() <= 1 || path.starts_with('\\') || path.starts_with('/') {
            return None;
        }
        (left.split_once('@').map(|(_, h)| h).unwrap_or(left), path)
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

/// Walk up from `start` to the enclosing repo root and return
/// `(root, identity)` from its `remote.origin.url`. Follows one level of
/// `.git`-file indirection (worktrees / submodules) via `commondir`.
pub fn discover(start: &Path) -> Option<(PathBuf, String)> {
    let mut dir: Option<PathBuf> = Some(start.to_path_buf());
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        if dotgit.is_dir() {
            return origin_identity(&dotgit.join("config")).map(|id| (d, id));
        }
        if dotgit.is_file() {
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let gd = text.strip_prefix("gitdir:")?.trim();
            let gd = if Path::new(gd).is_absolute() { PathBuf::from(gd) } else { d.join(gd) };
            let common = match std::fs::read_to_string(gd.join("commondir")) {
                Ok(c) => {
                    let c = c.trim();
                    if Path::new(c).is_absolute() { PathBuf::from(c) } else { gd.join(c) }
                }
                Err(_) => gd,
            };
            return origin_identity(&common.join("config")).map(|id| (d, id));
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Minimal INI walk of a git config for `[remote "origin"] url = ...`.
fn origin_identity(config: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.eq_ignore_ascii_case("[remote \"origin\"]");
        } else if in_origin {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    return normalize_origin(v.trim());
                }
            }
        }
    }
    None
}

/// identity -> this machine's repo root (absolute plain path).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GitMap {
    pub roots: BTreeMap<String, String>,
    /// Former roots (the folder was moved/renamed): old transcript dirs keep
    /// the old path baked into their names forever, so these stay recognized
    /// when TOKENIZING — history keeps its canonical `${GIT}` keys instead of
    /// re-keying and splitting into a second project. Never used to expand.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, Vec<String>>,
}

impl GitMap {
    /// Load the map, reporting whether the result can be TRUSTED as this
    /// machine's full picture.
    ///
    /// Returns `(map, trusted)`. `trusted` is false when the file exists but
    /// could not be read or parsed — a torn write, a bad disk, a partial
    /// restore. This distinction matters more than it looks: a transcript's
    /// logical key DEPENDS on this map (`${GIT:id}/...` with a root known,
    /// `${EHOME}-...` without it). Silently falling back to an empty map
    /// re-keys every file on the next scan, which `SyncState::mark_deletions`
    /// then reads as "the user deleted all of them" — and `deleted_locally`
    /// is one-way, so those sessions stop syncing to this machine forever.
    /// Callers must suppress deletion marking when this is false.
    pub fn load(path: &Path) -> (Self, bool) {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // Absent is the normal first-run case, and it IS trustworthy:
            // nothing has been learned yet, so nothing can be lost.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), true),
            Err(e) => {
                crate::dlog::warn(|| {
                    format!("project map: {} unreadable ({e}) — deletion marking suppressed this sync", path.display())
                });
                return (Self::default(), false);
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(m) => (m, true),
            Err(e) => {
                crate::dlog::warn(|| {
                    format!("project map: {} is corrupt ({e}) — deletion marking suppressed this sync", path.display())
                });
                (Self::default(), false)
            }
        }
    }

    /// Atomic write: a torn `git_roots.json` costs far more than the rename
    /// (see [`GitMap::load`]), so never write the live path in place.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Learn the repo containing `cwd`. Returns true if the map changed.
    /// First mapping wins; a stored root replaced only once its path is gone
    /// — and the replaced root is kept as a tokenize-only alias so existing
    /// transcript dirs (named after the old path) keep their canonical keys.
    /// A SECOND living clone of an already-mapped repo also becomes a
    /// tokenize-only alias: its sessions join the repo's one identity, while
    /// expansion keeps targeting the primary clone.
    pub fn learn(&mut self, cwd: &Path) -> bool {
        let Some((root, id)) = discover(cwd) else {
            crate::dlog::debug(|| {
                format!("project map: {} is not in a git repo with an origin", cwd.display())
            });
            return false;
        };
        // Extended-length Windows prefixes come in via tool-recorded cwds;
        // stored verbatim they never prefix-match a stripped input path.
        let root = root.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
        let root = root.strip_prefix("\\\\?\\").unwrap_or(&root).to_string();
        match self.roots.get(&id) {
            Some(existing) if *existing == root => false,
            Some(existing) if Path::new(existing).exists() => {
                let aliases = self.aliases.entry(id.clone()).or_default();
                if aliases.contains(&root) {
                    return false;
                }
                crate::dlog::info(|| {
                    format!("project map: second clone of {id} at {root} (tokenize-only alias)")
                });
                aliases.push(root);
                true
            }
            old => {
                if let Some(old_root) = old.cloned() {
                    let aliases = self.aliases.entry(id.clone()).or_default();
                    if old_root != root && !aliases.contains(&old_root) {
                        aliases.push(old_root);
                    }
                    aliases.retain(|a| *a != root);
                }
                crate::dlog::info(|| format!("project map: learned {id} -> {root}"));
                self.roots.insert(id, root);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants_agree() {
        for url in [
            "git@github.com:JohnKesko/vibesync.git",
            "https://github.com/JohnKesko/vibesync.git",
            "https://user@github.com/JohnKesko/vibesync",
            "ssh://git@github.com:22/JohnKesko/vibesync.git",
            "https://github.com/JohnKesko/vibesync/",
        ] {
            assert_eq!(
                normalize_origin(url).as_deref(),
                Some("github.com/johnkesko/vibesync"),
                "{url}"
            );
        }
    }

    #[test]
    fn normalize_rejects_unstable() {
        assert_eq!(normalize_origin("C:\\repos\\bare.git"), None);
        assert_eq!(normalize_origin("/srv/git/repo.git"), None);
        assert_eq!(normalize_origin(""), None);
    }

    #[test]
    fn rename_keeps_old_root_as_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old-spot");
        std::fs::create_dir_all(old.join(".git")).unwrap();
        std::fs::write(
            old.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n",
        )
        .unwrap();
        let mut map = GitMap::default();
        assert!(map.learn(&old));
        // "Move" the repo: old path disappears, new one appears.
        let new = tmp.path().join("new-spot");
        std::fs::rename(&old, &new).unwrap();
        assert!(map.learn(&new));
        let id = "github.com/owner/repo";
        assert_eq!(map.roots.get(id).unwrap(), &new.to_string_lossy().to_string());
        assert_eq!(map.aliases.get(id).unwrap(), &vec![old.to_string_lossy().to_string()]);
    }

    #[test]
    fn token_roundtrip() {
        let t = git_token("github.com/johnkesko/vibesync");
        assert_eq!(t, "${GIT:github.com:johnkesko:vibesync}");
        let with_tail = format!("{t}/app/src");
        let (id, rest) = parse_git_token(&with_tail).unwrap();
        assert_eq!(id, "github.com/johnkesko/vibesync");
        assert_eq!(rest, "/app/src");
    }

    #[test]
    fn discover_and_learn() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:Owner/Proj.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .unwrap();
        let deep = repo.join("app").join("src");
        std::fs::create_dir_all(&deep).unwrap();

        let (root, id) = discover(&deep).unwrap();
        assert_eq!(root, repo);
        assert_eq!(id, "github.com/owner/proj");

        let mut map = GitMap::default();
        assert!(map.learn(&deep));
        assert!(!map.learn(&deep)); // already known, root still exists
        assert_eq!(map.roots["github.com/owner/proj"], repo.to_string_lossy().trim_end_matches(['/', '\\']));
    }

    #[test]
    fn second_clone_becomes_tokenize_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let r = tmp.path().join(name);
            std::fs::create_dir_all(r.join(".git")).unwrap();
            std::fs::write(
                r.join(".git/config"),
                "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n",
            )
            .unwrap();
            r
        };
        let first = mk("first-clone");
        let second = mk("second-clone");
        let mut map = GitMap::default();
        assert!(map.learn(&first));
        assert!(map.learn(&second)); // living primary stays, second aliases
        let id = "github.com/o/r";
        assert_eq!(map.roots[id], first.to_string_lossy().to_string());
        assert_eq!(map.aliases[id], vec![second.to_string_lossy().to_string()]);
        assert!(!map.learn(&second)); // idempotent

        // Both clones tokenize to the one identity; expansion targets the
        // primary clone only.
        let t = crate::tokenizer::Tokenizer::with_case_sensitivity(
            &tmp.path().to_string_lossy(),
            false,
        )
        .with_gitmap(&map);
        assert_eq!(t.tokenize_plain(&second.join("x").to_string_lossy()), "${GIT:github.com:o:r}/x");
        // Component-wise: expansion keeps the token tail's separator style.
        assert_eq!(
            std::path::Path::new(&t.expand_plain("${GIT:github.com:o:r}/x")),
            first.join("x").as_path()
        );
    }

    #[test]
    fn absent_map_is_trusted_corrupt_map_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("git_roots.json");

        // First run: nothing learned yet, so an empty map IS the truth.
        let (map, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert!(map.roots.is_empty());

        let mut m = GitMap::default();
        m.roots.insert("github.com/o/r".into(), "/repos/r".into());
        m.save(&path).unwrap();
        let (loaded, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert_eq!(loaded.roots["github.com/o/r"], "/repos/r");

        // A torn write must NOT read as "this machine knows no repos" — that
        // silently re-keys every ${GIT} path and reads as a mass deletion.
        std::fs::write(&path, b"{\"roots\":{\"github.com/o/r\"").unwrap();
        let (map, trusted) = GitMap::load(&path);
        assert!(!trusted);
        assert!(map.roots.is_empty());
    }

    #[test]
    fn save_leaves_no_partial_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("git_roots.json");
        let mut m = GitMap::default();
        m.roots.insert("github.com/o/r".into(), "/repos/r".into());
        m.save(&path).unwrap();
        m.roots.insert("github.com/o/r2".into(), "/repos/r2".into());
        m.save(&path).unwrap();
        // The temp sibling must not survive a successful save.
        assert!(!path.with_extension("tmp").exists());
        let (loaded, trusted) = GitMap::load(&path);
        assert!(trusted);
        assert_eq!(loaded.roots.len(), 2);
    }

    #[test]
    fn no_repo_learns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut map = GitMap::default();
        assert!(!map.learn(tmp.path()));
        assert!(map.roots.is_empty());
    }
}
